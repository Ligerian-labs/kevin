//! The OpenAPI document served at `GET /api/v1/openapi.json`
//! (`plan/07-api-and-tui.md` §1).
//!
//! It is generated from the same `#[utoipa::path]` annotations the handlers
//! carry and the same `ToSchema` derives the DTOs carry, so the document
//! cannot drift from the implementation.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::dto;
use crate::error::{ErrorBody, ErrorCode};

/// Adds the `bearer` security scheme every `/api/v1` path references.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Token from `server.auth_token_file`; compared in constant time.",
                    ))
                    .build(),
            ),
        );
    }
}

/// The Kevin HTTP API.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "Kevin API",
        description = "Autonomous agent runtime — runs, tasks, questions, events, cost, memory.",
        license(name = "Apache-2.0")
    ),
    modifiers(&BearerAuth),
    tags(
        (name = "runs", description = "Start, inspect and steer runs"),
        (name = "tasks", description = "Task board, transcripts and artifacts"),
        (name = "questions", description = "Clarification inbox"),
        (name = "events", description = "Server-sent event streams"),
        (name = "cost", description = "Spend reporting"),
        (name = "routes", description = "Routing leaderboard"),
        (name = "memory", description = "Retrieval memory and lessons"),
        (name = "proposals", description = "Evaluator proposals"),
        (name = "workers", description = "Worker health"),
        (name = "config", description = "Effective configuration"),
        (name = "maintenance", description = "Drain and admission control"),
        (name = "health", description = "Liveness and readiness"),
    ),
    paths(
        crate::routes::runs::create_run,
        crate::routes::runs::list_runs,
        crate::routes::runs::get_run,
        crate::routes::runs::cancel_run,
        crate::routes::runs::approve_plan,
        crate::routes::runs::reject_plan,
        crate::routes::runs::evaluate_run,
        crate::routes::runs::list_run_tasks,
        crate::routes::tasks::get_task,
        crate::routes::tasks::retry_task,
        crate::routes::tasks::cancel_task,
        crate::routes::tasks::task_log,
        crate::routes::tasks::task_artifacts,
        crate::routes::tasks::artifact_bytes,
        crate::routes::questions::list_questions,
        crate::routes::questions::get_question,
        crate::routes::questions::answer_question,
        crate::routes::events::firehose,
        crate::routes::events::run_events,
        crate::routes::events::task_log_stream,
        crate::routes::cost::cost,
        crate::routes::routes::leaderboard,
        crate::routes::memory::search,
        crate::routes::memory::lessons,
        crate::routes::memory::forget,
        crate::routes::proposals::list,
        crate::routes::proposals::accept,
        crate::routes::proposals::reject,
        crate::routes::workers::doctor,
        crate::routes::config::effective_config,
        crate::routes::maintenance::drain_status,
        crate::routes::maintenance::start_drain,
        crate::routes::maintenance::stop_drain,
        crate::routes::health::healthz,
        crate::routes::health::readyz,
    ),
    components(schemas(
        ErrorBody,
        ErrorCode,
        dto::AnswerDto,
        dto::AnswerRequest,
        dto::ArtifactDto,
        dto::AttachmentRef,
        dto::AttemptDto,
        dto::BudgetDto,
        dto::CancelRunRequest,
        dto::ConfigDto,
        dto::CostReportDto,
        dto::CostRowDto,
        dto::CreateRunRequest,
        dto::DrainStatusDto,
        dto::EmptyRequest,
        dto::EvaluationSummaryDto,
        dto::EventDto,
        dto::FailureDto,
        dto::GoalDto,
        dto::HealthDto,
        dto::MemoryItemDto,
        dto::PlanDto,
        dto::ProposalDecisionRequest,
        dto::ProposalDto,
        dto::QuestionDto,
        dto::QuestionOptionDto,
        dto::QuestionPolicyDto,
        dto::QuestionPolicyKind,
        dto::ReadyDto,
        dto::RejectPlanRequest,
        dto::ResyncDto,
        dto::RetryTaskRequest,
        dto::RouteDto,
        dto::RouteScoreDto,
        dto::RunDto,
        dto::RunModeDto,
        dto::RunStatusDto,
        dto::RunSummaryDto,
        dto::TaskCountsDto,
        dto::TaskDto,
        dto::TaskLogLineDto,
        dto::TaskSummaryDto,
        dto::UnderstandingDto,
        dto::UsageDto,
        dto::WorkerDoctorDto,
        dto::WorkspaceDto,
    ))
)]
pub struct ApiDoc;

impl ApiDoc {
    /// The document as JSON.
    #[must_use]
    pub fn json() -> serde_json::Value {
        serde_json::to_value(Self::openapi()).unwrap_or(serde_json::Value::Null)
    }
}

/// A self-contained Swagger UI page (`server.docs = true`).
///
/// The assets are loaded from a CDN rather than vendored: the docs toggle is a
/// developer convenience on the laptop profile and defaults to `false` on the
/// server and Kohral profiles, so no offline deployment depends on it.
pub const DOCS_HTML: &str = r##"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Kevin API</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js" crossorigin></script>
    <script>
      window.onload = () => {
        window.ui = SwaggerUIBundle({ url: "/api/v1/openapi.json", dom_id: "#swagger-ui" });
      };
    </script>
  </body>
</html>
"##;
