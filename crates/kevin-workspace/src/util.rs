//! Small helpers shared by the workspace manager and the integrator.

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Number of hex characters kept in a "short" id.
pub const SHORT_LEN: usize = 8;

/// Short form of an id used in paths and branch names: the **last** 8 hex
/// characters of the uuid. For v7 ids the leading characters are the
/// timestamp (identical for ids minted within the same minute), the trailing
/// ones are random, so the tail is the discriminating part.
pub fn short_id(id: impl Into<Uuid>) -> String {
    let s = id.into().simple().to_string();
    s[s.len() - SHORT_LEN..].to_owned()
}

/// Lower-case `[a-z0-9-]` slug of a title, at most `max` chars, never empty
/// when `input` contains at least one alphanumeric character.
pub fn slugify(input: &str, max: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = true;
    for c in input.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= max {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Joins `rel` onto `base` unless `rel` is absolute.
pub fn join_or_absolute(base: &Path, rel: &Path) -> PathBuf {
    if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    }
}

/// `true` when `child` is lexically inside `parent` (both should be canonical).
pub fn is_within(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent) && child != parent
}

/// Canonicalises when the path exists; otherwise canonicalises the deepest
/// existing ancestor and re-appends the rest (so not-yet-created workspace
/// directories still compare against a canonical root).
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(base) = cur.canonicalize() {
            let mut out = base;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_tail_of_simple_form() {
        let id = Uuid::parse_str("01910000-0000-7000-8000-0000deadbeef").unwrap();
        assert_eq!(short_id(id), "deadbeef");
    }

    #[test]
    fn slugify_collapses_and_bounds() {
        assert_eq!(
            slugify("Implement the API!!  v2", 40),
            "implement-the-api-v2"
        );
        assert_eq!(slugify("   ", 40), "");
        assert_eq!(slugify("abcdefghij", 4), "abcd");
        assert_eq!(slugify("ab--cd", 40), "ab-cd");
    }

    #[test]
    fn canonicalize_lenient_handles_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let missing = dir.path().join("a").join("b");
        assert_eq!(canonicalize_lenient(&missing), real.join("a").join("b"));
        assert!(is_within(&real, &canonicalize_lenient(&missing)));
        assert!(!is_within(&real, &real));
    }
}
