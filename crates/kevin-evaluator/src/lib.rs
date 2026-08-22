//! Evaluation core context (`plan/06-memory-and-learning.md` §3).
//!
//! Owns rubrics, the judge prompt and its `kevin.evaluation.v1` schema, the
//! judge runner (through `kevin-worker`), evaluation records, score
//! normalisation, lessons extraction and the proposals inbox. Schema `eval.*`.
//!
//! # The loop
//!
//! ```text
//!            evidence (scrubbed)              overall = Σ w_i·s_i/10
//! subject ─▶ judge (kevin.evaluation.v1) ─▶ recompute ─▶ evaluation.recorded
//!                                                          │
//!                    routing ◀── RecordRouteOutcome ────────┤ auto_apply
//!                     memory ◀── StoreMemoryItem(lesson) ───┤
//!                  proposals ◀── eval.proposals (human) ────┘
//! ```
//!
//! Evaluations auto-update **routing scores and memory only**; prompt, config
//! and routing changes the judge proposes are inbox items a human accepts or
//! rejects (`plan/adr/0010-evaluation-auto-apply-policy.md`).
//!
//! Dependency direction: depends on `kevin-worker`, `kevin-router` (through the
//! narrow [`RouterPort`], so the router can land independently), `kevin-memory`
//! and the platform crates; downstream of orchestration by events only.

pub mod auto_apply;
pub mod error;
pub mod evaluator;
pub mod events;
pub mod evidence;
pub mod judge;
pub mod memory_port;
pub mod prompt;
pub mod proposals;
pub mod repo;
pub mod router_port;
pub mod rubric;
pub mod runner;
pub mod schemas;

pub use auto_apply::{AutoApply, AutoApplyContext, AutoApplyReport};
pub use error::{EvaluatorError, Result, SkipReason};
pub use evaluator::{
    EvaluationOutcome, EvaluationRequest, Evaluator, EvaluatorConfig, JUDGE_TAG, reconcile,
};
pub use evidence::{ArtifactLine, Evidence, Scrubber, TaskVerdict};
pub use judge::{Judge, JudgeContext, JudgeOutput, JudgeOutputError, JudgeProposal, JudgeScore};
pub use memory_port::{InMemoryLessons, LESSON_DEDUP_SIMILARITY, MemoryPort, MemoryPortError};
pub use proposals::{AcceptOutcome, Proposals, RoutingAction, RoutingDirective};
pub use repo::{
    Decision, EvaluationRecord, EvaluationRepo, InMemoryEvaluationRepo, PgEvaluationRepo,
    ProposalRow,
};
pub use router_port::{InMemoryRouter, OutcomeAttempt, RouterPort, RouterPortError};
pub use rubric::{Criterion, Rubric, RubricError};
pub use runner::JudgeRunner;
pub use schemas::{EVALUATION_V1_ID, EVALUATION_V1_JSON};
