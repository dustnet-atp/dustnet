use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

// ─── Types ──────────────────────────────────────────────

/// A registered user.
#[derive(Clone)]
struct User {
    name: String,
    email: String,
    password_hash: String,
    verified: bool,
    created_at: u64,
}

/// A pending registration awaiting email verification.
#[derive(Clone)]
struct PendingRegistration {
    email: String,
    verification_code: String,
    expires_at: u64,
    name: String,
    password_hash: String,
}

/// An active session.
#[derive(Clone)]
pub(crate) struct SessionInfo {
    pub username: String,
    pub created_at: u64,
    pub expires_at: u64,
}

/// Session lifetime in seconds (24 hours).
const SESSION_LIFETIME_SECS: u64 = 24 * 3600;

/// Verification code lifetime in seconds (1 hour).
const VERIFICATION_LIFETIME_SECS: u64 = 3600;

const MAX_USERS: usize = 10_000;
const MAX_PENDING_REGISTRATIONS: usize = 2_048;
const MAX_SESSIONS: usize = 8_192;
const MAX_SESSIONS_PER_USER: usize = 8;
const MAX_RATE_LIMIT_KEYS: usize = 16_384;

// ─── Auth System ────────────────────────────────────────

/// Combined user store, session store, and rate limiter.
///
/// All persistent state lives in `site_root/.auth/state.tsv`. Users, pending
/// registrations, and sessions are committed in one atomic replacement so a
/// logical auth operation cannot leave the files mutually inconsistent.
pub(crate) struct AuthSystem {
    users: Vec<User>,
    pending: Vec<PendingRegistration>,
    sessions: HashMap<String, SessionInfo>,
    rate_limiter: AuthRateLimiter,
    auth_dir: PathBuf,
    /// Valid Argon2 hash used to equalize unknown/unverified login work.
    dummy_password_hash: String,
}

impl AuthSystem {
    /// Load or initialize the auth system for a site root.
    pub fn load(site_root: &Path) -> Self {
        let auth_dir = site_root.join(".auth");
        if !auth_dir.exists() {
            let _ = std::fs::create_dir_all(&auth_dir);
        }
        restrict_directory_permissions(&auth_dir);
        for name in ["state.tsv", "users.tsv", "pending.tsv", "sessions.tsv"] {
            restrict_file_permissions(&auth_dir.join(name));
        }

        let state_path = auth_dir.join("state.tsv");
        let (users, pending, sessions) = if state_path.exists() {
            load_state(&state_path)
        } else {
            // One-way compatibility migration from the original three-file
            // layout. Legacy files remain untouched as a recovery aid.
            (
                load_users(&auth_dir),
                load_pending(&auth_dir),
                load_sessions(&auth_dir),
            )
        };

        let dummy_password_hash = Self::hash_password("dustnet-dummy-password")
            .expect("failed to initialize authentication timing defense");
        let system = AuthSystem {
            users,
            pending,
            sessions,
            rate_limiter: AuthRateLimiter::new(),
            auth_dir,
            dummy_password_hash,
        };
        if !state_path.exists() {
            let _ = system.save_state();
        }
        system
    }

    // ─── Token / Session ────────────────────────────────

    /// Generate a CSPRNG session token (32 bytes, hex-encoded).
    pub fn generate_token() -> String {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("failed to generate random bytes");
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Create a session for the given username. Returns the token.
    pub fn create_session(&mut self, username: &str) -> Result<String, String> {
        let previous_sessions = self.sessions.clone();
        let now = now_secs();
        self.sessions.retain(|_, session| session.expires_at > now);
        let mut user_sessions: Vec<(String, u64)> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.username == username)
            .map(|(token, session)| (token.clone(), session.created_at))
            .collect();
        if user_sessions.len() >= MAX_SESSIONS_PER_USER {
            user_sessions.sort_by_key(|(_, created_at)| *created_at);
            if let Some((oldest, _)) = user_sessions.first() {
                self.sessions.remove(oldest);
            }
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err("The server has reached its active-session limit.".into());
        }
        let token = Self::generate_token();
        self.sessions.insert(
            token.clone(),
            SessionInfo {
                username: username.to_string(),
                created_at: now,
                expires_at: now + SESSION_LIFETIME_SECS,
            },
        );
        if let Err(error) = self.save_state() {
            self.sessions = previous_sessions;
            return Err(error);
        }
        Ok(token)
    }

    /// Resolve a session token to a username. Returns `None` if expired or missing.
    pub fn resolve_session(&mut self, token: &str) -> Option<String> {
        let now = now_secs();
        if let Some(info) = self.sessions.get(token)
            && info.expires_at > now
        {
            return Some(info.username.clone());
        }
        // Expired — remove it
        self.sessions.remove(token);
        None
    }

    /// Remove a session by token.
    pub fn destroy_session(&mut self, token: &str) -> Result<(), String> {
        let removed = self.sessions.remove(token);
        if let Err(error) = self.save_state() {
            if let Some(info) = removed {
                self.sessions.insert(token.to_string(), info);
            }
            return Err(error);
        }
        Ok(())
    }

    // ─── User Store ─────────────────────────────────────

    /// Hash a password with argon2.
    pub fn hash_password(password: &str) -> Result<String, String> {
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes).expect("failed to generate salt");
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| format!("Salt encoding failed: {e}"))?;
        let argon2 = Argon2::default();
        argon2
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| format!("Password hashing failed: {e}"))
    }

    /// Verify a password against a stored hash.
    fn verify_password(password: &str, hash: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }

    /// Copy the small amount of immutable data needed for an expensive login
    /// check so callers can release the auth-state mutex before running
    /// Argon2. Unknown and unverified accounts receive the dummy hash.
    pub fn login_challenge(&self, name: &str) -> (Option<String>, String) {
        match self
            .users
            .iter()
            .find(|user| user.name.eq_ignore_ascii_case(name) && user.verified)
        {
            Some(user) => (Some(user.name.clone()), user.password_hash.clone()),
            None => (None, self.dummy_password_hash.clone()),
        }
    }

    pub fn verify_login_challenge(password: &str, hash: &str) -> bool {
        Self::verify_password(password, hash)
    }

    /// Authenticate a user by name and password. Returns the username on success.
    #[cfg(test)]
    pub fn authenticate(&self, name: &str, password: &str) -> Result<String, &'static str> {
        let user = self
            .users
            .iter()
            .find(|u| u.name.eq_ignore_ascii_case(name));

        match user {
            None => {
                Self::verify_password(password, &self.dummy_password_hash);
                Err("Invalid handle or password.")
            }
            Some(u) if !u.verified => {
                Self::verify_password(password, &self.dummy_password_hash);
                Err("Invalid handle or password.")
            }
            Some(u) => {
                if Self::verify_password(password, &u.password_hash) {
                    Ok(u.name.clone())
                } else {
                    Err("Invalid handle or password.")
                }
            }
        }
    }

    /// Check if a username is already taken (case-insensitive).
    pub fn username_taken(&self, name: &str) -> bool {
        self.users.iter().any(|u| u.name.eq_ignore_ascii_case(name))
            || self
                .pending
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Check if an email is already registered.
    pub fn email_taken(&self, email: &str) -> bool {
        self.users
            .iter()
            .any(|u| u.email.eq_ignore_ascii_case(email))
            || self
                .pending
                .iter()
                .any(|p| p.email.eq_ignore_ascii_case(email))
    }

    // ─── Registration ───────────────────────────────────

    /// Generate an 8-character alphanumeric verification code.
    fn generate_verification_code() -> String {
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        // Rejection sampling avoids modulo bias (252 is the largest multiple
        // of 36 representable below 256).
        let mut code = String::with_capacity(8);
        while code.len() < 8 {
            let mut bytes = [0u8; 16];
            getrandom::getrandom(&mut bytes).expect("failed to generate random bytes");
            for byte in bytes {
                if byte < 252 {
                    code.push(CHARSET[(byte as usize) % CHARSET.len()] as char);
                    if code.len() == 8 {
                        break;
                    }
                }
            }
        }
        code
    }

    /// Register a new user. Returns the verification code to email.
    /// Does NOT send the email — caller is responsible for that.
    #[cfg(test)]
    pub fn register(
        &mut self,
        name: String,
        email: String,
        password: &str,
    ) -> Result<String, String> {
        let password_hash = Self::hash_password(password)?;
        self.register_hashed(name, email, password_hash)
    }

    /// Commit a registration after password hashing has been performed
    /// outside the shared auth-state lock.
    pub fn register_hashed(
        &mut self,
        name: String,
        email: String,
        password_hash: String,
    ) -> Result<String, String> {
        if self.username_taken(&name) {
            return Err("That handle is already taken.".into());
        }
        if self.email_taken(&email) {
            return Err("That email is already registered.".into());
        }
        if self.users.len() >= MAX_USERS {
            return Err("The server has reached its account limit.".into());
        }

        let previous_pending = self.pending.clone();
        let code = loop {
            let candidate = Self::generate_verification_code();
            if !self
                .pending
                .iter()
                .any(|pending| pending.verification_code == candidate)
            {
                break candidate;
            }
        };
        let now = now_secs();

        // Prune expired pending registrations
        self.pending.retain(|p| p.expires_at > now);
        if self.pending.len() >= MAX_PENDING_REGISTRATIONS {
            return Err("The server has too many pending registrations.".into());
        }

        self.pending.push(PendingRegistration {
            email,
            verification_code: code.clone(),
            expires_at: now + VERIFICATION_LIFETIME_SECS,
            name,
            password_hash,
        });
        if let Err(error) = self.save_state() {
            self.pending = previous_pending;
            return Err(error);
        }

        Ok(code)
    }

    /// Remove a pending registration when its verification message could not be sent.
    pub fn cancel_registration(&mut self, code: &str) -> Result<(), String> {
        let previous = self.pending.clone();
        self.pending
            .retain(|registration| registration.verification_code != code);
        if let Err(error) = self.save_state() {
            self.pending = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Verify a pending registration with the given code.
    pub fn verify(&mut self, code: &str) -> Result<String, String> {
        let now = now_secs();

        // Find matching pending registration
        let idx = self
            .pending
            .iter()
            .position(|p| p.verification_code.eq_ignore_ascii_case(code) && p.expires_at > now);

        match idx {
            Some(i) => {
                if self.users.len() >= MAX_USERS {
                    return Err("The server has reached its account limit.".into());
                }
                let previous_users = self.users.clone();
                let previous_pending = self.pending.clone();
                let reg = self.pending.remove(i);
                self.users.push(User {
                    name: reg.name.clone(),
                    email: reg.email,
                    password_hash: reg.password_hash,
                    verified: true,
                    created_at: now,
                });
                if let Err(error) = self.save_state() {
                    self.users = previous_users;
                    self.pending = previous_pending;
                    return Err(error);
                }
                Ok(reg.name)
            }
            None => Err("Invalid or expired verification code.".into()),
        }
    }

    // ─── Rate Limiting ──────────────────────────────────

    /// Check login rate limit. Returns Err with message if limited.
    pub fn check_login_rate(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        self.rate_limiter.check_login(ip)
    }

    /// Record a login attempt.
    pub fn record_login_attempt(&mut self, ip: IpAddr) {
        self.rate_limiter.record_login(ip);
    }

    /// Check registration rate limit.
    pub fn check_register_rate(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        self.rate_limiter.check_register(ip)
    }

    /// Record a registration attempt.
    pub fn record_register_attempt(&mut self, ip: IpAddr) {
        self.rate_limiter.record_register(ip);
    }

    /// Check verification-code rate limit.
    pub fn check_verify_rate(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        self.rate_limiter.check_verify(ip)
    }

    /// Record a verification-code attempt.
    pub fn record_verify_attempt(&mut self, ip: IpAddr) {
        self.rate_limiter.record_verify(ip);
    }

    // ─── Persistence ────────────────────────────────────

    fn save_state(&self) -> Result<(), String> {
        let now = now_secs();
        let mut lines =
            Vec::with_capacity(self.users.len() + self.pending.len() + self.sessions.len());
        lines.extend(self.users.iter().map(|u| {
            format!(
                "U\t{}\t{}\t{}\t{}\t{}",
                u.name, u.email, u.password_hash, u.verified, u.created_at
            )
        }));
        lines.extend(self.pending.iter().filter(|p| p.expires_at > now).map(|p| {
            format!(
                "P\t{}\t{}\t{}\t{}\t{}",
                p.email, p.verification_code, p.expires_at, p.name, p.password_hash
            )
        }));
        lines.extend(
            self.sessions
                .iter()
                .filter(|(_, s)| s.expires_at > now)
                .map(|(token, s)| {
                    format!(
                        "S\t{}\t{}\t{}\t{}",
                        token, s.username, s.created_at, s.expires_at
                    )
                }),
        );
        atomic_write(&self.auth_dir.join("state.tsv"), &lines.join("\n"))
    }
}

// ─── Rate Limiter ───────────────────────────────────────

/// Per-route rate limiter for auth endpoints.
struct AuthRateLimiter {
    /// Login: (attempt_count, window_start) per IP.
    login: HashMap<IpAddr, (u32, Instant)>,
    /// Registration: (attempt_count, window_start) per IP.
    register: HashMap<IpAddr, (u32, Instant)>,
    /// Verification: (attempt_count, window_start) per IP.
    verify: HashMap<IpAddr, (u32, Instant)>,
}

/// Login: max 5 attempts per 15 minutes.
const LOGIN_MAX_ATTEMPTS: u32 = 5;
const LOGIN_WINDOW_SECS: u64 = 15 * 60;

/// Registration: max 3 attempts per hour.
const REGISTER_MAX_ATTEMPTS: u32 = 3;
const REGISTER_WINDOW_SECS: u64 = 3600;

/// Verification: max 10 attempts per 15 minutes.
const VERIFY_MAX_ATTEMPTS: u32 = 10;
const VERIFY_WINDOW_SECS: u64 = 15 * 60;

impl AuthRateLimiter {
    fn new() -> Self {
        AuthRateLimiter {
            login: HashMap::new(),
            register: HashMap::new(),
            verify: HashMap::new(),
        }
    }

    fn check_login(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        check_rate(&mut self.login, ip, LOGIN_MAX_ATTEMPTS, LOGIN_WINDOW_SECS)
    }

    fn record_login(&mut self, ip: IpAddr) {
        record_attempt(&mut self.login, ip, LOGIN_WINDOW_SECS);
    }

    fn check_register(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        check_rate(
            &mut self.register,
            ip,
            REGISTER_MAX_ATTEMPTS,
            REGISTER_WINDOW_SECS,
        )
    }

    fn record_register(&mut self, ip: IpAddr) {
        record_attempt(&mut self.register, ip, REGISTER_WINDOW_SECS);
    }

    fn check_verify(&mut self, ip: IpAddr) -> Result<(), &'static str> {
        check_rate(
            &mut self.verify,
            ip,
            VERIFY_MAX_ATTEMPTS,
            VERIFY_WINDOW_SECS,
        )
    }

    fn record_verify(&mut self, ip: IpAddr) {
        record_attempt(&mut self.verify, ip, VERIFY_WINDOW_SECS);
    }
}

fn check_rate<K: Eq + std::hash::Hash>(
    map: &mut HashMap<K, (u32, Instant)>,
    key: K,
    max: u32,
    window_secs: u64,
) -> Result<(), &'static str> {
    map.retain(|_, (_, started)| started.elapsed().as_secs() < window_secs);
    if map.len() >= MAX_RATE_LIMIT_KEYS && !map.contains_key(&key) {
        return Err("The server is temporarily at its rate-limit capacity.");
    }
    if let Some((count, started)) = map.get(&key)
        && started.elapsed().as_secs() < window_secs
        && *count >= max
    {
        return Err("Too many attempts. Please try again later.");
    }
    Ok(())
}

fn record_attempt<K: Eq + std::hash::Hash>(
    map: &mut HashMap<K, (u32, Instant)>,
    key: K,
    window_secs: u64,
) {
    map.retain(|_, (_, started)| started.elapsed().as_secs() < window_secs);
    if map.len() >= MAX_RATE_LIMIT_KEYS && !map.contains_key(&key) {
        return;
    }
    let entry = map.entry(key).or_insert((0, Instant::now()));
    if entry.1.elapsed().as_secs() >= window_secs {
        // Window expired, reset
        *entry = (1, Instant::now());
    } else {
        entry.0 += 1;
    }
}

// ─── File I/O helpers ───────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Atomic write: write to .tmp then rename into place.
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tsv.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| format!("Could not write authentication state: {e}"))?;
    file.write_all(content.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Could not write authentication state: {e}"))?;
    restrict_file_permissions(&tmp);
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Could not commit authentication state: {e}"))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
    restrict_file_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) {}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) {}

fn load_state(
    path: &Path,
) -> (
    Vec<User>,
    Vec<PendingRegistration>,
    HashMap<String, SessionInfo>,
) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new(), HashMap::new());
    };
    let now = now_secs();
    let mut users = Vec::new();
    let mut pending = Vec::new();
    let mut sessions = HashMap::new();

    for line in data.lines() {
        let mut parts = line.split('\t');
        match parts.next() {
            Some("U") => {
                let fields: Vec<&str> = parts.collect();
                if fields.len() != 5 {
                    continue;
                }
                let Ok(created_at) = fields[4].parse() else {
                    continue;
                };
                users.push(User {
                    name: fields[0].to_string(),
                    email: fields[1].to_string(),
                    password_hash: fields[2].to_string(),
                    verified: fields[3] == "true",
                    created_at,
                });
            }
            Some("P") => {
                let fields: Vec<&str> = parts.collect();
                if fields.len() != 5 {
                    continue;
                }
                let Ok(expires_at) = fields[2].parse() else {
                    continue;
                };
                if expires_at > now {
                    pending.push(PendingRegistration {
                        email: fields[0].to_string(),
                        verification_code: fields[1].to_string(),
                        expires_at,
                        name: fields[3].to_string(),
                        password_hash: fields[4].to_string(),
                    });
                }
            }
            Some("S") => {
                let fields: Vec<&str> = parts.collect();
                if fields.len() != 4 {
                    continue;
                }
                let (Ok(created_at), Ok(expires_at)) = (fields[2].parse(), fields[3].parse())
                else {
                    continue;
                };
                if expires_at > now {
                    sessions.insert(
                        fields[0].to_string(),
                        SessionInfo {
                            username: fields[1].to_string(),
                            created_at,
                            expires_at,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    (users, pending, sessions)
}

fn load_users(auth_dir: &Path) -> Vec<User> {
    let path = auth_dir.join("users.tsv");
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    data.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '\t');
            Some(User {
                name: parts.next()?.to_string(),
                email: parts.next()?.to_string(),
                password_hash: parts.next()?.to_string(),
                verified: parts.next()? == "true",
                created_at: parts.next()?.parse().ok()?,
            })
        })
        .collect()
}

fn load_pending(auth_dir: &Path) -> Vec<PendingRegistration> {
    let path = auth_dir.join("pending.tsv");
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let now = now_secs();
    data.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '\t');
            let reg = PendingRegistration {
                email: parts.next()?.to_string(),
                verification_code: parts.next()?.to_string(),
                expires_at: parts.next()?.parse().ok()?,
                name: parts.next()?.to_string(),
                password_hash: parts.next()?.to_string(),
            };
            // Skip expired entries on load
            if reg.expires_at > now {
                Some(reg)
            } else {
                None
            }
        })
        .collect()
}

fn load_sessions(auth_dir: &Path) -> HashMap<String, SessionInfo> {
    let path = auth_dir.join("sessions.tsv");
    let Ok(data) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let now = now_secs();
    data.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let token = parts.next()?.to_string();
            let info = SessionInfo {
                username: parts.next()?.to_string(),
                created_at: parts.next()?.parse().ok()?,
                expires_at: parts.next()?.parse().ok()?,
            };
            // Skip expired sessions on load
            if info.expires_at > now {
                Some((token, info))
            } else {
                None
            }
        })
        .collect()
}

/// Validate a public account handle.
pub(crate) fn validate_handle(handle: &str) -> bool {
    let len = handle.chars().count();
    (1..=30).contains(&len)
        && handle
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Validate email format and bound data written to the credential store.
pub(crate) fn validate_email(email: &str) -> bool {
    if email.len() > 254 || email.chars().any(char::is_control) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && domain.len() > 2
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_and_verify() {
        let hash = AuthSystem::hash_password("hunter2").unwrap();
        assert!(AuthSystem::verify_password("hunter2", &hash));
        assert!(!AuthSystem::verify_password("wrong", &hash));
    }

    #[test]
    fn account_fields_are_bounded_and_safe() {
        assert!(validate_handle("dust_user-2"));
        assert!(!validate_handle(""));
        assert!(!validate_handle("space user"));
        assert!(!validate_handle(&"a".repeat(31)));
        assert!(!validate_email(&format!("{}@example.com", "a".repeat(250))));
        assert!(!validate_email("a@@example.com"));
        assert!(!validate_email("a@.example"));
    }

    #[test]
    fn token_generation_unique() {
        let t1 = AuthSystem::generate_token();
        let t2 = AuthSystem::generate_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 64); // 32 bytes hex = 64 chars
    }

    #[test]
    fn verification_code_format() {
        let code = AuthSystem::generate_verification_code();
        assert_eq!(code.len(), 8);
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn email_validation() {
        assert!(validate_email("user@example.com"));
        assert!(validate_email("a@b.co"));
        assert!(!validate_email("noatsign"));
        assert!(!validate_email("@nodomain"));
        assert!(!validate_email("no@dot"));
        assert!(!validate_email(""));
    }

    #[test]
    fn user_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let site_root = dir.path();

        let mut auth = AuthSystem::load(site_root);
        assert!(auth.users.is_empty());

        // Register
        let code = auth
            .register("alice".into(), "alice@example.com".into(), "pass123")
            .unwrap();
        assert_eq!(code.len(), 8);
        assert!(auth.username_taken("alice"));
        assert!(auth.username_taken("Alice")); // case-insensitive

        // Verify
        let name = auth.verify(&code).unwrap();
        assert_eq!(name, "alice");

        // Authenticate
        assert!(auth.authenticate("alice", "pass123").is_ok());
        assert!(auth.authenticate("alice", "wrong").is_err());

        // Reload from disk
        let auth2 = AuthSystem::load(site_root);
        assert_eq!(auth2.users.len(), 1);
        assert_eq!(auth2.users[0].name, "alice");
        assert!(auth2.authenticate("alice", "pass123").is_ok());
    }

    #[test]
    fn session_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let site_root = dir.path();

        let mut auth = AuthSystem::load(site_root);
        let token = auth.create_session("bob").unwrap();
        assert_eq!(auth.resolve_session(&token), Some("bob".to_string()));

        // Reload from disk
        let mut auth2 = AuthSystem::load(site_root);
        assert_eq!(auth2.resolve_session(&token), Some("bob".to_string()));

        // Destroy
        auth2.destroy_session(&token).unwrap();
        assert_eq!(auth2.resolve_session(&token), None);
    }

    #[test]
    fn duplicate_username_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());

        let code = auth
            .register("alice".into(), "a@example.com".into(), "pass")
            .unwrap();
        auth.verify(&code).unwrap();

        let result = auth.register("Alice".into(), "b@example.com".into(), "pass");
        assert!(result.is_err());
    }

    #[test]
    fn wrong_code_does_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());

        auth.register("alice".into(), "a@example.com".into(), "pass")
            .unwrap();
        assert_eq!(auth.pending.len(), 1);

        // Wrong code should fail but not remove the pending entry
        assert!(auth.verify("WRONGCOD").is_err());
        assert_eq!(auth.pending.len(), 1, "pending entry should still exist");

        // Users list should still be empty
        assert!(auth.users.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auth_state_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());
        auth.create_session("alice").unwrap();

        let auth_dir = dir.path().join(".auth");
        assert_eq!(
            std::fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(auth_dir.join("state.tsv"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
        );
    }

    #[test]
    fn verification_attempts_are_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        for _ in 0..VERIFY_MAX_ATTEMPTS {
            assert!(auth.check_verify_rate(ip).is_ok());
            auth.record_verify_attempt(ip);
        }
        assert!(auth.check_verify_rate(ip).is_err());
    }

    #[test]
    fn sessions_are_bounded_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());
        for _ in 0..MAX_SESSIONS_PER_USER + 2 {
            auth.create_session("alice").unwrap();
        }
        assert_eq!(
            auth.sessions
                .values()
                .filter(|session| session.username == "alice")
                .count(),
            MAX_SESSIONS_PER_USER
        );
    }

    #[test]
    fn auth_entities_share_one_committed_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = AuthSystem::load(dir.path());
        let code = auth
            .register("alice".into(), "alice@example.com".into(), "pass123")
            .unwrap();
        auth.verify(&code).unwrap();
        auth.create_session("alice").unwrap();

        let auth_dir = dir.path().join(".auth");
        let state = std::fs::read_to_string(auth_dir.join("state.tsv")).unwrap();
        assert!(state.lines().any(|line| line.starts_with("U\talice\t")));
        assert!(state.lines().any(|line| line.starts_with("S\t")));
        assert!(!auth_dir.join("users.tsv").exists());
        assert!(!auth_dir.join("sessions.tsv").exists());
    }

    #[test]
    fn rate_limit_maps_prune_expired_addresses() {
        let mut limiter = AuthRateLimiter::new();
        let old_ip: IpAddr = "192.0.2.1".parse().unwrap();
        let new_ip: IpAddr = "192.0.2.2".parse().unwrap();
        limiter.login.insert(
            old_ip,
            (
                1,
                Instant::now() - std::time::Duration::from_secs(LOGIN_WINDOW_SECS + 1),
            ),
        );
        assert!(
            limiter.check_login(new_ip).is_ok(),
            "stale entries must be pruned during checks"
        );
        limiter.record_login(new_ip);
        assert!(!limiter.login.contains_key(&old_ip));
        assert!(limiter.login.contains_key(&new_ip));
    }
}
