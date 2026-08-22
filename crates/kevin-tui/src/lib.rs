//! Terminal UI interface crate (`plan/07-api-and-tui.md` §4).
//!
//! Elm-style ratatui client: pure `Model` + `Msg` + `update` reducer, `view`,
//! and a runtime task executing `Cmd`s through `KevinClient`. Never touches the
//! store.
//!
//! # Shape
//!
//! ```text
//! keys ─┐                       ┌─ view(&Model, &mut Frame)  (pure)
//! ticks ─┼→ Msg → update(&mut Model, Msg) → Vec<Cmd> ─┐
//! SSE  ─┘   (pure)                                    └→ runtime → KevinClient
//!                                                              └→ Msg …
//! ```
//!
//! - [`model`] holds the state of every screen and the bounded buffers
//!   ([`ring::Ring`]: 5 000 transcript lines, 500 timeline events).
//! - [`update`] is the reducer: keybindings, snapshot folding, and the
//!   `Lagged`/`resync` handling that refetches snapshots and reconnects the
//!   stream from the last position seen.
//! - [`view`] renders the seven screens plus the modals, and is asserted with
//!   `ratatui::backend::TestBackend` + `insta` snapshots.
//! - [`runtime`] is the only impure part: the terminal, the client and the
//!   task that turns [`msg::Cmd`]s into [`msg::Msg`]s.
//!
//! Dependency direction: depends only on `kevin-api` (feature `client`, no
//! axum) and `kevin-domain`. Implemented by WS-17.

pub mod fmt;
pub mod keys;
pub mod model;
pub mod msg;
pub mod plan;
pub mod ring;
pub mod runtime;
pub mod theme;
pub mod update;
pub mod view;

pub use keys::{Key, KeyPress};
pub use model::{Model, Overlay, Pane, Screen};
pub use msg::{Cmd, Msg};
pub use ring::Ring;
pub use runtime::{Error, Options};
pub use theme::Theme;
pub use update::{init, update};
pub use view::view;

/// Opens a terminal session against a Kevin daemon.
///
/// `kevin tui` calls this; nothing else in the crate touches a terminal.
pub async fn run(options: Options) -> Result<(), Error> {
    runtime::run(options).await
}
