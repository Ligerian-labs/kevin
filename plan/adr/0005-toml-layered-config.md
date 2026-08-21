# ADR 0005 — TOML configuration with layered precedence and strict validation

**Status:** accepted · **Date:** 2026-08-21

## Decision
`KevinConfig` (serde, `deny_unknown_fields`) resolved from defaults → user file → project file → env (`KEVIN__A__B`) → CLI. Validated as a whole at startup (all errors at once), immutable afterwards, `kevin config show` reveals each value's source with secrets redacted. Profiles (`laptop|server|kohral`) only change defaults, never branch behaviour.

## Alternatives
YAML/JSON (less idiomatic for Rust CLIs, comments/anchors issues); env-only (unreadable for a catalog of models).

## Consequences
The model catalog and routing candidates are configuration, so adding a model is a config change; validation guarantees roles/candidates reference enabled workers.
