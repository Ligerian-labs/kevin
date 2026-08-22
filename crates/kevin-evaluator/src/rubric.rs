//! Rubrics (`plan/06-memory-and-learning.md` §3.1).
//!
//! Rubrics are TOML documents. The four built-ins (`default`, `code`,
//! `research`, `writing`) are embedded with `include_str!`; `evaluation.rubric`
//! either names one of them or is a path to a TOML file that overrides it.
//!
//! Weights must sum to `1.0` (± [`WEIGHT_EPSILON`]) — validated at load, so a
//! rubric that cannot produce a `0..=1` overall never reaches a judge.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kevin_domain::TaskKind;
use serde::{Deserialize, Serialize};

/// `default` as TOML text.
pub const DEFAULT_TOML: &str = include_str!("../rubrics/default.toml");
/// `code` as TOML text.
pub const CODE_TOML: &str = include_str!("../rubrics/code.toml");
/// `research` as TOML text.
pub const RESEARCH_TOML: &str = include_str!("../rubrics/research.toml");
/// `writing` as TOML text.
pub const WRITING_TOML: &str = include_str!("../rubrics/writing.toml");

/// Every built-in rubric, `(id, TOML)`, in a stable order.
pub const BUILTINS: [(&str, &str); 4] = [
    ("default", DEFAULT_TOML),
    ("code", CODE_TOML),
    ("research", RESEARCH_TOML),
    ("writing", WRITING_TOML),
];

/// How far the sum of the weights may be from `1.0`.
pub const WEIGHT_EPSILON: f64 = 1e-6;

/// One rubric criterion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    /// Stable key the judge scores (`correctness`, `test_coverage`, …).
    pub key: String,
    /// Share of the overall score, in `0..=1`.
    pub weight: f64,
    /// What the criterion means, shown to the judge verbatim.
    #[serde(default)]
    pub description: String,
}

/// A scoring rubric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rubric {
    /// Rubric id recorded on the evaluation.
    pub id: String,
    /// One line for humans.
    #[serde(default)]
    pub description: String,
    /// The criteria, in the order the judge sees them.
    pub criteria: Vec<Criterion>,
}

impl Rubric {
    /// Parses and validates a rubric from TOML text.
    pub fn parse(toml_text: &str) -> Result<Self, RubricError> {
        let rubric: Rubric = toml::from_str(toml_text).map_err(|e| RubricError::Parse {
            source: Box::new(e),
        })?;
        rubric.validate()?;
        Ok(rubric)
    }

    /// A built-in rubric by id, or `None`.
    #[must_use]
    pub fn builtin(id: &str) -> Option<Rubric> {
        BUILTINS.iter().find(|(name, _)| *name == id).map(|(_, t)| {
            Rubric::parse(t).unwrap_or_else(|e| panic!("built-in rubric `{id}` is invalid: {e}"))
        })
    }

    /// Loads a rubric from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RubricError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| RubricError::Read {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        Rubric::parse(&text)
    }

    /// Resolves `evaluation.rubric`: a built-in id, else a path to a TOML file.
    pub fn resolve(spec: &str) -> Result<Self, RubricError> {
        if let Some(rubric) = Rubric::builtin(spec) {
            return Ok(rubric);
        }
        if spec.contains(['/', '\\', '.']) {
            return Rubric::load(spec);
        }
        Err(RubricError::Unknown {
            id: spec.to_owned(),
        })
    }

    /// Checks the invariants: non-empty id, at least one criterion, unique
    /// keys, weights in `0..=1` summing to `1.0`.
    pub fn validate(&self) -> Result<(), RubricError> {
        if self.id.trim().is_empty() {
            return Err(RubricError::Invalid {
                id: self.id.clone(),
                reason: "`id` must not be empty".to_owned(),
            });
        }
        if self.criteria.is_empty() {
            return Err(RubricError::Invalid {
                id: self.id.clone(),
                reason: "a rubric needs at least one criterion".to_owned(),
            });
        }
        let mut seen = BTreeSet::new();
        for criterion in &self.criteria {
            if criterion.key.trim().is_empty() {
                return Err(RubricError::Invalid {
                    id: self.id.clone(),
                    reason: "a criterion key must not be empty".to_owned(),
                });
            }
            if !seen.insert(criterion.key.as_str()) {
                return Err(RubricError::Invalid {
                    id: self.id.clone(),
                    reason: format!("duplicate criterion `{}`", criterion.key),
                });
            }
            if !(0.0..=1.0).contains(&criterion.weight) {
                return Err(RubricError::Invalid {
                    id: self.id.clone(),
                    reason: format!(
                        "criterion `{}` has weight {} outside 0..=1",
                        criterion.key, criterion.weight
                    ),
                });
            }
        }
        let sum = self.weight_sum();
        if (sum - 1.0).abs() > WEIGHT_EPSILON {
            return Err(RubricError::WeightSum {
                id: self.id.clone(),
                sum,
            });
        }
        Ok(())
    }

    /// Sum of every weight (exactly `1.0` for a valid rubric).
    #[must_use]
    pub fn weight_sum(&self) -> f64 {
        self.criteria.iter().map(|c| c.weight).sum()
    }

    /// The criterion keys, in rubric order.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.criteria.iter().map(|c| c.key.as_str()).collect()
    }

    /// The criterion with this key.
    #[must_use]
    pub fn criterion(&self, key: &str) -> Option<&Criterion> {
        self.criteria.iter().find(|c| c.key == key)
    }

    /// `Σ weight_i * score_i / 10`, clamped to `0..=1`
    /// (`plan/06-memory-and-learning.md` §3.2). Scores are looked up by
    /// criterion key; a criterion nobody scored counts as `0`.
    #[must_use]
    pub fn overall(&self, scores: &[(String, u8)]) -> f32 {
        let sum: f64 = self
            .criteria
            .iter()
            .map(|criterion| {
                let score = scores
                    .iter()
                    .find(|(key, _)| *key == criterion.key)
                    .map_or(0, |(_, score)| *score);
                criterion.weight * f64::from(score.min(10)) / 10.0
            })
            .sum();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the sum is in 0..=1; f32 is the domain's score type"
        )]
        let overall = sum.clamp(0.0, 1.0) as f32;
        overall
    }

    /// Markdown bullet list of the criteria for the judge prompt.
    #[must_use]
    pub fn as_prompt_block(&self) -> String {
        self.criteria
            .iter()
            .map(|c| {
                format!(
                    "- `{}` (weight {:.2}) — {}",
                    c.key,
                    c.weight,
                    c.description.trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The rubric a subject is judged with (`plan/06-memory-and-learning.md` §3.1):
/// task kind `implement|test|refactor|debug` → `code`, `research` → `research`,
/// `write` → `writing`, anything else → the configured rubric.
#[must_use]
pub fn builtin_for_kind(kind: &TaskKind) -> Option<&'static str> {
    match kind {
        TaskKind::Implement | TaskKind::Test | TaskKind::Refactor | TaskKind::Debug => Some("code"),
        TaskKind::Research => Some("research"),
        TaskKind::Write => Some("writing"),
        _ => None,
    }
}

/// Resolves the rubric for a task kind, falling back to `configured`
/// (`evaluation.rubric`) for kinds without a specialised rubric.
pub fn for_kind(kind: Option<&TaskKind>, configured: &str) -> Result<Rubric, RubricError> {
    match kind.and_then(builtin_for_kind) {
        Some(id) => Ok(Rubric::builtin(id).expect("built-in rubric")),
        None => Rubric::resolve(configured),
    }
}

/// Why a rubric could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum RubricError {
    /// The file could not be read.
    #[error("cannot read rubric {path}: {reason}")]
    Read {
        /// The path.
        path: PathBuf,
        /// The OS error.
        reason: String,
    },
    /// The TOML is malformed or has unknown keys.
    #[error("invalid rubric TOML: {source}")]
    Parse {
        /// The `toml` error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// A structural invariant was broken.
    #[error("rubric `{id}`: {reason}")]
    Invalid {
        /// Rubric id.
        id: String,
        /// What is wrong.
        reason: String,
    },
    /// The weights do not sum to 1.
    #[error("rubric `{id}`: weights sum to {sum}, expected 1.0")]
    WeightSum {
        /// Rubric id.
        id: String,
        /// The actual sum.
        sum: f64,
    },
    /// `evaluation.rubric` names neither a built-in nor a path.
    #[error("unknown rubric `{id}` (built-ins: default, code, research, writing)")]
    Unknown {
        /// What was asked for.
        id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses_and_its_weights_sum_to_one() {
        for (id, _) in BUILTINS {
            let rubric = Rubric::builtin(id).expect("built-in");
            assert_eq!(rubric.id, id);
            assert!((rubric.weight_sum() - 1.0).abs() <= WEIGHT_EPSILON, "{id}");
            assert!(rubric.criteria.iter().all(|c| !c.description.is_empty()));
        }
    }

    #[test]
    fn the_documented_criteria_are_the_ones_shipped() {
        assert_eq!(
            Rubric::builtin("default").unwrap().keys(),
            [
                "correctness",
                "completeness",
                "quality",
                "safety",
                "efficiency"
            ]
        );
        assert_eq!(
            Rubric::builtin("code").unwrap().keys(),
            [
                "correctness",
                "completeness",
                "code_quality",
                "test_coverage",
                "safety",
                "efficiency"
            ]
        );
        assert_eq!(
            Rubric::builtin("research").unwrap().keys(),
            ["accuracy", "coverage", "sourcing", "clarity", "efficiency"]
        );
        assert_eq!(
            Rubric::builtin("writing").unwrap().keys(),
            ["fit_to_brief", "clarity", "structure", "tone", "efficiency"]
        );
    }

    #[test]
    fn weights_that_do_not_sum_to_one_are_rejected() {
        let bad = "id = \"bad\"\n[[criteria]]\nkey = \"a\"\nweight = 0.5\n[[criteria]]\nkey = \"b\"\nweight = 0.2\n";
        assert!(matches!(
            Rubric::parse(bad),
            Err(RubricError::WeightSum { sum, .. }) if (sum - 0.7).abs() < 1e-9
        ));
    }

    #[test]
    fn unknown_keys_and_duplicate_criteria_are_rejected() {
        let unknown =
            "id = \"x\"\nrubric_kind = \"nope\"\n[[criteria]]\nkey = \"a\"\nweight = 1.0\n";
        assert!(matches!(
            Rubric::parse(unknown),
            Err(RubricError::Parse { .. })
        ));
        let dup = "id = \"x\"\n[[criteria]]\nkey = \"a\"\nweight = 0.5\n[[criteria]]\nkey = \"a\"\nweight = 0.5\n";
        assert!(matches!(
            Rubric::parse(dup),
            Err(RubricError::Invalid { .. })
        ));
    }

    #[test]
    fn overall_is_the_weighted_mean_of_the_scores() {
        let rubric = Rubric::builtin("default").unwrap();
        let perfect: Vec<(String, u8)> = rubric
            .keys()
            .into_iter()
            .map(|k| (k.to_owned(), 10))
            .collect();
        assert!((rubric.overall(&perfect) - 1.0).abs() < 1e-6);
        let none: Vec<(String, u8)> = Vec::new();
        assert!((rubric.overall(&none)).abs() < 1e-6);
        // 0.30*1.0 + 0.25*0.5 = 0.425, the rest 0.
        let partial = vec![
            ("correctness".to_owned(), 10),
            ("completeness".to_owned(), 5),
        ];
        assert!((rubric.overall(&partial) - 0.425).abs() < 1e-6);
    }

    #[test]
    fn task_kinds_map_to_the_documented_rubrics() {
        assert_eq!(builtin_for_kind(&TaskKind::Implement), Some("code"));
        assert_eq!(builtin_for_kind(&TaskKind::Debug), Some("code"));
        assert_eq!(builtin_for_kind(&TaskKind::Research), Some("research"));
        assert_eq!(builtin_for_kind(&TaskKind::Write), Some("writing"));
        assert_eq!(builtin_for_kind(&TaskKind::Review), None);
        assert_eq!(for_kind(None, "default").unwrap().id, "default");
        assert_eq!(
            for_kind(Some(&TaskKind::Review), "research").unwrap().id,
            "research"
        );
    }

    #[test]
    fn a_rubric_file_overrides_a_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.toml");
        std::fs::write(
            &path,
            "id = \"mine\"\n[[criteria]]\nkey = \"only\"\nweight = 1.0\ndescription = \"all of it\"\n",
        )
        .unwrap();
        let rubric = Rubric::resolve(path.to_str().unwrap()).unwrap();
        assert_eq!(rubric.id, "mine");
        assert!(matches!(
            Rubric::resolve("nonexistent"),
            Err(RubricError::Unknown { .. })
        ));
    }
}
