//! Secrets classification and content redaction.
//!
//! Pure helpers so crawl, checkpoint, and logging never retain recoverable
//! credentials. Fixtures must use **synthetic** secrets only.

use std::path::Path;

/// True when a relative path looks secret-bearing (basename / pattern rules).
///
/// Matches common env/key material: `.env`, `.env.*`, `*secret*`, `*credential*`,
/// `*.pem`, `*.key`, `id_rsa`, etc. Comparison is case-insensitive on the
/// final path component.
#[must_use]
pub fn is_sensitive_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    if lower.ends_with(".pem") || lower.ends_with(".key") || lower.ends_with(".p12") {
        return true;
    }
    if lower == "id_rsa" || lower == "id_ed25519" || lower.starts_with("id_rsa.") {
        return true;
    }
    if lower.contains("secret") || lower.contains("credential") || lower.contains("password") {
        return true;
    }
    if lower == "credentials.json" || lower == "service-account.json" {
        return true;
    }
    false
}

/// Same as [`is_sensitive_path`] for a [`Path`].
#[must_use]
pub fn path_is_sensitive(path: &Path) -> bool {
    path.to_str().is_some_and(is_sensitive_path)
}

/// Placeholder used when redacting secret-shaped values.
pub const REDACTED_PLACEHOLDER: &str = "****";

/// Redact common secret-shaped substrings in text for safe logging/checkpoints.
///
/// Heuristics (best-effort, not a security boundary):
/// - `KEY=value` lines for known key names
/// - Long hex/base64-looking tokens after `Bearer ` / `token=` / `api_key=`
#[must_use]
pub fn redact_content(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn redact_line(line: &str) -> String {
    let trimmed = line.trim_start();
    // KEY=value forms
    if let Some((key, _val)) = trimmed.split_once('=') {
        let k = key.trim().to_ascii_lowercase();
        if is_secret_key_name(&k) {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            return format!("{indent}{}={REDACTED_PLACEHOLDER}", key.trim());
        }
    }
    // Bearer tokens
    if let Some(idx) = find_ci(line, "bearer ") {
        let prefix = &line[..idx + "bearer ".len()];
        return format!("{prefix}{REDACTED_PLACEHOLDER}");
    }
    line.to_owned()
}

fn is_secret_key_name(k: &str) -> bool {
    k.contains("secret")
        || k.contains("password")
        || k.contains("token")
        || k.contains("api_key")
        || k.contains("apikey")
        || k.ends_with("_key")
        || k == "authorization"
        || k == "private_key"
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_ascii_lowercase().find(&needle.to_ascii_lowercase())
}

/// Content to store for a path: empty/redacted marker if sensitive path, else redacted body.
#[must_use]
pub fn content_for_checkpoint(path: &str, raw_utf8: &str) -> String {
    if is_sensitive_path(path) {
        format!("/* {REDACTED_PLACEHOLDER} sensitive path omitted */")
    } else {
        redact_content(raw_utf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_paths() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path("config/.env.local"));
        assert!(is_sensitive_path("certs/server.pem"));
        assert!(is_sensitive_path("my_secret_token.txt"));
        assert!(!is_sensitive_path("src/main.rs"));
        assert!(!is_sensitive_path("README.md"));
    }

    #[test]
    fn redact_env_lines() {
        let s = redact_content("API_KEY=super-secret\nname=ok\n");
        assert!(s.contains(&format!("API_KEY={REDACTED_PLACEHOLDER}")));
        assert!(s.contains("name=ok"));
        assert!(!s.contains("super-secret"));
    }

    #[test]
    fn redact_bearer() {
        let s = redact_content("Authorization: Bearer abcdef123456");
        assert!(s.contains(REDACTED_PLACEHOLDER));
        assert!(!s.contains("abcdef123456"));
    }

    #[test]
    fn content_for_sensitive_path_omits_body() {
        let out = content_for_checkpoint(".env", "SECRET=real");
        assert!(!out.contains("real"));
        assert!(out.contains(REDACTED_PLACEHOLDER));
    }

    // ------------------------------------------------------------------
    // Issue #230: id_rsa, credentials.json, service-account.json, non-UTF8
    // ------------------------------------------------------------------

    #[test]
    fn is_sensitive_path_id_rsa() {
        assert!(is_sensitive_path("config/id_rsa"));
        assert!(is_sensitive_path("id_rsa"));
        assert!(is_sensitive_path("ssh/id_rsa"));
    }

    #[test]
    fn is_sensitive_path_credentials_json() {
        assert!(is_sensitive_path("creds/credentials.json"));
        assert!(is_sensitive_path("credentials.json"));
        assert!(is_sensitive_path("config/credentials.json"));
    }

    #[test]
    fn is_sensitive_path_service_account_json() {
        assert!(is_sensitive_path("svc/service-account.json"));
        assert!(is_sensitive_path("service-account.json"));
        assert!(is_sensitive_path("gcp/service-account.json"));
    }

    #[test]
    fn is_sensitive_path_id_ed25519() {
        assert!(is_sensitive_path("ssh/id_ed25519"));
        assert!(is_sensitive_path("id_ed25519"));
    }

    #[test]
    #[cfg(unix)]
    fn path_is_sensitive_non_utf8_returns_false() {
        // A path with non-UTF-8 bytes: `to_str()` returns None, so
        // `path_is_sensitive` must return false (cannot classify).
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let non_utf8 = OsStr::from_bytes(b"\xFF\xFE/path");
        let path = Path::new(non_utf8);
        assert!(
            !path_is_sensitive(path),
            "non-UTF-8 path should return false (cannot classify)"
        );
    }

    #[test]
    fn content_for_checkpoint_normal_path_returns_redacted_body() {
        // A normal (non-sensitive) path should return the redacted body,
        // not the sensitive-path placeholder.
        let body = "API_KEY=secret123\nname=myproject\n";
        let out = content_for_checkpoint("src/config.rs", body);
        // The API_KEY line should be redacted.
        assert!(out.contains(&format!("API_KEY={REDACTED_PLACEHOLDER}")));
        assert!(!out.contains("secret123"));
        // The name line should be preserved.
        assert!(out.contains("name=myproject"));
        // It should NOT contain the sensitive-path placeholder marker.
        assert!(!out.contains("sensitive path omitted"));
    }

    #[test]
    fn content_for_checkpoint_normal_path_no_secrets_passes_through() {
        let body = "# My Project\n\nA simple project.\n";
        let out = content_for_checkpoint("README.md", body);
        assert_eq!(out, body, "normal path with no secrets should pass through");
    }
}
