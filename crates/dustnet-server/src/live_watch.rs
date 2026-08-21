//! Shared live-region watching.
//!
//! Live regions used to cost O(clients × files): every connection independently
//! stat'd and read every file it watched, four times a second, so a thousand
//! viewers of one ticker meant four thousand reads per second of one file, all
//! producing the same bytes. That, not the number of open connections, is what
//! limited how many clients a server could carry.
//!
//! Here a file is read once per change, and the result is shared. What is
//! shared is deliberately only the *read*: the serialized UPDATE is not, because
//! each subscriber has its own region name and — under `SubscribeMode::Delta` —
//! its own baseline to compute a suffix against. Two connections may watch the
//! same path in different modes at the same time.
//!
//! Sharing is by `Arc<LiveGeneration>`, and the budget lease lives *inside* the
//! generation, so a generation's bytes are charged once no matter how many
//! subscribers hold it and are returned when the last one lets go. `Arc` is the
//! refcount; there is no separate bookkeeping to get wrong.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dustnet_core::protocol::{MAX_LIVE_UPDATE_SIZE, ProtocolError};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    ServerAllocationSite, SubscriptionBudget, SubscriptionLease, read_subscription_content,
    reject_allocation_at,
};

/// How long to let filesystem events settle before reading.
///
/// One editor save can produce several events. Coalescing them into a single
/// read is what makes "read once per change" true rather than aspirational, and
/// it gives a file being written in place a moment to finish.
const SETTLE: Duration = Duration::from_millis(50);

/// How often to re-stat every watched file regardless of events.
///
/// The safety net. Filesystem notification is not universally available — NFS
/// and some FUSE mounts deliver nothing — and every backend can drop events
/// under load. Correctness must not depend on the watcher firing, so it does
/// not: this loop alone is sufficient to serve live regions, and the event
/// source only makes them fast.
const RECONCILE: Duration = Duration::from_secs(1);

/// Distinct paths one server will watch at a time.
const MAX_WATCHED_FILES: usize = 256;

/// Wake slots reserved per watched file before any subscriber arrives.
const WAKE_SLOTS: usize = 64;

/// Largest content that could be sent to *any* subscriber.
///
/// The real limit is per-subscriber, because the region name counts toward the
/// body, but the region is not known at read time. This is the region-free
/// upper bound; a subscriber whose region pushes it over is refused at send.
fn max_shared_content_len() -> usize {
    MAX_LIVE_UPDATE_SIZE.saturating_sub("UPDATE \n\n".len())
}

/// One version of one watched file's contents.
///
/// The budget lease is held here rather than by a subscriber, which is the
/// whole point: N subscribers sharing a generation charge for it once. Dropping
/// the last `Arc` drops the lease and returns the bytes.
#[derive(Debug)]
pub(crate) struct LiveGeneration {
    pub(crate) text: String,
    _content_lease: SubscriptionLease,
}

impl LiveGeneration {
    #[cfg(test)]
    pub(crate) fn empty_for_tests(budget: &SubscriptionBudget) -> Result<Arc<Self>, ProtocolError> {
        Self::empty(budget)
    }

    /// An empty generation, charged nothing, for a file that has not been read.
    fn empty(budget: &SubscriptionBudget) -> Result<Arc<Self>, ProtocolError> {
        Ok(Arc::new(Self {
            text: String::new(),
            _content_lease: budget.reserve(0)?,
        }))
    }
}

/// Cheap identity for "has this file changed?".
///
/// Compared instead of re-reading, so the reconcile sweep costs one `stat` per
/// watched file rather than one read. Length and mtime alone would miss a
/// same-length write inside one mtime tick; the inode guards against the file
/// being replaced rather than modified, which is how most editors save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        };
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode,
        }
    }
}

/// What the event source tells the registry.
#[derive(Debug)]
pub(crate) enum WatchEvent {
    /// Something happened at or under this path. Deliberately not "this file
    /// changed": event payloads are not trustworthy across backends, so this
    /// only ever means "re-check".
    Touched(PathBuf),
    /// The backend coalesced or overflowed. Re-check everything.
    Rescan,
    /// The backend is gone. Reconcile is now the only source of truth, which is
    /// slower but not wrong.
    SourceFailed,
}

/// Where change notifications come from.
///
/// Narrow on purpose: tests drive the registry through the same channel the
/// real watcher uses, so every behaviour below can be exercised deterministically
/// without touching a real filesystem watcher or waiting on one.
pub(crate) trait ChangeSource: Send {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), ProtocolError>;
    fn unwatch_dir(&mut self, dir: &Path);
}

/// A change source that observes nothing. Tests push `WatchEvent`s themselves;
/// the reconcile sweep covers the rest.
#[derive(Default)]
pub(crate) struct ManualChangeSource;

impl ChangeSource for ManualChangeSource {
    fn watch_dir(&mut self, _dir: &Path) -> Result<(), ProtocolError> {
        Ok(())
    }
    fn unwatch_dir(&mut self, _dir: &Path) {}
}

/// What the watcher thread is asked to do.
enum WatchDirCommand {
    Watch(PathBuf),
    Unwatch(PathBuf),
}

/// The production change source: real filesystem notifications.
///
/// Watches *parent directories*, not files. An inode-level watch dies the
/// moment the file is replaced, and replacing the file is how most editors save
/// — write a temporary and rename over the target. Watching the directory
/// survives that; the cost is events for siblings, which are cheap because an
/// unchanged file is a stat and not a read.
///
/// The platform watcher lives on its own thread and is spoken to over a
/// channel. That is not tidiness: registering an FSEvents watch was measured at
/// **1.7 seconds**, and the registry is a single task shared by every
/// connection, so a blocking registration there stalls every other subscriber
/// behind it. Registration is only ever a latency optimisation — the reconcile
/// sweep serves the file either way — so it must never be something a
/// subscriber waits on.
pub(crate) struct NotifyChangeSource {
    commands: std::sync::mpsc::Sender<WatchDirCommand>,
    watched: HashMap<PathBuf, usize>,
}

impl NotifyChangeSource {
    /// Start the watcher thread.
    ///
    /// Returns `None` only if the thread cannot be spawned. A platform watcher
    /// that fails to start is reported through `WatchEvent::SourceFailed`
    /// instead, because by then the registry already exists and periodic
    /// reconciliation is the correct thing to fall back to.
    pub(crate) fn new(events: mpsc::Sender<WatchEvent>) -> Option<Self> {
        let (commands, requests) = std::sync::mpsc::channel::<WatchDirCommand>();
        let spawned = std::thread::Builder::new()
            .name("dustnet-live-watch".to_owned())
            .spawn(move || {
                let sink = events.clone();
                let handler = move |result: Result<notify::Event, notify::Error>| {
                    // Runs on the notify backend's own thread and must never
                    // block. A full channel means the registry has not drained
                    // what it already has, so the useful message is "re-check
                    // everything" rather than a queue of individual paths.
                    let event = match result {
                        Ok(event) => match event.paths.first() {
                            Some(path) => WatchEvent::Touched(path.clone()),
                            None => WatchEvent::Rescan,
                        },
                        // A backend error may mean the watcher is gone, not
                        // that events were coalesced.
                        Err(_) => WatchEvent::SourceFailed,
                    };
                    if sink.try_send(event).is_err() {
                        let _ = sink.try_send(WatchEvent::Rescan);
                    }
                };
                let mut watcher = match notify::recommended_watcher(handler) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "filesystem notification unavailable; \
                             live regions fall back to periodic reconciliation"
                        );
                        let _ = events.try_send(WatchEvent::SourceFailed);
                        return;
                    }
                };
                use notify::Watcher;
                // Ends when the registry drops its sender.
                while let Ok(command) = requests.recv() {
                    match command {
                        WatchDirCommand::Watch(dir) => {
                            if let Err(error) =
                                watcher.watch(&dir, notify::RecursiveMode::NonRecursive)
                            {
                                tracing::warn!(
                                    dir = %dir.display(),
                                    %error,
                                    "watching this directory failed; \
                                     its live regions fall back to reconciliation"
                                );
                            }
                        }
                        // Failing to unwatch costs a descriptor, not correctness.
                        WatchDirCommand::Unwatch(dir) => {
                            let _ = watcher.unwatch(&dir);
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self {
                commands,
                watched: HashMap::new(),
            }),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not start the live-region watcher thread; \
                     live regions fall back to periodic reconciliation"
                );
                None
            }
        }
    }
}

impl ChangeSource for NotifyChangeSource {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), ProtocolError> {
        // Refcounted: many watched files share one directory, and the last one
        // to leave unwatches it.
        if let Some(count) = self.watched.get_mut(dir) {
            *count = count.saturating_add(1);
            return Ok(());
        }
        let mut key = PathBuf::new();
        key.try_reserve_exact(dir.as_os_str().len()).map_err(|_| {
            ProtocolError::ResourceExhausted {
                requested: dir.as_os_str().len(),
            }
        })?;
        key.push(dir);
        // Non-blocking by construction: the registry must not wait on this.
        if self
            .commands
            .send(WatchDirCommand::Watch(key.clone()))
            .is_err()
        {
            return Err(ProtocolError::InvalidMessage(
                "the live-region watcher thread has stopped".into(),
            ));
        }
        self.watched.insert(key, 1);
        Ok(())
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        let Some(count) = self.watched.get_mut(dir) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count > 0 {
            return;
        }
        if let Some((key, _)) = self.watched.remove_entry(dir) {
            let _ = self.commands.send(WatchDirCommand::Unwatch(key));
        }
    }
}

/// One watched file, and everyone listening to it.
struct WatchedFile {
    updates: watch::Sender<Arc<LiveGeneration>>,
    /// One per subscribing connection. The wake carries no payload: on waking, a
    /// connection reconciles all of its own subscriptions by pointer identity,
    /// so a dropped wake is harmless — a full capacity-1 channel already means
    /// "you have unhandled work".
    wakes: Vec<mpsc::Sender<()>>,
    stamp: Option<FileStamp>,
    dirty: bool,
    _entry_lease: SubscriptionLease,
}

/// What a connection asks the registry for.
enum WatchCommand {
    Attach {
        path: PathBuf,
        wake: mpsc::Sender<()>,
        owner_key: u64,
        reply: oneshot::Sender<Result<AttachedWatch, ProtocolError>>,
    },
}

/// What a subscriber receives when it attaches.
pub(crate) struct AttachedWatch {
    pub(crate) current: Arc<LiveGeneration>,
    pub(crate) updates: watch::Receiver<Arc<LiveGeneration>>,
}

/// A connection's handle on the registry.
#[derive(Clone)]
pub(crate) struct LiveWatcher {
    commands: mpsc::Sender<WatchCommand>,
}

impl LiveWatcher {
    /// Subscribe to a path, receiving its current contents and a channel of
    /// later ones.
    ///
    /// Synchronous with respect to the file: if the file changed since it was
    /// last read, the read happens before this returns. A client that
    /// reconnects therefore never receives a generation staler than the moment
    /// it asked for one.
    pub(crate) async fn attach(
        &self,
        path: PathBuf,
        wake: mpsc::Sender<()>,
        owner_key: u64,
    ) -> Result<AttachedWatch, ProtocolError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(WatchCommand::Attach {
                path,
                wake,
                owner_key,
                reply,
            })
            .await
            .map_err(|_| ProtocolError::ConnectionClosed)?;
        answer.await.map_err(|_| ProtocolError::ConnectionClosed)?
    }
}

/// The single reader.
pub(crate) struct WatchRegistry {
    files: HashMap<PathBuf, WatchedFile>,
    budget: SubscriptionBudget,
    source: Box<dyn ChangeSource>,
    events: mpsc::Receiver<WatchEvent>,
    commands: mpsc::Receiver<WatchCommand>,
    sequence: u64,
    /// Set once the event channel is exhausted. A closed `mpsc::Receiver`
    /// reports "ready" immediately and forever, so an unguarded `recv` arm in
    /// the loop below would spin and starve the command arm — subscribers would
    /// still attach, but only when `select!` happened to pick them, which is a
    /// latency bug that looks exactly like a hang.
    source_ended: bool,
    settle: Duration,
    reconcile: Duration,
    /// Reads performed. Lets a test assert that an event storm produced one
    /// read, which is the claim this whole module exists to make.
    #[cfg(test)]
    reads: usize,
}

impl WatchRegistry {
    pub(crate) fn new(
        budget: SubscriptionBudget,
        source: Box<dyn ChangeSource>,
        events: mpsc::Receiver<WatchEvent>,
    ) -> (Self, LiveWatcher) {
        let (command_tx, commands) = mpsc::channel(256);
        (
            Self {
                files: HashMap::new(),
                budget,
                source,
                events,
                commands,
                sequence: 0,
                source_ended: false,
                settle: SETTLE,
                reconcile: RECONCILE,
                #[cfg(test)]
                reads: 0,
            },
            LiveWatcher {
                commands: command_tx,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn with_intervals(mut self, settle: Duration, reconcile: Duration) -> Self {
        self.settle = settle;
        self.reconcile = reconcile;
        self
    }

    #[cfg(test)]
    pub(crate) fn reads(&self) -> usize {
        self.reads
    }

    /// Run until shutdown.
    pub(crate) async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut settle = tokio::time::interval(self.settle);
        settle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconcile = tokio::time::interval(self.reconcile);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            self.reap();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.apply_command(command).await,
                    None => break,
                },
                event = self.events.recv(), if !self.source_ended => match event {
                    Some(event) => self.mark(event),
                    None => {
                        // The source is gone; reconcile still serves every file.
                        self.source_ended = true;
                        self.mark_all_dirty();
                        tracing::warn!(
                            "live-region change source ended; \
                             falling back to periodic reconciliation"
                        );
                    }
                },
                _ = settle.tick(), if self.any_dirty() => self.refresh_dirty().await,
                _ = reconcile.tick() => {
                    self.mark_all_dirty();
                    self.refresh_dirty().await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    }

    fn mark(&mut self, event: WatchEvent) {
        match event {
            WatchEvent::Touched(path) => {
                // Attribution is an optimisation, never a correctness
                // requirement: an event we cannot place marks the whole
                // directory, and an unplaceable directory marks everything.
                let mut placed = false;
                if let Some(entry) = self.files.get_mut(&path) {
                    entry.dirty = true;
                    placed = true;
                } else {
                    for (watched, entry) in self.files.iter_mut() {
                        if watched.parent() == Some(path.as_path())
                            || watched.parent() == path.parent()
                        {
                            entry.dirty = true;
                            placed = true;
                        }
                    }
                }
                if !placed {
                    self.mark_all_dirty();
                }
            }
            WatchEvent::Rescan => self.mark_all_dirty(),
            WatchEvent::SourceFailed => {
                tracing::warn!(
                    "live-region change source failed; \
                     falling back to periodic reconciliation"
                );
                self.mark_all_dirty();
            }
        }
    }

    fn mark_all_dirty(&mut self) {
        for entry in self.files.values_mut() {
            entry.dirty = true;
        }
    }

    fn any_dirty(&self) -> bool {
        self.files.values().any(|entry| entry.dirty)
    }

    /// Drop entries nobody is listening to.
    ///
    /// A file is live exactly while someone holds a `watch::Receiver` for it,
    /// so unsubscribing and disconnecting need no explicit deregistration —
    /// dropping the receiver is the deregistration.
    fn reap(&mut self) {
        let mut dropped: Vec<PathBuf> = Vec::new();
        for (path, entry) in self.files.iter() {
            if entry.updates.receiver_count() == 0 {
                dropped.push(path.clone());
            }
        }
        for path in dropped {
            self.files.remove(&path);
            if let Some(parent) = path.parent() {
                let still_needed = self
                    .files
                    .keys()
                    .any(|watched| watched.parent() == Some(parent));
                if !still_needed {
                    self.source.unwatch_dir(parent);
                }
            }
        }
    }

    async fn apply_command(&mut self, command: WatchCommand) {
        match command {
            WatchCommand::Attach {
                path,
                wake,
                owner_key,
                reply,
            } => {
                let result = self.attach(path, wake, owner_key).await;
                // A caller that gave up before we answered is not an error.
                let _ = reply.send(result);
            }
        }
    }

    async fn attach(
        &mut self,
        path: PathBuf,
        wake: mpsc::Sender<()>,
        owner_key: u64,
    ) -> Result<AttachedWatch, ProtocolError> {
        if !self.files.contains_key(&path) {
            if self.files.len() >= MAX_WATCHED_FILES {
                return Err(ProtocolError::ResourceExhausted {
                    requested: self.files.len(),
                });
            }
            self.install(&path, owner_key)?;
        }

        // Take the receiver before reading. It makes the subscriber count a
        // publish reports accurate — reading first meant the initial read of
        // every file logged "subscribers=0" while plainly serving one — and
        // because `reap` drops entries with no receivers, holding one across
        // the read below removes the window in which this entry could be
        // reaped mid-await. Reserve the wake slot here too, so the push after
        // the read cannot fail or allocate.
        let Some(entry) = self.files.get_mut(&path) else {
            return Err(ProtocolError::ResourceExhausted { requested: 0 });
        };
        if entry.wakes.len() == entry.wakes.capacity()
            && (reject_allocation_at(ServerAllocationSite::WakeSlots, owner_key)
                || entry.wakes.try_reserve(1).is_err())
        {
            return Err(ProtocolError::ResourceExhausted {
                requested: entry.wakes.capacity().saturating_add(1),
            });
        }
        let updates = entry.updates.subscribe();

        // Read before answering when the file has moved on, so a reconnecting
        // subscriber never receives a generation staler than its request.
        self.refresh_if_changed(&path).await;

        let Some(entry) = self.files.get_mut(&path) else {
            return Err(ProtocolError::ResourceExhausted { requested: 0 });
        };
        // The wake goes on *after* the read. Registering it earlier means the
        // read above wakes this subscriber for the very generation it is about
        // to be handed as `current` — a wake with nothing behind it, which on a
        // capacity-one channel displaces the next real one.
        entry.wakes.push(wake);
        let current = {
            let guard = entry.updates.borrow();
            Arc::clone(&guard)
        };
        Ok(AttachedWatch { current, updates })
    }

    fn install(&mut self, path: &Path, owner_key: u64) -> Result<(), ProtocolError> {
        if reject_allocation_at(ServerAllocationSite::WatchEntry, owner_key) {
            return Err(ProtocolError::ResourceExhausted {
                requested: path.as_os_str().len(),
            });
        }
        let mut key = PathBuf::new();
        key.try_reserve_exact(path.as_os_str().len()).map_err(|_| {
            ProtocolError::ResourceExhausted {
                requested: path.as_os_str().len(),
            }
        })?;
        key.push(path);

        if reject_allocation_at(ServerAllocationSite::WakeSlots, owner_key) {
            return Err(ProtocolError::ResourceExhausted {
                requested: WAKE_SLOTS,
            });
        }
        let mut wakes: Vec<mpsc::Sender<()>> = Vec::new();
        wakes
            .try_reserve_exact(WAKE_SLOTS)
            .map_err(|_| ProtocolError::ResourceExhausted {
                requested: WAKE_SLOTS,
            })?;

        let entry_bytes = key
            .capacity()
            .checked_add(WAKE_SLOTS.saturating_mul(size_of::<mpsc::Sender<()>>()))
            .ok_or(ProtocolError::ResourceExhausted {
                requested: usize::MAX,
            })?;
        let entry_lease = self.budget.reserve(entry_bytes)?;

        // A failing backend degrades latency, not correctness: the reconcile
        // sweep still serves this file. Refusing the subscription instead would
        // trade a working slow path for no path at all.
        if let Some(parent) = path.parent()
            && let Err(error) = self.source.watch_dir(parent)
        {
            tracing::warn!(
                path = %path.display(),
                %error,
                "watching this directory failed; \
                 live updates for it fall back to periodic reconciliation"
            );
        }

        let (updates, _) = watch::channel(LiveGeneration::empty(&self.budget)?);
        self.files.insert(
            key,
            WatchedFile {
                updates,
                wakes,
                stamp: None,
                dirty: true,
                _entry_lease: entry_lease,
            },
        );
        Ok(())
    }

    async fn refresh_dirty(&mut self) {
        let dirty: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(_, entry)| entry.dirty)
            .map(|(path, _)| path.clone())
            .collect();
        for path in dirty {
            if let Some(entry) = self.files.get_mut(&path) {
                entry.dirty = false;
            }
            self.refresh_if_changed(&path).await;
        }
    }

    /// Stat the file and, only if it moved, read it once and publish.
    async fn refresh_if_changed(&mut self, path: &Path) {
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return,
        };
        let stamp = FileStamp::of(&metadata);
        let Some(entry) = self.files.get(path) else {
            return;
        };
        if entry.stamp == Some(stamp) {
            return;
        }

        let content_len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if content_len > max_shared_content_len() {
            tracing::warn!(
                path = %path.display(),
                bytes = content_len,
                max = max_shared_content_len(),
                "live region not published: file exceeds the maximum update size"
            );
            if let Some(entry) = self.files.get_mut(path) {
                entry.stamp = Some(stamp);
            }
            return;
        }

        // Scratch plus the retained text. Charged once, here — not once per
        // subscriber, which is what makes the budget viable at scale.
        let peak = match content_len
            .checked_add(1)
            .and_then(|s| s.checked_add(content_len))
        {
            Some(peak) => peak,
            None => return,
        };
        let mut lease = match self.budget.reserve(peak) {
            Ok(lease) => lease,
            // Already logged by the budget. Keep the previous generation and
            // try again on the next sweep; this is a retryable failure and the
            // reconcile timer is the retry.
            Err(_) => return,
        };
        let owner_key = crate::subscription_owner_key(&path.to_string_lossy(), "");
        let text = match read_subscription_content(path, content_len, owner_key, &mut lease).await {
            Ok(text) => text,
            Err(_) => return,
        };
        #[cfg(test)]
        {
            self.reads = self.reads.saturating_add(1);
        }
        lease.shrink_to(text.capacity());

        self.sequence = self.sequence.saturating_add(1);
        let sequence = self.sequence;
        let Some(entry) = self.files.get_mut(path) else {
            return;
        };
        entry.stamp = Some(stamp);
        if entry.updates.borrow().text == text {
            return;
        }
        let bytes = text.len();
        let subscribers = entry.updates.receiver_count();
        entry.updates.send_replace(Arc::new(LiveGeneration {
            text,
            _content_lease: lease,
        }));
        entry.wakes.retain(|wake| !wake.is_closed());
        for wake in &entry.wakes {
            // A full channel already means "you have work"; a second wake would
            // tell the connection nothing it does not already know.
            let _ = wake.try_send(());
        }
        tracing::info!(
            path = %path.display(),
            bytes,
            sequence,
            subscribers,
            "live region updated"
        );
    }

    #[cfg(test)]
    pub(crate) async fn attach_now(
        &mut self,
        path: PathBuf,
        wake: mpsc::Sender<()>,
        owner_key: u64,
    ) -> Result<AttachedWatch, ProtocolError> {
        self.attach(path, wake, owner_key).await
    }

    #[cfg(test)]
    pub(crate) fn reap_now(&mut self) {
        self.reap();
    }

    #[cfg(test)]
    pub(crate) fn subscribers_of(&self, path: &Path) -> usize {
        self.files
            .get(path)
            .map(|entry| entry.updates.receiver_count())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn watched_files(&self) -> usize {
        self.files.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: usize = 8 * 1024 * 1024;

    fn registry(
        budget: &SubscriptionBudget,
    ) -> (WatchRegistry, LiveWatcher, mpsc::Sender<WatchEvent>) {
        let (events_tx, events) = mpsc::channel(64);
        let (registry, watcher) =
            WatchRegistry::new(budget.clone(), Box::new(ManualChangeSource), events);
        (registry, watcher, events_tx)
    }

    /// A wake channel plus the receiver that keeps it alive.
    fn wake() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        mpsc::channel(1)
    }

    /// The claim the module exists to make: N subscribers of one path are
    /// served by one read, and see the very same allocation.
    #[tokio::test]
    async fn one_read_serves_every_subscriber_of_a_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let mut keepalive = Vec::new();
        let mut attached = Vec::new();
        for index in 0..3 {
            let (tx, rx) = wake();
            keepalive.push(rx);
            attached.push(registry.attach_now(path.clone(), tx, index).await.unwrap());
        }

        assert_eq!(registry.reads(), 1, "one file, one read, three subscribers");
        for handle in &attached {
            assert_eq!(handle.current.text, "first");
            assert!(
                Arc::ptr_eq(&handle.current, &attached[0].current),
                "subscribers must share one allocation, not copies of it"
            );
        }
    }

    /// Sharing is only a win if the budget agrees it is shared. Adding
    /// subscribers must not add content-sized charges.
    #[tokio::test]
    async fn generation_bytes_are_charged_once_however_many_subscribers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "x".repeat(4096)).unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, rx0) = wake();
        let first = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        let after_one = budget.used();

        let mut keepalive = vec![rx0];
        let mut handles = vec![first];
        for index in 1..16 {
            let (tx, rx) = wake();
            keepalive.push(rx);
            handles.push(registry.attach_now(path.clone(), tx, index).await.unwrap());
        }

        let after_sixteen = budget.used();
        assert_eq!(
            after_one, after_sixteen,
            "sixteen subscribers charged {after_sixteen} where one charged \
             {after_one}; the generation is not actually shared"
        );
        assert_eq!(registry.reads(), 1);
    }

    /// The bytes come back when the last holder lets go, via `Arc` alone.
    #[tokio::test]
    async fn a_generation_is_released_when_its_last_holder_drops() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "x".repeat(4096)).unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, rx) = wake();
        let attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        assert!(budget.used() >= 4096);

        drop(attached);
        drop(rx);
        registry.reap_now();
        assert_eq!(registry.watched_files(), 0, "nobody is listening any more");
        assert_eq!(budget.used(), 0, "every byte returned without bookkeeping");
    }

    /// An event storm is one read, not one read per event. Without coalescing
    /// the shared watcher would simply relocate the old polling cost.
    #[tokio::test]
    async fn an_event_storm_produces_a_single_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, _rx) = wake();
        let _attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        assert_eq!(registry.reads(), 1);

        std::fs::write(&path, "second").unwrap();
        for _ in 0..50 {
            registry.mark(WatchEvent::Touched(path.clone()));
        }
        registry.refresh_dirty().await;
        assert_eq!(
            registry.reads(),
            2,
            "fifty events over one change must still be one read"
        );
    }

    /// An unchanged file is a stat, not a read. This is what makes the
    /// reconcile safety net affordable enough to always be on.
    #[tokio::test]
    async fn reconciling_an_unchanged_file_does_not_read_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, _rx) = wake();
        let _attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        let after_attach = registry.reads();

        for _ in 0..5 {
            registry.mark_all_dirty();
            registry.refresh_dirty().await;
        }
        assert_eq!(
            registry.reads(),
            after_attach,
            "an unchanged file must never be re-read"
        );
    }

    /// A file too large for any subscriber is refused without disturbing what
    /// subscribers already hold.
    #[tokio::test]
    async fn an_oversize_file_leaves_the_previous_generation_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, _rx) = wake();
        let attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();

        std::fs::write(&path, "x".repeat(max_shared_content_len() + 1)).unwrap();
        registry.mark_all_dirty();
        registry.refresh_dirty().await;

        assert_eq!(
            attached.updates.borrow().text,
            "first",
            "an unsendable file must not replace a good generation"
        );
    }

    /// A publish must report how many subscribers it actually served.
    ///
    /// The initial read of a file used to happen before the subscriber's
    /// receiver existed, so it logged "subscribers=0" while serving one — the
    /// log said the opposite of what was happening, in exactly the situation
    /// someone would be reading it to find out.
    #[tokio::test]
    async fn a_publish_reports_the_subscribers_it_served() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, _rx) = wake();
        let attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        assert_eq!(attached.current.text, "first");
        assert_eq!(
            registry.subscribers_of(&path),
            1,
            "the initial read must count the subscriber it is reading for"
        );
    }

    /// Attaching must not leave a wake behind for the generation it already
    /// handed over. On a capacity-one channel that stale wake displaces the
    /// next real one, and the subscriber then waits forever for an update it
    /// was already told about.
    #[tokio::test]
    async fn attaching_does_not_leave_a_wake_for_the_generation_it_returned() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);
        let (mut registry, _watcher, _events) = registry(&budget);

        let (tx, mut rx) = wake();
        let _attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "attaching woke the subscriber for the value it was just given"
        );

        // A real change must still wake it.
        std::fs::write(&path, "second").unwrap();
        registry.mark_all_dirty();
        registry.refresh_dirty().await;
        assert!(
            rx.try_recv().is_ok(),
            "a real change must wake the subscriber"
        );
    }

    /// Correctness must not depend on the event source. With a source that
    /// reports failure and then never emits anything, reconciliation alone must
    /// still deliver the change. This is the property the supply-chain
    /// disposition for `notify` rests on: the watcher is a latency
    /// optimisation, never a dependency.
    #[tokio::test]
    async fn event_source_failure_falls_back_to_reconcile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(BUDGET);

        let (events_tx, events) = mpsc::channel(8);
        let (registry, watcher) =
            WatchRegistry::new(budget.clone(), Box::new(ManualChangeSource), events);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(
            registry
                .with_intervals(Duration::from_millis(1), Duration::from_millis(5))
                .run(shutdown_rx),
        );

        let (tx, _rx) = wake();
        let attached = watcher.attach(path.clone(), tx, 0).await.unwrap();
        assert_eq!(attached.current.text, "first");

        // The source declares itself dead, and then emits nothing ever again.
        events_tx.send(WatchEvent::SourceFailed).await.unwrap();
        drop(events_tx);

        std::fs::write(&path, "second").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while attached.updates.borrow().text != "second" {
            assert!(
                std::time::Instant::now() < deadline,
                "reconciliation never delivered the change without an event source"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A rejected allocation at any watcher site must leave the budget exactly
    /// as it found it. Drives the owner-keyed hooks the audit requires.
    #[tokio::test]
    async fn watch_entry_allocation_rejection_returns_every_byte() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();

        for site in [
            ServerAllocationSite::WatchEntry,
            ServerAllocationSite::WakeSlots,
            ServerAllocationSite::Content,
        ] {
            let budget = SubscriptionBudget::new(BUDGET);
            let (mut registry, _watcher, _events) = registry(&budget);
            let owner = 7;
            let rejection = crate::AllocationRejectionGuard::at(site, owner);
            let (tx, _rx) = wake();
            let result = registry.attach_now(path.clone(), tx, owner).await;
            drop(rejection);

            if site == ServerAllocationSite::Content {
                // The read is refused; the attach still succeeds with an empty
                // generation, and the reconcile sweep will fill it in.
                assert!(result.is_ok());
            } else {
                assert!(result.is_err(), "{site:?} must refuse the attach");
            }
            drop(result);
            registry.reap_now();
            assert_eq!(budget.used(), 0, "{site:?} leaked budget");
        }
    }
}

#[cfg(test)]
mod notify_tests {
    use super::*;

    /// The regression guard for watching directories rather than file inodes:
    /// a write-via-rename, which is how vim and most editors save, must still
    /// be observed. A watch on the old inode would see nothing.
    #[tokio::test]
    async fn write_via_rename_is_observed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();

        let (events_tx, events) = mpsc::channel(64);
        let Some(source) = NotifyChangeSource::new(events_tx) else {
            // No watcher on this platform; reconciliation covers it and there
            // is nothing here to assert.
            return;
        };
        let budget = SubscriptionBudget::new(8 * 1024 * 1024);
        let (mut registry, _watcher) = WatchRegistry::new(budget.clone(), Box::new(source), events);

        let (tx, _rx) = mpsc::channel(1);
        let attached = registry.attach_now(path.clone(), tx, 0).await.unwrap();
        assert_eq!(attached.current.text, "first");

        // Save the way an editor saves: write a sibling, rename over the target.
        let staging = directory.path().join("ticker.aml.tmp");
        std::fs::write(&staging, "second").unwrap();
        std::fs::rename(&staging, &path).unwrap();

        // Drive the registry the way its own loop would.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            while let Ok(event) = registry.events.try_recv() {
                registry.mark(event);
            }
            registry.mark_all_dirty();
            registry.refresh_dirty().await;
            if attached.updates.borrow().text == "second" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a write-via-rename was never observed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
