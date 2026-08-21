//! The scheduler (`plan/05-orchestration.md` §3.5).
//!
//! Two independent concerns:
//!
//! - **Ready set** — a topological pass over the task DAG. A task is ready
//!   when it is `pending` with every dependency `succeeded`, or `routed` (it
//!   has a route and is only waiting for a permit). A task with a dependency
//!   that ended in any other terminal state is *blocked* and gets
//!   `SkipTask{reason: dependency_failed}` instead.
//! - **Bulkheads** — the global `budget.max_parallel_tasks` semaphore and the
//!   per-worker-kind `concurrency.per_worker_kind` semaphores. Permits are
//!   held by the running [`crate::task_runner::TaskRunner`] and released when
//!   it finishes; a routed task without a permit is retried on the next tick
//!   or terminal event.
//!
//! Everything here is pure or `try_*`: the scheduler never blocks the actor.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kevin_config::KevinConfig;
use kevin_domain::{Task, TaskId, TaskSpec, TaskStatus, WorkerKind, WorkspacePolicy};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Reason recorded on `task.skipped`.
pub const DEPENDENCY_FAILED: &str = "dependency_failed";

/// Why a pending task cannot run yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedReason {
    /// At least one dependency has not finished.
    DependenciesPending,
    /// At least one dependency ended in a non-successful terminal state.
    DependencyFailed,
}

/// A pending task that can never run because a dependency failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocked {
    /// The task to skip.
    pub task_id: TaskId,
    /// The dependency that broke it.
    pub dependency: TaskId,
}

/// The tasks that may be started now, in plan order.
///
/// `order` is `run.execution_started.task_ids` (plan order, stable); `tasks`
/// holds the rehydrated aggregates.
#[must_use]
pub fn ready_tasks(order: &[TaskId], tasks: &BTreeMap<TaskId, Task>) -> Vec<TaskId> {
    order
        .iter()
        .copied()
        .filter(|id| {
            tasks.get(id).is_some_and(|task| match task.status() {
                TaskStatus::Routed => true,
                TaskStatus::Pending => dependencies_satisfied(task, tasks),
                _ => false,
            })
        })
        .collect()
}

/// Pending tasks whose dependencies can never succeed (`dependency_failed`).
#[must_use]
pub fn blocked_tasks(order: &[TaskId], tasks: &BTreeMap<TaskId, Task>) -> Vec<Blocked> {
    let mut doomed: BTreeSet<TaskId> = BTreeSet::new();
    let mut blocked = Vec::new();
    for id in order {
        let Some(task) = tasks.get(id) else { continue };
        if task.status() != TaskStatus::Pending {
            if is_failed_terminal(task.status()) {
                doomed.insert(*id);
            }
            continue;
        }
        let Some(spec) = task.spec() else { continue };
        if let Some(dependency) = spec.depends_on.iter().copied().find(|dep| {
            doomed.contains(dep)
                || tasks
                    .get(dep)
                    .is_some_and(|d| is_failed_terminal(d.status()))
        }) {
            doomed.insert(*id);
            blocked.push(Blocked {
                task_id: *id,
                dependency,
            });
        }
    }
    blocked
}

/// Why `task_id` is not in the ready set (`None` = it is ready or terminal).
#[must_use]
pub fn blocked_reason(task: &Task, tasks: &BTreeMap<TaskId, Task>) -> Option<BlockedReason> {
    if task.status() != TaskStatus::Pending {
        return None;
    }
    let spec = task.spec()?;
    let mut pending = false;
    for dep in &spec.depends_on {
        match tasks.get(dep).map(Task::status) {
            Some(TaskStatus::Succeeded) => {}
            Some(status) if is_failed_terminal(status) => {
                return Some(BlockedReason::DependencyFailed);
            }
            _ => pending = true,
        }
    }
    pending.then_some(BlockedReason::DependenciesPending)
}

/// Number of tasks with an attempt in flight.
#[must_use]
pub fn running_count(tasks: &BTreeMap<TaskId, Task>) -> usize {
    tasks
        .values()
        .filter(|t| t.status().has_active_attempt())
        .count()
}

/// Whether `candidate` may run while `running` do
/// (`plan/05` §3.4: `shared` writers are serialised, `parallel_safe = false`
/// tasks run alone).
#[must_use]
pub fn may_run_concurrently(candidate: &TaskSpec, running: &[&TaskSpec]) -> bool {
    if running.is_empty() {
        return true;
    }
    if !candidate.parallel_safe || running.iter().any(|s| !s.parallel_safe) {
        return false;
    }
    let candidate_writes = candidate.workspace_policy != WorkspacePolicy::ReadOnly;
    let shared_running = running
        .iter()
        .any(|s| s.workspace_policy == WorkspacePolicy::Shared);
    let writer_running = running
        .iter()
        .any(|s| s.workspace_policy != WorkspacePolicy::ReadOnly);
    match candidate.workspace_policy {
        WorkspacePolicy::Shared => !writer_running,
        WorkspacePolicy::Isolated => !shared_running,
        WorkspacePolicy::ReadOnly => {
            let _ = candidate_writes;
            true
        }
    }
}

fn dependencies_satisfied(task: &Task, tasks: &BTreeMap<TaskId, Task>) -> bool {
    task.spec().is_some_and(|spec| {
        spec.depends_on
            .iter()
            .all(|dep| tasks.get(dep).map(Task::status) == Some(TaskStatus::Succeeded))
    })
}

const fn is_failed_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Cancelled | TaskStatus::Skipped | TaskStatus::Failed
    )
}

// ---------------------------------------------------------------------------
// Bulkheads
// ---------------------------------------------------------------------------

/// Permits held while one attempt runs; released on drop.
#[derive(Debug)]
pub struct Permits {
    /// Worker kind the per-kind permit belongs to.
    pub worker: WorkerKind,
    _global: OwnedSemaphorePermit,
    _per_kind: OwnedSemaphorePermit,
}

/// The global and per-worker-kind concurrency limits (`plan/05` §3.5).
#[derive(Debug, Clone)]
pub struct Bulkheads {
    global: Arc<Semaphore>,
    per_kind: BTreeMap<WorkerKind, Arc<Semaphore>>,
    default_per_kind: usize,
}

impl Bulkheads {
    /// Limits from `budget.max_parallel_tasks` and `concurrency.per_worker_kind`.
    #[must_use]
    pub fn from_config(config: &KevinConfig) -> Self {
        let per_kind = config
            .concurrency
            .per_worker_kind
            .iter()
            .map(|(kind, limit)| (*kind, Arc::new(Semaphore::new(permits(*limit as usize)))))
            .collect();
        Self {
            global: Arc::new(Semaphore::new(permits(
                config.budget.max_parallel_tasks as usize,
            ))),
            per_kind,
            default_per_kind: 4,
        }
    }

    /// Explicit limits (tests).
    #[must_use]
    pub fn new(global: usize, per_kind: BTreeMap<WorkerKind, usize>) -> Self {
        Self {
            global: Arc::new(Semaphore::new(permits(global))),
            per_kind: per_kind
                .into_iter()
                .map(|(k, v)| (k, Arc::new(Semaphore::new(permits(v)))))
                .collect(),
            default_per_kind: 4,
        }
    }

    /// Free global permits.
    #[must_use]
    pub fn global_available(&self) -> usize {
        self.global.available_permits()
    }

    /// Takes one global and one per-kind permit, or `None` when either is
    /// exhausted (never blocks; the task stays `routed`).
    #[must_use]
    pub fn try_acquire(&self, worker: WorkerKind) -> Option<Permits> {
        let per_kind = self.kind_semaphore(worker);
        let global = match Arc::clone(&self.global).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                metrics::gauge!(
                    kevin_telemetry::metrics::WORKER_SEMAPHORE_WAITERS,
                    "worker" => worker.as_str(),
                )
                .increment(1.0);
                return None;
            }
            Err(TryAcquireError::Closed) => return None,
        };
        let Ok(permit) = per_kind.try_acquire_owned() else {
            metrics::gauge!(
                kevin_telemetry::metrics::WORKER_SEMAPHORE_WAITERS,
                "worker" => worker.as_str(),
            )
            .increment(1.0);
            return None;
        };
        Some(Permits {
            worker,
            _global: global,
            _per_kind: permit,
        })
    }

    fn kind_semaphore(&self, worker: WorkerKind) -> Arc<Semaphore> {
        self.per_kind.get(&worker).map_or_else(
            || Arc::new(Semaphore::new(self.default_per_kind)),
            Arc::clone,
        )
    }
}

const fn permits(limit: usize) -> usize {
    if limit == 0 { 1 } else { limit }
}

#[cfg(test)]
mod tests {
    use kevin_domain::task::{CreateTask, TaskCommand};
    use kevin_domain::{Aggregate, Budget, RunId, TaskKind};

    use super::*;

    fn task(id: TaskId, spec: TaskSpec) -> Task {
        let mut task = Task::default();
        let events = task
            .handle(&TaskCommand::Create(CreateTask {
                task_id: id,
                run_id: RunId::nil(),
                kind: TaskKind::Implement,
                spec,
                budget: Budget::unlimited(),
            }))
            .expect("create");
        for event in &events {
            task.apply(event);
        }
        task
    }

    fn finish(task: &mut Task, status: TaskStatus) {
        use kevin_domain::task::TaskEvent;
        match status {
            TaskStatus::Succeeded => {
                task.apply(&TaskEvent::Routed {
                    route: kevin_domain::Route::new(
                        WorkerKind::Fake,
                        kevin_domain::ModelAlias::new("fake").expect("alias"),
                    ),
                    selection: kevin_domain::task::RouteSelectionInfo::fixed(
                        kevin_domain::ModelAlias::new("fake").expect("alias"),
                    ),
                });
                task.apply(&TaskEvent::AttemptStarted {
                    attempt_id: kevin_domain::AttemptId::new(),
                    attempt_no: 1,
                    route: kevin_domain::Route::new(
                        WorkerKind::Fake,
                        kevin_domain::ModelAlias::new("fake").expect("alias"),
                    ),
                    workspace: kevin_domain::Workspace::in_place("/tmp"),
                    worker_session_id: None,
                });
                let attempt = task.active_attempt().expect("attempt").id;
                task.apply(&TaskEvent::AttemptSucceeded {
                    attempt_id: attempt,
                    artifacts: Vec::new(),
                    summary: "ok".into(),
                    usage: kevin_domain::Usage::ZERO,
                });
            }
            TaskStatus::Skipped => task.apply(&TaskEvent::Skipped {
                reason: DEPENDENCY_FAILED.into(),
            }),
            _ => task.apply(&TaskEvent::Cancelled {
                reason: "test".into(),
            }),
        }
    }

    #[test]
    fn ready_set_follows_dependencies_in_plan_order() {
        let a = TaskId::new();
        let b = TaskId::new();
        let mut tasks = BTreeMap::new();
        tasks.insert(a, task(a, TaskSpec::new("a", "do a")));
        let mut spec_b = TaskSpec::new("b", "do b");
        spec_b.depends_on = vec![a];
        tasks.insert(b, task(b, spec_b));
        let order = vec![a, b];
        assert_eq!(ready_tasks(&order, &tasks), vec![a]);
        finish(tasks.get_mut(&a).expect("a"), TaskStatus::Succeeded);
        assert_eq!(ready_tasks(&order, &tasks), vec![b]);
    }

    #[test]
    fn dependents_of_a_failed_task_are_blocked_transitively() {
        let a = TaskId::new();
        let b = TaskId::new();
        let c = TaskId::new();
        let mut tasks = BTreeMap::new();
        tasks.insert(a, task(a, TaskSpec::new("a", "do a")));
        let mut spec_b = TaskSpec::new("b", "do b");
        spec_b.depends_on = vec![a];
        tasks.insert(b, task(b, spec_b));
        let mut spec_c = TaskSpec::new("c", "do c");
        spec_c.depends_on = vec![b];
        tasks.insert(c, task(c, spec_c));
        finish(tasks.get_mut(&a).expect("a"), TaskStatus::Cancelled);
        let order = vec![a, b, c];
        let blocked = blocked_tasks(&order, &tasks);
        assert_eq!(blocked.len(), 2);
        assert_eq!(blocked[0].task_id, b);
        assert_eq!(blocked[1].task_id, c);
        assert!(ready_tasks(&order, &tasks).is_empty());
    }

    #[test]
    fn shared_workspace_tasks_are_serialised() {
        let mut shared = TaskSpec::new("s", "shared");
        shared.workspace_policy = WorkspacePolicy::Shared;
        let isolated = TaskSpec::new("i", "isolated");
        let mut read_only = TaskSpec::new("r", "read");
        read_only.workspace_policy = WorkspacePolicy::ReadOnly;
        assert!(may_run_concurrently(&shared, &[]));
        assert!(!may_run_concurrently(&shared, &[&isolated]));
        assert!(!may_run_concurrently(&isolated, &[&shared]));
        assert!(may_run_concurrently(&isolated, &[&isolated]));
        assert!(may_run_concurrently(&shared, &[&read_only]));
        assert!(may_run_concurrently(&read_only, &[&shared]));
    }

    #[test]
    fn non_parallel_safe_tasks_run_alone() {
        let mut lonely = TaskSpec::new("l", "alone");
        lonely.parallel_safe = false;
        let other = TaskSpec::new("o", "other");
        assert!(may_run_concurrently(&lonely, &[]));
        assert!(!may_run_concurrently(&lonely, &[&other]));
        assert!(!may_run_concurrently(&other, &[&lonely]));
    }

    #[test]
    fn bulkheads_bound_global_and_per_kind_concurrency() {
        let bulkheads = Bulkheads::new(2, BTreeMap::from([(WorkerKind::Fake, 1)]));
        let first = bulkheads.try_acquire(WorkerKind::Fake).expect("first");
        assert!(bulkheads.try_acquire(WorkerKind::Fake).is_none());
        drop(first);
        assert!(bulkheads.try_acquire(WorkerKind::Fake).is_some());
    }

    #[test]
    fn global_bulkhead_is_the_tighter_bound() {
        let bulkheads = Bulkheads::new(1, BTreeMap::from([(WorkerKind::Fake, 8)]));
        let held = bulkheads.try_acquire(WorkerKind::Fake).expect("first");
        assert_eq!(bulkheads.global_available(), 0);
        assert!(bulkheads.try_acquire(WorkerKind::Fake).is_none());
        drop(held);
        assert_eq!(bulkheads.global_available(), 1);
    }
}
