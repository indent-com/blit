//! `@muster` — supervise units that run in terminals.
//!
//! The parts that are easy to get wrong live here as a host-testable library:
//! unit-file parsing, stack substitution, dotenv merging, and the supervisor's
//! phase and backoff bookkeeping. The protocol plumbing that binds them to a
//! running server is in `main.rs`, which is what compiles to wasm.

pub mod config;
pub mod envfile;
pub mod journal;
pub mod supervisor;
pub mod worktrees;
