//! Backward-compat alias for `Pane`. Phase 3 replaces the single-session model
//! with a multi-pane WindowManager; new code should use `crate::pane::Pane`.

pub use crate::pane::Pane as Session;
