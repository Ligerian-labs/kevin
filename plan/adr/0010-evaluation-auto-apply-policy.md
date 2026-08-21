# ADR 0010 — Evaluations auto-apply only to routing scores and memory

**Status:** accepted · **Date:** 2026-08-21

## Decision
A judge evaluation may automatically (a) record route outcomes that update `RouteScore`s and (b) store lessons/summaries in memory. Prompt, rubric and configuration changes are emitted as `Proposal`s that a human accepts or rejects (`kevin proposals`). `evaluation.auto_apply` can narrow this further, never widen it.

## Rationale
Fast feedback where the blast radius is bounded (a bad score is corrected by later samples; a bad lesson can be forgotten) and human control where drift would be silent and compounding.
