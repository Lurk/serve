use crate::errors::ServeError;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use subtle::ConstantTimeEq;

#[must_use]
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Hash `password` using Argon2id.
///
/// # Errors
/// Returns error if argon2 hashing fails.
pub fn hash_password(password: &str) -> Result<String, ServeError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| ServeError::Stats(format!("argon2 hash failed: {e}")))?
        .to_string();
    Ok(hash)
}

/// Verify `password` against a stored PHC-format `stored_hash`.
///
/// # Errors
/// Returns error if the stored hash is malformed.
pub fn verify_password(password: &str, stored_hash: &str) -> Result<bool, ServeError> {
    let parsed = PasswordHash::new(stored_hash)
        .map_err(|e| ServeError::Stats(format!("argon2 parse failed: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[derive(Default)]
pub struct AuthMetrics {
    failed_logins_since_startup: AtomicU64,
    failed_setup_token_attempts_since_startup: AtomicU64,
    last_failed_login_ts: AtomicI64,
}

impl AuthMetrics {
    pub fn record_failed_login(&self, now: i64) {
        self.failed_logins_since_startup
            .fetch_add(1, Ordering::Relaxed);
        self.last_failed_login_ts.store(now, Ordering::Relaxed);
    }
    pub fn record_failed_setup_token(&self) {
        self.failed_setup_token_attempts_since_startup
            .fetch_add(1, Ordering::Relaxed);
    }
    #[must_use]
    pub fn failed_logins(&self) -> u64 {
        self.failed_logins_since_startup.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn failed_setup_tokens(&self) -> u64 {
        self.failed_setup_token_attempts_since_startup
            .load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn last_failed_login_ts(&self) -> Option<i64> {
        let v = self.last_failed_login_ts.load(Ordering::Relaxed);
        if v == 0 { None } else { Some(v) }
    }
}

#[derive(Clone, Default)]
pub struct SetupTokenState {
    inner: Arc<RwLock<Option<String>>>,
}

impl SetupTokenState {
    #[must_use]
    pub fn new_initialised() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Some(random_token()))),
        }
    }
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }
    /// Returns the current setup token if one is set.
    ///
    /// # Panics
    /// Panics if the setup token `RwLock` was poisoned.
    #[must_use]
    pub fn read(&self) -> Option<String> {
        self.inner.read().expect("setup token poisoned").clone()
    }
    /// Clears the setup token.
    ///
    /// # Panics
    /// Panics if the setup token `RwLock` was poisoned.
    pub fn clear(&self) {
        *self.inner.write().expect("setup token poisoned") = None;
    }
    /// Returns `true` if a setup token is currently set.
    ///
    /// # Panics
    /// Panics if the setup token `RwLock` was poisoned.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.inner.read().expect("setup token poisoned").is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let hash = hash_password("hunter2").unwrap();
        assert!(!verify_password("hunter3", &hash).unwrap());
    }

    #[test]
    fn hash_is_phc_format() {
        let hash = hash_password("x").unwrap();
        assert!(hash.starts_with("$argon2"));
    }

    #[test]
    fn random_token_is_43_chars_base64_url() {
        // 32 bytes base64-url-no-pad = ceil(32 * 4 / 3) = 43 characters.
        let tok = random_token();
        assert_eq!(tok.len(), 43);
        assert!(
            tok.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn random_tokens_are_unique() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn auth_metrics_record_and_read() {
        let m = AuthMetrics::default();
        assert_eq!(m.failed_logins(), 0);
        assert_eq!(m.last_failed_login_ts(), None);
        m.record_failed_login(1000);
        assert_eq!(m.failed_logins(), 1);
        assert_eq!(m.last_failed_login_ts(), Some(1000));
        m.record_failed_login(2000);
        assert_eq!(m.failed_logins(), 2);
        assert_eq!(m.last_failed_login_ts(), Some(2000));
        m.record_failed_setup_token();
        assert_eq!(m.failed_setup_tokens(), 1);
    }

    #[test]
    fn setup_token_lifecycle() {
        let s = SetupTokenState::new_initialised();
        assert!(s.is_set());
        let tok = s.read().unwrap();
        assert_eq!(tok.len(), 43);
        s.clear();
        assert!(!s.is_set());
        assert!(s.read().is_none());
    }

    #[test]
    fn setup_token_empty() {
        let s = SetupTokenState::empty();
        assert!(!s.is_set());
        assert!(s.read().is_none());
    }
}
