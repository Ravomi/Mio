//! Inline TUI for interactive `mio` sessions.
//!
//! See `docs/tui-interaction-redesign.md`. Non-TTY, `-p`, pipe and
//! `--mode json|rpc` paths never go through this module.

// The palette is staged ahead of the P0 inline viewport; drop this once the
// viewport renders with it.
#[allow(dead_code)]
pub mod palette;
