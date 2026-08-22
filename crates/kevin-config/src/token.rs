//! API bearer token files: 32 random bytes, base64url (no padding), written
//! atomically with mode `0600` (`plan/09-security.md` §API authentication).

use std::io;
use std::path::Path;

/// Random bytes per token.
pub const TOKEN_BYTES: usize = 32;

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes `bytes` as unpadded base64url.
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        let idx = |shift: u32| B64URL[((n >> shift) & 0x3f) as usize] as char;
        out.push(idx(18));
        out.push(idx(12));
        if chunk.len() > 1 {
            out.push(idx(6));
        }
        if chunk.len() > 2 {
            out.push(idx(0));
        }
    }
    out
}

/// A fresh token: [`TOKEN_BYTES`] random bytes, base64url-encoded.
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::fill(&mut bytes);
    base64url(&bytes)
}

/// Writes `token` (plus a trailing newline) to `path` atomically: parent
/// directories are created, the file is written next to the target and renamed
/// over it, with mode `0600` on Unix.
pub fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        io::Write::write_all(&mut file, token.as_bytes())?;
        io::Write::write_all(&mut file, b"\n")?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Reads and trims a token file.
pub fn read_token_file(path: &Path) -> io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_known_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn tokens_are_43_chars_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn write_creates_parents_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/token");
        write_token_file(&path, "abc").unwrap();
        assert_eq!(read_token_file(&path).unwrap(), "abc");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        write_token_file(&path, "def").unwrap();
        assert_eq!(read_token_file(&path).unwrap(), "def");
    }
}

// ---------------------------------------------------------------------------
// Startup check for a non-loopback bind
// ---------------------------------------------------------------------------

/// Refuses to serve a non-loopback bind unless a token file **exists with mode
/// `0600`** (`plan/09-security.md` §API authentication and exposure).
///
/// This is a *startup* check, not a load-time one: `load()` stays a pure
/// function of the configuration layers, and the file may legitimately be
/// created between `kevin config show` and `kevin serve`. A configured but
/// missing or world-readable token file is worse than no configuration at all
/// — the operator believes the port is protected — so it is an error, not a
/// warning.
///
/// Loopback binds return `Ok` without touching the filesystem.
///
/// # Errors
/// [`ConfigError::InsecureBind`] with the reason the bind cannot be protected.
pub fn check_bind_security(config: &crate::KevinConfig) -> Result<(), crate::ConfigError> {
    if config.server.bind.ip().is_loopback() {
        return Ok(());
    }
    let api = token_state(&config.server.auth_token_file);
    let kohral = if config.kevin.profile == crate::Profile::Kohral || config.kohral.enabled {
        token_state(&config.kohral.token_file)
    } else {
        TokenState::Unset
    };
    // Either surface may carry the credential, so one usable token file is
    // enough; the reason reported is the more specific of the two.
    if matches!(api, TokenState::Ok) || matches!(kohral, TokenState::Ok) {
        return Ok(());
    }
    let reason = match (&api, &kohral) {
        (TokenState::Problem(reason), _) | (_, TokenState::Problem(reason)) => reason.clone(),
        _ => "no token file is configured".to_owned(),
    };
    Err(crate::ConfigError::InsecureBind {
        bind: config.server.bind.to_string(),
        layer: crate::Source::Default,
        reason,
    })
}

/// Whether a token file can actually protect a non-loopback bind.
enum TokenState {
    /// No path configured.
    Unset,
    /// A path is configured but unusable; the string says why.
    Problem(String),
    /// Exists, non-empty, and (on unix) mode `0600`.
    Ok,
}

fn token_state(path: &Path) -> TokenState {
    if path.as_os_str().is_empty() {
        return TokenState::Unset;
    }
    let display = path.display();
    let Ok(meta) = std::fs::metadata(path) else {
        return TokenState::Problem(format!("token file {display} does not exist"));
    };
    if !meta.is_file() {
        return TokenState::Problem(format!("token file {display} is not a regular file"));
    }
    if meta.len() == 0 {
        return TokenState::Problem(format!("token file {display} is empty"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return TokenState::Problem(format!(
                "token file {display} has mode {mode:04o}, expected 0600"
            ));
        }
    }
    TokenState::Ok
}
