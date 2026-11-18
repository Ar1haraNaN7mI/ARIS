//! Ratatui terminal UI for aris (Codex-inspired transcript + category colors).

mod app;
mod commands;
mod keymap;
mod terminal;
mod theme;

pub use app::{run_tui, TuiOptions};
