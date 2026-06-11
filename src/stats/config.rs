use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_URL_PREFIX: &str = "/__stats__";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StatsConfig {
    pub db_path: PathBuf,
    #[serde(default = "default_session_ttl")]
    pub session_ttl_days: u32,
    /// Force the `Secure` cookie attribute on/off. When `None`, falls back
    /// to whether the server was started with the `tls` subcommand. Set this
    /// to `true` if TLS terminates upstream (e.g. behind a reverse proxy).
    #[serde(default)]
    pub secure_cookies: Option<bool>,
    /// URL prefix the stats UI and API mount under. Defaults to `/__stats__`.
    /// Must start with `/`, must not end with `/`, must not contain `//` or `..`.
    #[serde(default)]
    pub url_prefix: Option<String>,
    /// Path to a `GeoIP` `.mmdb` country database. When set and readable, the
    /// dashboard gains a per-country breakdown. Any MaxMind-format country DB
    /// works (`MaxMind` `GeoLite2`, `DB-IP` Lite, `IPinfo`). When `None`, country
    /// stats are disabled.
    #[serde(default)]
    pub geoip_db_path: Option<PathBuf>,
    /// When `true`, geolocate the right-most `X-Forwarded-For` entry instead of
    /// the socket peer IP. Enable this only when a trusted proxy sets the
    /// header — otherwise clients can spoof their country. Defaults to `false`.
    #[serde(default)]
    pub trust_forwarded_for: bool,
}

const fn default_session_ttl() -> u32 {
    30
}

/// Validate a configured URL prefix. Returns the validated prefix or an error
/// describing what's wrong with it.
///
/// # Errors
/// Returns error if the prefix is empty, doesn't start with `/`, ends with `/`,
/// or contains `//` or `..`.
pub fn validate_url_prefix(prefix: &str) -> Result<&str, String> {
    if prefix.len() < 2 {
        return Err(format!("url_prefix must be non-empty: {prefix:?}"));
    }
    if !prefix.starts_with('/') {
        return Err(format!("url_prefix must start with '/': {prefix:?}"));
    }
    if prefix.ends_with('/') {
        return Err(format!("url_prefix must not end with '/': {prefix:?}"));
    }
    if prefix.contains("//") || prefix.contains("..") {
        return Err(format!(
            "url_prefix must not contain '//' or '..': {prefix:?}"
        ));
    }
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_explicit_ttl() {
        let cfg: StatsConfig = toml::from_str(r#"db_path = "/tmp/x.db""#).unwrap();
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/x.db"));
        assert_eq!(cfg.session_ttl_days, 30);

        let cfg: StatsConfig = toml::from_str(
            r#"
            db_path = "/tmp/x.db"
            session_ttl_days = 60
            "#,
        )
        .unwrap();
        assert_eq!(cfg.session_ttl_days, 60);
    }

    #[test]
    fn parses_secure_cookies_override() {
        let toml = r#"
            db_path = "/tmp/x.db"
            secure_cookies = true
        "#;
        let cfg: StatsConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.secure_cookies, Some(true));
    }

    #[test]
    fn parses_url_prefix_override() {
        let toml = r#"
            db_path = "/tmp/x.db"
            url_prefix = "/admin/stats"
        "#;
        let cfg: StatsConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.url_prefix.as_deref(), Some("/admin/stats"));
    }

    #[test]
    fn parses_geoip_and_trust_forwarded_for() {
        let toml = r#"
            db_path = "/tmp/x.db"
            geoip_db_path = "/etc/serve/GeoLite2-Country.mmdb"
            trust_forwarded_for = true
        "#;
        let cfg: StatsConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.geoip_db_path,
            Some(PathBuf::from("/etc/serve/GeoLite2-Country.mmdb"))
        );
        assert!(cfg.trust_forwarded_for);
    }

    #[test]
    fn geoip_defaults_absent_and_trust_false() {
        let cfg: StatsConfig = toml::from_str(r#"db_path = "/tmp/x.db""#).unwrap();
        assert_eq!(cfg.geoip_db_path, None);
        assert!(!cfg.trust_forwarded_for);
    }

    #[test]
    fn validate_url_prefix_accepts_good() {
        assert!(validate_url_prefix("/__stats__").is_ok());
        assert!(validate_url_prefix("/admin/stats").is_ok());
        assert!(validate_url_prefix("/x").is_ok());
    }

    #[test]
    fn validate_url_prefix_rejects_bad() {
        assert!(validate_url_prefix("").is_err());
        assert!(validate_url_prefix("/").is_err());
        assert!(validate_url_prefix("admin").is_err());
        assert!(validate_url_prefix("/admin/").is_err());
        assert!(validate_url_prefix("/admin//x").is_err());
        assert!(validate_url_prefix("/../etc").is_err());
    }
}
