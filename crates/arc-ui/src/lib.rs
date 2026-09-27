//! A small GTK4 layer-shell panel showing what Arc is doing.
//!
//! This is deliberately a *view*: it reads the same `bar.json` the Omarchy bar
//! widget reads, and talks to the daemon over its existing socket only when
//! the user presses a confirmation button. It never talks to a model and
//! never runs a tool.
pub mod panel;

pub use panel::Panel;
