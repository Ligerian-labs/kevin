//! A deterministic, dependency-free renderer for the judge prompt templates in
//! `crates/kevin-evaluator/prompts/`.
//!
//! Same two constructs as the orchestrator's role renderer
//! (`crates/kevin-orchestrator/src/roles/render.rs`), duplicated because
//! `kevin-evaluator` sits *below* `kevin-orchestrator` in the dependency
//! direction fixed by `plan/01-architecture.md` §Crate map:
//!
//! - `{{name}}` — replaced by the variable's value; an unknown name is left
//!   verbatim so a snapshot catches the mistake.
//! - a line containing only `{{#name}}` … a line containing only `{{/name}}` —
//!   the body is kept when `name` is set to a non-blank value. Sections do not
//!   nest.
//!
//! After substitution the result is trimmed and runs of blank lines collapse to
//! one, so dropping an optional section leaves no hole.

use std::collections::BTreeMap;

/// Variables handed to [`render`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// An empty variable set.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Sets `name`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.0.insert(name.into(), value.into());
        self
    }

    /// Sets `name` to the joined `lines`, or leaves it unset when `lines` is
    /// empty — which drops the enclosing section.
    pub fn set_lines<I, S>(&mut self, name: impl Into<String>, lines: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let joined = lines
            .into_iter()
            .map(|l| l.as_ref().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.trim().is_empty() {
            self.set(name, joined);
        }
        self
    }

    /// Sets `name` only when `value` is `Some` and non-blank.
    pub fn set_opt(
        &mut self,
        name: impl Into<String>,
        value: Option<impl Into<String>>,
    ) -> &mut Self {
        if let Some(value) = value {
            let value = value.into();
            if !value.trim().is_empty() {
                self.set(name, value);
            }
        }
        self
    }

    /// The value of `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// `true` when `name` is unset or blank (a section on it is dropped).
    #[must_use]
    pub fn is_blank(&self, name: &str) -> bool {
        self.get(name).is_none_or(|v| v.trim().is_empty())
    }
}

/// Renders `template` with `vars` (see the module docs for the syntax).
#[must_use]
pub fn render(template: &str, vars: &Vars) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping: Option<&str> = None;
    for line in template.lines() {
        let trimmed = line.trim();
        if let Some(name) = marker(trimmed, "{{#") {
            if skipping.is_none() && vars.is_blank(name) {
                skipping = Some(name);
            }
            continue;
        }
        if let Some(name) = marker(trimmed, "{{/") {
            if skipping == Some(name) {
                skipping = None;
            }
            continue;
        }
        if skipping.is_none() {
            kept.push(line);
        }
    }
    tidy(&substitute(&kept.join("\n"), vars))
}

/// The name inside a marker line (`{{#name}}` / `{{/name}}`), if this whole
/// line is one.
fn marker<'a>(line: &'a str, open: &str) -> Option<&'a str> {
    let name = line.strip_prefix(open)?.strip_suffix("}}")?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(name)
}

/// Replaces every `{{name}}` that has a value; unknown names stay verbatim.
fn substitute(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match vars.get(name) {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Trims the document and collapses runs of blank lines to a single one.
fn tidy(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if !blank && !out.is_empty() {
                out.push("");
            }
            blank = true;
        } else {
            out.push(line);
            blank = false;
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_known_names_and_keeps_unknown_ones() {
        let mut vars = Vars::new();
        vars.set("who", "the judge");
        assert_eq!(
            render("Hello {{who}}, meet {{nobody}}.", &vars),
            "Hello the judge, meet {{nobody}}."
        );
    }

    #[test]
    fn sections_are_kept_only_for_non_blank_variables() {
        let template = "head\n\n{{#body}}\n# Body\n\n{{body}}\n\n{{/body}}\ntail";
        let mut vars = Vars::new();
        assert_eq!(render(template, &vars), "head\n\ntail");
        vars.set("body", "text");
        assert_eq!(render(template, &vars), "head\n\n# Body\n\ntext\n\ntail");
    }
}
