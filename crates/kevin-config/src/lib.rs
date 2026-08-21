//! Configuration context (`plan/03-config-schema.md`).
//!
//! Owns the typed `KevinConfig` schema (TOML, `deny_unknown_fields`), the
//! layered loader (defaults → user file → project file → env → CLI flags),
//! whole-config validation with aggregated errors, redaction for `kevin config
//! show`, and the default model catalog.
//!
//! Dependency direction: depends on `kevin-domain` only (for value objects such
//! as `ModelAlias`). Every other crate may depend on it; it depends on none of
//! them. Implemented by WS-02.
