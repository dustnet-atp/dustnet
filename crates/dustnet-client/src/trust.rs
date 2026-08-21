//! Pinned certificates for sites no certification authority vouches for.
//!
//! Dustnet's premise is that anyone can run a site. Until this existed a client
//! had two ways to reach one: a certificate signed by a CA in the compiled-in
//! `webpki-roots` bundle, which needs a registered domain, or `--insecure`,
//! which disables certificate *and* host name verification for every
//! connection it is passed. The second is not a trust model — an `--insecure`
//! connection is encrypted and unauthenticated, and offers nothing against an
//! active man in the middle.
//!
//! This is the SSH model instead. The first time a site is reached with
//! `--tofu`, its certificate fingerprint is written here. Every later
//! connection to the same host and port must present the same certificate, and
//! a mismatch is a hard failure rather than a prompt. That is weaker than a CA
//! for the first connection and stronger for every one after it.
//!
//! # What is pinned
//!
//! The SHA-256 of the end-entity certificate in DER form, keyed by the host and
//! port the user typed. Pinning the whole certificate rather than its public
//! key means a renewal breaks the pin even when the key is unchanged; that is
//! the honest trade for not parsing X.509 here, and a broken pin is a visible
//! failure the operator can publish a new fingerprint for.
//!
//! A pin authenticates a peer only in combination with a real signature check.
//! The certificate is public, so a verifier that matched the fingerprint and
//! then waved the handshake signature through would let anyone who had ever
//! seen the certificate impersonate the site. [`crate::transport`] verifies the
//! signature against the pinned certificate for exactly this reason.
//!
//! # File
//!
//! One pin per line, in the order they were made:
//!
//! ```text
//! # host port sha256 first-seen
//! example.com 1985 3b1f…64 hex chars… 1755720000
//! ```
//!
//! Line-based and greppable because the recovery path for a mismatch is a
//! human deleting a line. A store that could only be edited by the program
//! that wrote it would make "I re-keyed my server" unrecoverable without one.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Bytes in a SHA-256 digest.
const FINGERPRINT_BYTES: usize = 32;

/// A SHA-256 digest of a DER certificate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fingerprint([u8; FINGERPRINT_BYTES]);

impl Fingerprint {
    /// Digest an end-entity certificate exactly as it arrived on the wire.
    pub fn of_certificate(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut bytes = [0u8; FINGERPRINT_BYTES];
        for (slot, byte) in bytes.iter_mut().zip(digest.as_ref()) {
            *slot = *byte;
        }
        Self(bytes)
    }

    /// Parse the lowercase hex form written to the store.
    pub fn parse_hex(text: &str) -> Option<Self> {
        if text.len() != FINGERPRINT_BYTES * 2 {
            return None;
        }
        let mut bytes = [0u8; FINGERPRINT_BYTES];
        for (slot, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).ok()?;
            *slot = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// One pinned site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub fingerprint: Fingerprint,
    /// Seconds since the Unix epoch, recorded so a reader can tell an old
    /// decision from one made a minute ago. Never used for expiry: a pin does
    /// not weaken with age, and expiring one silently would re-open the first
    /// connection the store exists to protect.
    pub first_seen: u64,
}

/// Why a store could not be used.
#[derive(Debug)]
pub enum TrustError {
    /// The store exists but anyone on the machine can rewrite it.
    Writable {
        path: PathBuf,
        mode: u32,
    },
    /// A line could not be understood. Refusing beats skipping: a store that
    /// silently dropped the line pinning a site would re-prompt for it.
    Malformed {
        path: PathBuf,
        line: usize,
    },
    /// No home or configuration directory could be located.
    NoLocation,
    /// An operator-supplied certificate authority file was unusable.
    Certificates {
        path: PathBuf,
        reason: String,
    },
    Io(io::Error),
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrustError::Writable { path, mode } => write!(
                f,
                "trust store {} is writable by other users (mode {mode:04o}); \
                 anyone who can edit it can impersonate every site in it. \
                 Fix with: chmod 600 {}",
                path.display(),
                path.display()
            ),
            TrustError::Malformed { path, line } => write!(
                f,
                "trust store {} is malformed at line {line}; \
                 delete the line to re-pin that site",
                path.display()
            ),
            TrustError::Certificates { path, reason } => {
                write!(
                    f,
                    "cannot use certificate authority {}: {reason}",
                    path.display()
                )
            }
            TrustError::NoLocation => {
                write!(
                    f,
                    "no configuration directory (set HOME or XDG_CONFIG_HOME)"
                )
            }
            TrustError::Io(error) => write!(f, "trust store: {error}"),
        }
    }
}

impl std::error::Error for TrustError {}

impl From<io::Error> for TrustError {
    fn from(error: io::Error) -> Self {
        TrustError::Io(error)
    }
}

/// Where the store lives.
///
/// `DUSTNET_TRUST_STORE` wins, then `XDG_CONFIG_HOME`, then `~/.config`. The
/// same order [`crate::config`] resolves `client.conf` with, so a user who has
/// moved one has moved both.
pub fn default_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("DUSTNET_TRUST_STORE") {
        return Some(PathBuf::from(explicit));
    }
    let config_dir = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(config_dir.join("dustnet").join("known_sites"))
}

/// Pinned certificates, keyed by the host and port the user typed.
#[derive(Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    pins: BTreeMap<(String, u16), Pin>,
}

impl TrustStore {
    /// Load from [`default_path`], treating a missing file as an empty store.
    pub fn load() -> Result<Self, TrustError> {
        let path = default_path().ok_or(TrustError::NoLocation)?;
        Self::load_from(&path)
    }

    /// Load from an explicit path.
    ///
    /// A missing file is an empty store — nobody has pinned anything yet. Every
    /// other failure is an error: a store that could not be read is not the
    /// same as a store with nothing in it, and treating them alike would turn
    /// an unreadable file into a silent re-pin of every site.
    pub fn load_from(path: &Path) -> Result<Self, TrustError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(TrustStore {
                    path: Some(path.to_path_buf()),
                    pins: BTreeMap::new(),
                });
            }
            Err(error) => return Err(TrustError::Io(error)),
        };
        check_permissions(path)?;

        let mut pins = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let malformed = || TrustError::Malformed {
                path: path.to_path_buf(),
                line: index + 1,
            };
            let mut fields = line.split_whitespace();
            let host = fields.next().ok_or_else(malformed)?;
            let port: u16 = fields
                .next()
                .ok_or_else(malformed)?
                .parse()
                .map_err(|_| malformed())?;
            let fingerprint = Fingerprint::parse_hex(fields.next().ok_or_else(malformed)?)
                .ok_or_else(malformed)?;
            let first_seen: u64 = fields
                .next()
                .ok_or_else(malformed)?
                .parse()
                .map_err(|_| malformed())?;
            if fields.next().is_some() {
                return Err(malformed());
            }
            pins.insert(
                (host.to_ascii_lowercase(), port),
                Pin {
                    fingerprint,
                    first_seen,
                },
            );
        }

        Ok(TrustStore {
            path: Some(path.to_path_buf()),
            pins,
        })
    }

    /// An in-memory store that is never written. For callers that have
    /// disabled pinning, so the verifier does not need a second code path.
    pub fn detached() -> Self {
        TrustStore {
            path: None,
            pins: BTreeMap::new(),
        }
    }

    pub fn pin_for(&self, host: &str, port: u16) -> Option<Pin> {
        self.pins.get(&(host.to_ascii_lowercase(), port)).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// Every pin, host-ordered, for `dustnet trust list`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u16, &Pin)> {
        self.pins
            .iter()
            .map(|((host, port), pin)| (host.as_str(), *port, pin))
    }

    /// Record a pin and persist the store.
    pub fn record(
        &mut self,
        host: &str,
        port: u16,
        fingerprint: Fingerprint,
    ) -> Result<(), TrustError> {
        self.pins.insert(
            (host.to_ascii_lowercase(), port),
            Pin {
                fingerprint,
                first_seen: now_unix(),
            },
        );
        self.persist()
    }

    /// Drop a pin and persist. Returns whether anything was pinned.
    pub fn forget(&mut self, host: &str, port: u16) -> Result<bool, TrustError> {
        let removed = self
            .pins
            .remove(&(host.to_ascii_lowercase(), port))
            .is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Write the store out, replacing it in one step.
    ///
    /// Written to a sibling temporary file and renamed, so a crash or a full
    /// disk leaves the previous store intact rather than a half-written one.
    /// A truncated trust store is worse than a stale one: the pins that
    /// survived the truncation are the sites that stay protected, and which
    /// those are would be decided by where the write stopped.
    fn persist(&self) -> Result<(), TrustError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut body = String::from(
            "# dustnet known sites. One pinned certificate per line:\n\
             #   host port sha256 first-seen\n\
             # Delete a line to forget that site; the next --tofu connection re-pins it.\n",
        );
        for ((host, port), pin) in &self.pins {
            use fmt::Write as _;
            let _ = writeln!(body, "{host} {port} {} {}", pin.fingerprint, pin.first_seen);
        }

        let temporary = path.with_extension("tmp");
        write_private(&temporary, &body)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    }
}

/// Write owner-only, creating the file with restrictive permissions rather
/// than relaxing them afterwards — a file that is briefly world-readable is a
/// file another process on the machine had a chance to read.
fn write_private(path: &Path, body: &str) -> Result<(), TrustError> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // No mode to set. Recorded as a posture downgrade in
        // docs/guides/production-support.md rather than pretended away.
        std::fs::write(path, body)?;
        Ok(())
    }
}

/// Refuse a store other users can rewrite.
///
/// A pin is only worth what the file holding it is worth: an attacker who can
/// write a line here chooses the certificate the client will accept, which is
/// a man in the middle with the client's own cooperation. Checked rather than
/// silently repaired, because a store that suddenly became group-writable is
/// evidence about the machine, not a permissions bug to tidy up.
fn check_permissions(path: &Path) -> Result<(), TrustError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(TrustError::Writable {
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

/// Read PEM certificate authorities an operator supplied with `--ca-file`.
///
/// Lives here rather than in the command line so that no caller outside this
/// crate has to name a TLS type to configure trust.
pub fn load_certificate_authorities(
    path: &Path,
) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, TrustError> {
    use rustls_pki_types::pem::PemObject as _;
    let anchors: Vec<_> = rustls_pki_types::CertificateDer::pem_file_iter(path)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|error| TrustError::Certificates {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if anchors.is_empty() {
        // An empty file is far more likely to be the wrong path than a
        // deliberate instruction to trust nothing extra, and silently adding
        // no anchors would fail later as a confusing handshake error.
        return Err(TrustError::Certificates {
            path: path.to_path_buf(),
            reason: "no certificates found".into(),
        });
    }
    Ok(anchors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint::of_certificate(&[seed; 16])
    }

    #[test]
    fn a_missing_store_is_empty_rather_than_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::load_from(&directory.path().join("known_sites")).unwrap();
        assert!(store.is_empty());
        assert!(store.pin_for("example.com", 1985).is_none());
    }

    #[test]
    fn pins_round_trip_through_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_sites");

        let mut store = TrustStore::load_from(&path).unwrap();
        store.record("Example.COM", 1985, fingerprint(1)).unwrap();
        store.record("other.example", 9000, fingerprint(2)).unwrap();

        let reloaded = TrustStore::load_from(&path).unwrap();
        assert_eq!(
            reloaded.pin_for("example.com", 1985).unwrap().fingerprint,
            fingerprint(1)
        );
        assert_eq!(
            reloaded.pin_for("other.example", 9000).unwrap().fingerprint,
            fingerprint(2)
        );
        // Host names are matched the way the URI parser canonicalises them, so
        // a pin made for `Example.COM` governs `example.com`.
        assert_eq!(
            reloaded.pin_for("EXAMPLE.com", 1985).unwrap().fingerprint,
            fingerprint(1)
        );
        // The port is part of the key: a different service on the same host is
        // a different peer with a different certificate.
        assert!(reloaded.pin_for("example.com", 9000).is_none());
    }

    /// Refusing beats skipping. A store that dropped the line it could not read
    /// would silently stop protecting exactly one site, and the next connection
    /// to it would look like an ordinary first use.
    #[test]
    fn a_malformed_line_is_refused_rather_than_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_sites");
        std::fs::write(
            &path,
            format!(
                "# comment\nexample.com 1985 {} 100\nbroken.example 1985 not-a-digest 100\n",
                fingerprint(1)
            ),
        )
        .unwrap();

        let error = TrustStore::load_from(&path).unwrap_err();
        match error {
            TrustError::Malformed { line, .. } => assert_eq!(line, 3),
            other => panic!("expected a malformed-line error, got {other:?}"),
        }
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_sites");
        std::fs::write(
            &path,
            format!(
                "# a comment\n\n   \nexample.com 1985 {} 100\n",
                fingerprint(3)
            ),
        )
        .unwrap();
        let store = TrustStore::load_from(&path).unwrap();
        assert_eq!(
            store.pin_for("example.com", 1985).unwrap().fingerprint,
            fingerprint(3)
        );
    }

    /// A pin is worth exactly what the file holding it is worth: anyone who can
    /// write a line chooses the certificate the client will accept.
    #[cfg(unix)]
    #[test]
    fn a_store_other_users_can_write_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_sites");
        std::fs::write(&path, format!("example.com 1985 {} 100\n", fingerprint(1))).unwrap();

        // Only the write bits matter: group-write, other-write, and both.
        for mode in [0o666, 0o622, 0o606, 0o620, 0o602] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(
                    TrustStore::load_from(&path),
                    Err(TrustError::Writable { .. })
                ),
                "mode {mode:04o} lets another user rewrite the store"
            );
        }

        // Readable by others is not the same problem: a fingerprint is public,
        // and refusing 0644 would reject a store that is perfectly safe.
        for mode in [0o600, 0o644, 0o444] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                TrustStore::load_from(&path).is_ok(),
                "mode {mode:04o} is readable, not writable, and must be accepted"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("known_sites");

        let mut store = TrustStore::load_from(&path).unwrap();
        store.record("example.com", 1985, fingerprint(1)).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "created store is mode {mode:04o}");
        // And it reloads: a file this strict is still one this process reads.
        assert!(
            TrustStore::load_from(&path)
                .unwrap()
                .pin_for("example.com", 1985)
                .is_some()
        );
    }

    #[test]
    fn forgetting_a_pin_persists_and_reports_whether_anything_went() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("known_sites");
        let mut store = TrustStore::load_from(&path).unwrap();
        store.record("example.com", 1985, fingerprint(1)).unwrap();

        assert!(store.forget("EXAMPLE.com", 1985).unwrap());
        assert!(!store.forget("example.com", 1985).unwrap());
        assert!(TrustStore::load_from(&path).unwrap().is_empty());
    }

    #[test]
    fn a_detached_store_never_writes() {
        let mut store = TrustStore::detached();
        store.record("example.com", 1985, fingerprint(1)).unwrap();
        assert_eq!(
            store.pin_for("example.com", 1985).unwrap().fingerprint,
            fingerprint(1)
        );
    }

    #[test]
    fn fingerprints_round_trip_as_hex_and_reject_anything_else() {
        let printed = fingerprint(7).to_string();
        assert_eq!(printed.len(), FINGERPRINT_BYTES * 2);
        assert_eq!(Fingerprint::parse_hex(&printed), Some(fingerprint(7)));

        assert_eq!(Fingerprint::parse_hex(""), None);
        assert_eq!(Fingerprint::parse_hex(&printed[..62]), None);
        assert_eq!(Fingerprint::parse_hex(&format!("{printed}00")), None);
        assert_eq!(Fingerprint::parse_hex(&"z".repeat(64)), None);
    }

    #[test]
    fn different_certificates_do_not_share_a_fingerprint() {
        assert_ne!(fingerprint(1), fingerprint(2));
        assert_eq!(
            Fingerprint::of_certificate(b"same"),
            Fingerprint::of_certificate(b"same")
        );
    }
}
