//! tsuiku — structural TUI diff viewer.
//!
//! Current scope: data contracts, git access, and the asynchronous line-diff
//! viewer.

pub mod app;
pub mod asyncstate;
pub mod cache;
pub mod change;
pub mod compose;
pub mod config;
pub mod coords;
pub mod discover;
pub mod ids;
pub mod linediff;
pub mod loader;
pub mod path;
pub mod resolve;
pub mod structural;
pub mod structural_worker;
pub mod syntax;
pub mod syntax_worker;
pub mod text;
pub mod theme;
pub mod view;
pub mod watch;
pub mod worker;
