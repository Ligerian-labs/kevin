# ADR 0007 — One git worktree / jj workspace per task attempt

**Status:** accepted · **Date:** 2026-08-21

## Context
Parallel child agents editing the same checkout corrupt each other's work; the user's own workflow uses jj workspaces (`ws`).

## Decision
`kevin-workspace` creates an isolated checkout per attempt under `<repo>/.kevin/workspaces/<run-short>/<task>-<attempt>`: jj workspace when `.jj` exists, else git worktree on branch `kevin/<run-short>/<task-slug>`, else in-place (single task only). Integration is a dedicated step (`integrator` role) producing PRs or merges per `workspace.integration`. Cleanup policy is configurable.

## Consequences
Every worker runs with cwd = its workspace; env allow-list + sandbox flags keep it there. Tasks flagged `parallel_safe=false` serialise on the integration branch.
