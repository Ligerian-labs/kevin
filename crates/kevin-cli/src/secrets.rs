//! Registering Kevin's own secrets with the redaction layer at startup
//! (`plan/09-security.md` §Redaction, second bullet).
//!
//! The pattern list in `kevin-telemetry` catches *shapes* (`sk-ant-…`,
//! `ghp_…`, `Bearer …`). It cannot catch a Postgres password that happens to
//! look like a word, or a bearer token an operator wrote by hand. Those are
//! known exactly at startup, so [`register`] feeds them to
//! [`Redactor::register_secret`], which keeps only `(len, hash)` — the value is
//! never stored in clear — and masks the value wherever it later appears in a
//! log line, an event payload, a transcript or an API error.
//!
//! Called from [`crate::embedded::Backend::open_with`], so every process that
//! opens the database registers what it loaded, `kevin serve` included.

use kevin_config::KevinConfig;
use kevin_telemetry::redact::Redactor;

/// Registers every secret this configuration resolves to.
///
/// Missing or unreadable files are silently skipped: this is defence in depth,
/// not a validation step (`kevin config validate` owns that), and a failure
/// here must never stop a command from running.
pub fn register(config: &KevinConfig) {
    let redactor = Redactor::global();
    if let Some(password) = url_password(&config.database.url) {
        redactor.register_secret(password);
    }
    for path in [
        &config.server.auth_token_file,
        &config.kohral.token_file,
        &config.kohral.identity_file,
    ] {
        if path.as_os_str().is_empty() {
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                redactor.register_secret(trimmed);
            }
        }
    }
}

/// The password of a `scheme://user:password@host/…` URL, if it has one.
fn url_password(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?']).next()?;
    let credentials = authority.rsplit_once('@')?.0;
    let password = credentials.split_once(':')?.1;
    (!password.is_empty()).then_some(password)
}

#[cfg(test)]
mod tests {
    use super::url_password;

    #[test]
    fn url_password_is_extracted_only_when_present() {
        assert_eq!(
            url_password("postgres://kevin:hunter2@localhost:5433/kevin"),
            Some("hunter2")
        );
        assert_eq!(url_password("postgres://kevin@localhost/kevin"), None);
        assert_eq!(url_password("postgres://localhost/kevin"), None);
        assert_eq!(url_password("postgres://kevin:@localhost/kevin"), None);
        assert_eq!(url_password("not a url"), None);
        // An `@` in the path must not be read as an authority separator.
        assert_eq!(url_password("postgres://localhost/db@name"), None);
    }
}
