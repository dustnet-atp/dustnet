//! At-rest storage for CA-verified session tokens.
//!
//! On unless `remember-sessions = false`, in which case
//! [`crate::session_store`] holds sessions for the life of the process and
//! they go when it does. Remembering is the default because logging in again
//! at every launch is a cost paid continuously and counted nowhere, and what
//! the protocol hands the client is a revocable, path-scoped, expiring
//! credential rather than the password behind it. The client never stores a
//! password.
//!
//! It is still a credential at rest, which the memory-only client did not
//! have. The honest form of that trade is not to minimise it but to make the
//! stored thing as small as possible, which is what the rest of this module
//! is about.
//!
//! # What persistence may not do
//!
//! Nothing here widens what a token reaches. A stored token is sent under
//! exactly the rules a live one is — same origin, same scope, same refusal on
//! plaintext. What this module does instead is narrow what is eligible to be
//! written at all, in four ways worth stating because each closes something
//! that a naive file format would leave open:
//!
//! **Only CA-verified origins.** A session store is keyed by [`Origin`], and
//! an origin includes its [`TransportSecurity`]: a `--tofu`, `--insecure` and
//! CA-verified session to one host and port are three separate partitions, and
//! merging them is the dangerous kind of bug. So the file records no security
//! label at all. Every line is written from a `verified-tls` origin and read
//! back as one, which means there is no field an attacker who can write here
//! could alter to promote a token into a stronger partition than the one it
//! was earned in. A pinned or insecure session stays in memory and dies with
//! the process, which is also the right lifetime for what is a development
//! posture in the first place.
//!
//! **Only tokens with an expiry.** [`SessionToken::expires`] is optional on the
//! wire. In memory a token with no expiry still dies at exit; on disk it would
//! be a credential with no end. Those are written out, and an expired line is
//! dropped on load rather than sent.
//!
//! **Owner-only, checked on the way in.** Written `0600`, and refused on load
//! when another user can read it. This is the mirror image of the pin store's
//! check and deliberately points the other way: a pin is worth the *integrity*
//! of the file holding it, since whoever can write a line chooses the
//! certificate the client accepts. A session token is worth its
//! *confidentiality* — whoever can read a line is logged in as the user.
//!
//! **Admitted like a server directive.** [`load`] returns directives rather
//! than a populated store, so the caller feeds them through the same
//! `apply_directive` path a PAGE response takes. The per-site and total bounds
//! and the client's memory accounting therefore apply to a file exactly as
//! they apply to a hostile server, and a file with 10,000 lines in it is not a
//! way around either.
//!
//! # File
//!
//! One session per line, `host port` followed by the wire form of the
//! `Set-Session` value it came from:
//!
//! ```text
//! # host port token scope expires
//! example.com 1985 3b1f…64 hex chars… /admin/ 1789000000
//! ```
//!
//! The tail is parsed by [`SessionDirective::parse_set`] — the same function
//! that reads the directive off the wire — so token and scope validation is
//! not restated here and cannot drift from it. Unlike the pin store, this file
//! is not meant to be edited line by line: a line is a live credential, and
//! the recovery path is deleting the file, which `:sessions clear` also does.

use std::io;
use std::path::{Path, PathBuf};

use dustnet_core::protocol::origin::{Origin, TransportSecurity};
use dustnet_core::protocol::uri::AtpUri;
use dustnet_core::session::{SessionDirective, SessionStore};

/// The security level whose sessions may be written. Named once so the writer
/// and the reader cannot disagree about it.
const PERSISTED_SECURITY: TransportSecurity = TransportSecurity::VerifiedTls;

#[derive(Debug)]
pub enum SessionFileError {
    /// The store exists but users other than the owner can read it.
    Readable {
        path: PathBuf,
        mode: u32,
    },
    /// A line could not be parsed as a host, port and session directive.
    Malformed {
        path: PathBuf,
        line: usize,
    },
    /// Neither `DUSTNET_SESSION_STORE` nor a home directory was resolvable.
    NoLocation,
    Io(io::Error),
}

impl std::fmt::Display for SessionFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionFileError::Readable { path, mode } => write!(
                f,
                "session store {} is readable by other users (mode {mode:04o}); \
                 anyone who can read it is logged in as you. \
                 Run `chmod 600 {}`, or delete it and log in again.",
                path.display(),
                path.display()
            ),
            SessionFileError::Malformed { path, line } => write!(
                f,
                "session store {} is malformed at line {line}; \
                 delete {} and log in again.",
                path.display(),
                path.display()
            ),
            SessionFileError::NoLocation => write!(
                f,
                "no location for the session store: set DUSTNET_SESSION_STORE or HOME"
            ),
            SessionFileError::Io(error) => write!(f, "session store: {error}"),
        }
    }
}

impl std::error::Error for SessionFileError {}

impl From<io::Error> for SessionFileError {
    fn from(error: io::Error) -> Self {
        SessionFileError::Io(error)
    }
}

/// Where the store lives.
///
/// `DUSTNET_SESSION_STORE` wins, then `XDG_STATE_HOME`, then
/// `~/.local/state`. Under state rather than config, next to the pin store's
/// neighbourhood but not in it: a remembered session is state the client
/// acquired, not a preference the user expressed, and someone copying their
/// dotfiles to a second machine should not carry their logins along.
pub fn default_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("DUSTNET_SESSION_STORE") {
        return Some(PathBuf::from(explicit));
    }
    let state_dir = if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?)
            .join(".local")
            .join("state")
    };
    Some(state_dir.join("dustnet").join("sessions"))
}

/// A session store on disk, or a detached one that is never written.
#[derive(Debug, Default, Clone)]
pub struct SessionFile {
    path: Option<PathBuf>,
}

impl SessionFile {
    /// The store at [`default_path`].
    pub fn at_default_path() -> Result<Self, SessionFileError> {
        Ok(Self {
            path: Some(default_path().ok_or(SessionFileError::NoLocation)?),
        })
    }

    /// A store at an explicit path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    /// A store that reads nothing and writes nothing, so a client with
    /// persistence disabled needs no second code path.
    ///
    /// This is what an `AtpClient` gets until one is attached, which keeps the
    /// user-facing default (remember) and the API default (do not) separate:
    /// the viewer opts in once, and no other construction site — no test —
    /// acquires a credential file by saying nothing.
    pub fn detached() -> Self {
        Self { path: None }
    }

    pub fn is_detached(&self) -> bool {
        self.path.is_none()
    }

    /// Sessions worth restoring, as the directives that would have set them.
    ///
    /// A missing file is an empty store — nobody has logged in yet. Unlike the
    /// pin store, an unreadable file is *not* fatal to the client: the worst
    /// case is a login prompt, so the caller is free to report the error and
    /// carry on with no sessions. What is fatal to trusting the file is a
    /// permissions failure or a malformed line, both of which are reported
    /// rather than skipped past, because a store that has been tampered with
    /// is evidence about the machine rather than a parsing inconvenience.
    ///
    /// Expired and non-conforming lines are dropped silently — those are the
    /// ordinary passage of time, not a problem to report.
    pub fn load(&self) -> Result<Vec<(Origin, SessionDirective)>, SessionFileError> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SessionFileError::Io(error)),
        };
        check_permissions(path)?;

        let now = now_unix();
        let mut restored = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let malformed = || SessionFileError::Malformed {
                path: path.to_path_buf(),
                line: index + 1,
            };
            let (host, rest) = line.split_once(' ').ok_or_else(malformed)?;
            let (port, value) = rest.split_once(' ').ok_or_else(malformed)?;
            let port: u16 = port.parse().map_err(|_| malformed())?;

            // The wire parser owns token and scope validation, so a line that
            // could not have arrived over ATP cannot be introduced here.
            let directive = SessionDirective::parse_set(value)
                .map_err(|_| malformed())?
                .ok_or_else(malformed)?;
            let SessionDirective::Set { expires, .. } = &directive else {
                return Err(malformed());
            };
            // A line with no expiry was never written by this module, and a
            // lapsed one has nothing left to restore.
            let Some(expires) = *expires else {
                continue;
            };
            if expires <= now {
                continue;
            }

            let uri = AtpUri::parse(&format!("atp://{}:{port}/", bracketed(host)))
                .map_err(|_| malformed())?;
            let origin = Origin::try_from_uri(&uri, PERSISTED_SECURITY).map_err(|_| malformed())?;
            restored.push((origin, directive));
        }
        Ok(restored)
    }

    /// Write every eligible session out, replacing the file in one step.
    ///
    /// Written to a sibling temporary file and renamed, as the pin store is,
    /// so a crash leaves the previous store rather than a truncated one. A
    /// half-written session file is not a security failure the way a truncated
    /// pin store is — the tokens that were lost simply prompt a login — but a
    /// file that parses is still worth more than one that does not.
    ///
    /// A store with nothing eligible in it removes the file instead of writing
    /// an empty one, so the last logout leaves nothing at rest.
    pub fn save(&self, store: &SessionStore) -> Result<(), SessionFileError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let now = now_unix();
        let mut body = String::from(
            "# dustnet remembered sessions. These are live credentials: anyone\n\
             # who can read this file is logged in as you.\n\
             #   host port token scope expires\n\
             # Only CA-verified sites are stored. Delete this file, or run\n\
             # `:sessions clear`, to log out everywhere.\n",
        );
        let mut written = 0usize;
        for (origin, site) in store.iter_sites() {
            if origin.security() != PERSISTED_SECURITY {
                continue;
            }
            for token in site.list_tokens() {
                let Some(expires) = token.expires else {
                    continue;
                };
                if expires <= now {
                    continue;
                }
                use std::fmt::Write as _;
                let _ = writeln!(
                    body,
                    "{} {} {} {} {expires}",
                    bracketed(origin.host()),
                    origin.port(),
                    token.token,
                    token.scope
                );
                written += 1;
            }
        }

        if written == 0 {
            return self.remove();
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        crate::trust::write_private(&temporary, &body)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Delete the store. A missing file is already the desired state.
    pub fn remove(&self) -> Result<(), SessionFileError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SessionFileError::Io(error)),
        }
    }
}

/// Re-bracket an IPv6 literal so the canonical host round-trips through
/// `AtpUri::parse`, which requires brackets to tell an address from a port.
fn bracketed(host: &str) -> std::borrow::Cow<'_, str> {
    if host.contains(':') && !host.starts_with('[') {
        std::borrow::Cow::Owned(format!("[{host}]"))
    } else {
        std::borrow::Cow::Borrowed(host)
    }
}

/// Refuse a store other users can read.
///
/// Checked rather than silently repaired, for the same reason the pin store
/// checks its own mode: a credential file that became group-readable is
/// evidence about the machine, and tightening the mode would hide the fact
/// that the tokens inside had already been exposed.
fn check_permissions(path: &Path) -> Result<(), SessionFileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(SessionFileError::Readable {
                path: path.to_path_buf(),
                mode: mode & 0o7777,
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin_at(host: &str, security: TransportSecurity) -> Origin {
        Origin::from_uri(&AtpUri::parse(&format!("atp://{host}/")).unwrap(), security).unwrap()
    }

    fn store_with(entries: &[(Origin, &str, &str, Option<u64>)]) -> SessionStore {
        let mut store = SessionStore::new();
        for (origin, token, scope, expires) in entries {
            assert!(store.apply_directive(
                origin,
                &SessionDirective::Set {
                    token: (*token).into(),
                    scope: (*scope).into(),
                    expires: *expires,
                },
            ));
        }
        store
    }

    fn future() -> u64 {
        now_unix() + 3_600
    }

    fn temporary_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dustnet-session-file-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_verified_session_survives_a_round_trip() {
        let dir = temporary_dir("round-trip");
        let file = SessionFile::at(dir.join("sessions"));
        let expires = future();
        let store = store_with(&[(
            origin_at("example.com", TransportSecurity::VerifiedTls),
            "abc123",
            "/admin/",
            Some(expires),
        )]);
        file.save(&store).unwrap();

        let restored = file.load().unwrap();
        assert_eq!(restored.len(), 1);
        let (origin, directive) = &restored[0];
        assert_eq!(origin.host(), "example.com");
        assert_eq!(origin.security(), TransportSecurity::VerifiedTls);
        assert_eq!(
            directive,
            &SessionDirective::Set {
                token: "abc123".into(),
                scope: "/admin/".into(),
                expires: Some(expires),
            }
        );
    }

    /// The reason the file carries no security label: everything it can
    /// produce is a `verified-tls` origin, so a weaker session cannot be
    /// stored and then read back as a stronger one.
    #[test]
    fn only_ca_verified_sessions_are_written_to_disk() {
        let dir = temporary_dir("verified-only");
        let file = SessionFile::at(dir.join("sessions"));
        let expires = future();
        let mut entries = Vec::new();
        for security in TransportSecurity::ALL {
            let host = if security == TransportSecurity::PlaintextLoopback {
                "127.0.0.1"
            } else {
                "example.com"
            };
            entries.push((origin_at(host, security), "tok", "/", Some(expires)));
        }
        let store = store_with(&entries);
        assert_eq!(store.total_count(), TransportSecurity::ALL.len());

        file.save(&store).unwrap();
        let restored = file.load().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0.security(), TransportSecurity::VerifiedTls);
    }

    #[test]
    fn a_session_without_an_expiry_is_never_stored() {
        let dir = temporary_dir("no-expiry");
        let file = SessionFile::at(dir.join("sessions"));
        let store = store_with(&[(
            origin_at("example.com", TransportSecurity::VerifiedTls),
            "forever",
            "/",
            None,
        )]);
        file.save(&store).unwrap();
        // Nothing was eligible, so the last logout left nothing at rest.
        assert!(!dir.join("sessions").exists());
        assert!(file.load().unwrap().is_empty());
    }

    #[test]
    fn an_expired_session_is_dropped_on_load() {
        let dir = temporary_dir("expired");
        let path = dir.join("sessions");
        let file = SessionFile::at(&path);
        crate::trust::write_private(&path, "example.com 1985 tok / 1\n").unwrap();
        assert!(file.load().unwrap().is_empty());
    }

    #[test]
    fn a_session_store_other_users_can_read_is_refused() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = temporary_dir("readable");
            let path = dir.join("sessions");
            let expires = future();
            crate::trust::write_private(&path, &format!("example.com 1985 tok / {expires}\n"))
                .unwrap();
            // Loads while owner-only.
            assert_eq!(SessionFile::at(&path).load().unwrap().len(), 1);

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            let error = SessionFile::at(&path).load().unwrap_err();
            assert!(
                matches!(error, SessionFileError::Readable { .. }),
                "expected a refusal, got {error}"
            );
        }
    }

    #[test]
    fn a_written_session_store_is_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = temporary_dir("mode");
            let path = dir.join("sessions");
            let store = store_with(&[(
                origin_at("example.com", TransportSecurity::VerifiedTls),
                "tok",
                "/",
                Some(future()),
            )]);
            SessionFile::at(&path).save(&store).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {:04o}", mode & 0o777);
        }
    }

    #[test]
    fn a_malformed_line_is_reported_rather_than_skipped() {
        let dir = temporary_dir("malformed");
        let path = dir.join("sessions");
        crate::trust::write_private(&path, "example.com not-a-port tok / 1\n").unwrap();
        assert!(matches!(
            SessionFile::at(&path).load().unwrap_err(),
            SessionFileError::Malformed { line: 1, .. }
        ));
    }

    /// A token that could not have arrived over ATP cannot be introduced by
    /// editing the file, because the same wire parser reads both.
    #[test]
    fn a_traversing_scope_is_refused_on_load() {
        let dir = temporary_dir("traversal");
        let path = dir.join("sessions");
        let expires = future();
        crate::trust::write_private(&path, &format!("example.com 1985 tok /../etc/ {expires}\n"))
            .unwrap();
        assert!(matches!(
            SessionFile::at(&path).load().unwrap_err(),
            SessionFileError::Malformed { line: 1, .. }
        ));
    }

    #[test]
    fn a_detached_store_reads_nothing_and_writes_nothing() {
        let file = SessionFile::detached();
        assert!(file.is_detached());
        let store = store_with(&[(
            origin_at("example.com", TransportSecurity::VerifiedTls),
            "tok",
            "/",
            Some(future()),
        )]);
        file.save(&store).unwrap();
        assert!(file.load().unwrap().is_empty());
    }

    #[test]
    fn an_ipv6_host_round_trips_through_the_store() {
        let dir = temporary_dir("ipv6");
        let file = SessionFile::at(dir.join("sessions"));
        let store = store_with(&[(
            origin_at("[::1]:1985", TransportSecurity::VerifiedTls),
            "tok",
            "/",
            Some(future()),
        )]);
        file.save(&store).unwrap();
        let restored = file.load().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0.port(), 1985);
    }

    #[test]
    fn a_missing_store_is_an_empty_one() {
        let dir = temporary_dir("missing");
        assert!(
            SessionFile::at(dir.join("never-written"))
                .load()
                .unwrap()
                .is_empty()
        );
    }
}
