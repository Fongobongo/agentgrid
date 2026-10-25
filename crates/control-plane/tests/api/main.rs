//! One integration-test binary (was a single 12k-line api.rs), split into
//! domain modules over a shared helper module. Same 158 tests, same
//! per-test `AppState::open_temp()` isolation — only the file layout
//! changed. `helpers` re-exports everything a module needs via
//! `use crate::helpers::*;`.
mod artifacts;
mod auth_approvals;
mod events;
mod helpers;
mod lifecycle;
mod misc;
mod nodes;
mod profiles;
mod repos;
mod scheduler;
mod security;
mod workflows;
