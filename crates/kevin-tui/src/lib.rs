//! Terminal UI interface crate (`plan/07-api-and-tui.md` §4).
//!
//! Elm-style ratatui client: pure `Model` + `Msg` + `update` reducer, `view`,
//! and a runtime task executing `Cmd`s through `KevinClient`. Never touches the
//! store.
//!
//! Dependency direction: depends only on `kevin-api` (feature `client`, no
//! axum) and `kevin-domain`. Implemented by WS-17.
