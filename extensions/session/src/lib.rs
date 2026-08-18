//! `@session` — autostart and supervise GUI applications in a Blit session.
//!
//! The parts that are easy to get wrong live here as a host-testable library:
//! desktop-entry parsing, icon lookup, and the supervisor's desired-state
//! bookkeeping. The protocol plumbing that binds them to a running server is in
//! `main.rs`, which is what compiles to wasm.

#![no_std]

extern crate alloc;

pub mod desktop_entry;
pub mod icon;
pub mod supervisor;
