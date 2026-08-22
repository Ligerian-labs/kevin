//! The platform briefing (`plan/08-kohral-runtime.md` §5.1).
//!
//! Kohral mounts the agent's identity as read-only files in
//! `/opt/kevin/config/`: `AGENTS.md` (the mission written by the anamnesis
//! role), `SOUL.md` (persona, with a `## Kohral` section Kohral appends
//! itself) and `KOHRAL_DOCUMENTATION.md` (how the platform works). At boot
//! Kevin reads them once and registers a
//! [`SystemContextProvider`] so every role prompt starts with the same
//! briefing, in this order:
//!
//! 1. `AGENTS.md` — the mission,
//! 2. `SOUL.md` — persona **and** the `## Kohral` section, verbatim,
//! 3. a one-line pointer to `KOHRAL_DOCUMENTATION.md` (never the file: it is
//!    long, and a worker can read it from disk when it needs it).
//!
//! Kohral scans these files for prompt-injection patterns and replaces a whole
//! file with `[BLOCKED: …]`. A blocked file must be treated as **missing**:
//! [`kevin_orchestrator::roles::SystemContextSection::new`] drops it and logs a
//! warning, so nothing that Kohral refused ever reaches a model.
//!
//! Seeding `MEMORY.md` is deliberately **not** done here: the memory file
//! belongs to the agent, so the image entrypoint (WS-23) creates it once when
//! it is absent and Kevin never rewrites it.

use std::path::{Path, PathBuf};

use kevin_config::schema::Kohral;
use kevin_orchestrator::roles::{StaticSystemContext, SystemContextProvider};

/// Name reported by the provider in logs.
pub const PROVIDER_NAME: &str = "kohral";

/// Where the mission lives, relative to the directory holding `SOUL.md`.
const AGENTS_FILE: &str = "AGENTS.md";

/// The files a Kohral briefing is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefingFiles {
    /// `/opt/kevin/config/AGENTS.md`.
    pub agents: PathBuf,
    /// `kohral.soul_file`.
    pub soul: PathBuf,
    /// `kohral.documentation_file`.
    pub documentation: PathBuf,
}

impl BriefingFiles {
    /// The paths implied by a `[kohral]` section: `AGENTS.md` sits next to
    /// `SOUL.md` in the read-only config volume.
    #[must_use]
    pub fn from_config(kohral: &Kohral) -> Self {
        let agents = kohral
            .soul_file
            .parent()
            .map_or_else(|| PathBuf::from(AGENTS_FILE), |dir| dir.join(AGENTS_FILE));
        Self {
            agents,
            soul: kohral.soul_file.clone(),
            documentation: kohral.documentation_file.clone(),
        }
    }
}

/// Reads the briefing files and builds the provider the orchestrator prepends
/// to every role prompt. Missing, empty and `[BLOCKED …]` files are skipped.
#[must_use]
pub fn load(files: &BriefingFiles) -> StaticSystemContext {
    let mut sections: Vec<(String, String)> = Vec::new();
    if let Some(mission) = read(&files.agents) {
        sections.push(("AGENTS.md — mission".to_owned(), mission));
    }
    if let Some(soul) = read(&files.soul) {
        sections.push(("SOUL.md — who you are".to_owned(), soul));
    }
    if let Some(pointer) = documentation_pointer(&files.documentation) {
        sections.push(("Kohral platform".to_owned(), pointer));
    }
    StaticSystemContext::new(PROVIDER_NAME, sections)
}

/// [`load`], boxed as the trait object [`kevin_orchestrator::orchestrator::Deps`]
/// takes.
#[must_use]
pub fn provider(files: &BriefingFiles) -> std::sync::Arc<dyn SystemContextProvider> {
    std::sync::Arc::new(load(files))
}

/// One line telling the agent where the platform documentation is — the file
/// itself is far too long to prepend to every prompt.
fn documentation_pointer(path: &Path) -> Option<String> {
    let body = read(path)?;
    // A blank file, or one Kohral replaced with `[BLOCKED …]`, means the agent
    // has no platform documentation — do not point at something unusable.
    if body.trim().is_empty() || body.trim_start().starts_with("[BLOCKED") {
        return None;
    }
    let title = body
        .lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Kohral platform documentation");
    Some(format!(
        "You run inside Kohral. `{}` ({title}) documents the platform: read it \
         from disk when you need to know how Kohral deploys, configures or \
         observes you. Do not assume its contents.",
        path.display()
    ))
}

/// Reads a briefing file, or `None` when it is absent or unreadable.
fn read(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(body) => Some(body),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not read a Kohral briefing file; continuing without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kevin_orchestrator::roles::SystemContextProvider;

    use super::{BriefingFiles, PROVIDER_NAME, load};

    fn files(dir: &std::path::Path) -> BriefingFiles {
        BriefingFiles {
            agents: dir.join("AGENTS.md"),
            soul: dir.join("SOUL.md"),
            documentation: dir.join("KOHRAL_DOCUMENTATION.md"),
        }
    }

    #[test]
    fn the_sections_come_in_the_documented_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("AGENTS.md"), "Ship the thing.").expect("write");
        fs::write(
            dir.path().join("SOUL.md"),
            "You are Kevin.\n\n## Kohral\n\nBe terse.",
        )
        .expect("write");
        fs::write(
            dir.path().join("KOHRAL_DOCUMENTATION.md"),
            "# Kohral for agents\n\nlong text",
        )
        .expect("write");

        let provider = load(&files(dir.path()));
        assert_eq!(provider.name(), PROVIDER_NAME);
        let sections = provider.sections();
        assert_eq!(sections.len(), 3);
        assert!(sections[0].title.starts_with("AGENTS.md"));
        assert_eq!(sections[0].body, "Ship the thing.");
        assert!(sections[1].title.starts_with("SOUL.md"));
        assert!(
            sections[1].body.contains("## Kohral"),
            "the Kohral section is passed through verbatim"
        );
        assert!(
            sections[2].body.contains("KOHRAL_DOCUMENTATION.md"),
            "the documentation is referenced, not inlined"
        );
        assert!(
            !sections[2].body.contains("long text"),
            "the documentation body must not be inlined: {}",
            sections[2].body
        );
        assert!(sections[2].body.contains("Kohral for agents"));
    }

    #[test]
    fn missing_and_blocked_files_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("SOUL.md"),
            "[BLOCKED: prompt injection detected]",
        )
        .expect("write");
        fs::write(dir.path().join("KOHRAL_DOCUMENTATION.md"), "   ").expect("write");

        let provider = load(&files(dir.path()));
        assert!(
            provider.sections().is_empty(),
            "a blocked SOUL.md, an empty documentation file and a missing \
             AGENTS.md must all be treated as absent"
        );
    }

    #[test]
    fn the_paths_follow_the_kohral_config_section() {
        let kohral = kevin_config::schema::Kohral::default();
        let files = BriefingFiles::from_config(&kohral);
        assert_eq!(files.soul, kohral.soul_file);
        assert_eq!(files.documentation, kohral.documentation_file);
        assert_eq!(
            files.agents,
            std::path::PathBuf::from("/opt/kevin/config/AGENTS.md")
        );
    }
}
