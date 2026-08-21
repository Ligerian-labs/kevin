//! Well-known paths (`$XDG_CONFIG_HOME/kevin`, `~`) resolved from an explicit
//! environment so tests never touch the real home directory.

use std::path::{Path, PathBuf};

/// Looks `name` up in an environment list (last occurrence wins, like a shell).
#[must_use]
pub fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// `$HOME`, when set and non-empty.
#[must_use]
pub fn home_dir(env: &[(String, String)]) -> Option<PathBuf> {
    env_value(env, "HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME/kevin`, falling back to `~/.config/kevin` (and to a
/// relative `.config/kevin` when `$HOME` is unset).
#[must_use]
pub fn user_config_dir(env: &[(String, String)]) -> PathBuf {
    let base = env_value(env, "XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map_or_else(
            || home_dir(env).unwrap_or_default().join(".config"),
            PathBuf::from,
        );
    base.join("kevin")
}

/// The user config file: `<user_config_dir>/kevin.toml`.
#[must_use]
pub fn user_config_file(env: &[(String, String)]) -> PathBuf {
    user_config_dir(env).join("kevin.toml")
}

/// Expands a leading `~` / `~/…` with `home`; other paths are returned as-is.
#[must_use]
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn xdg_config_home_wins_over_home() {
        let e = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/xdg")]);
        assert_eq!(user_config_file(&e), PathBuf::from("/xdg/kevin/kevin.toml"));
        let e = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "")]);
        assert_eq!(
            user_config_file(&e),
            PathBuf::from("/home/u/.config/kevin/kevin.toml")
        );
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(
            expand_home(Path::new("~/.config/kevin/token"), Some(home)),
            PathBuf::from("/home/u/.config/kevin/token")
        );
        assert_eq!(
            expand_home(Path::new("~"), Some(home)),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            expand_home(Path::new("/abs/~/x"), Some(home)),
            PathBuf::from("/abs/~/x")
        );
        assert_eq!(
            expand_home(Path::new("~/x"), None),
            PathBuf::from("~/x"),
            "no home → unchanged"
        );
    }
}
