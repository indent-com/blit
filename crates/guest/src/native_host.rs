//! Native host shim for unit-testing guest code without Wasmi.

use std::{boxed::Box, cell::RefCell};

/// Native equivalent of the five raw host calls.
///
/// `recv` must implement the ABI's peek-on-small-buffer behavior and return
/// the required packet length without consuming it.
pub trait Host {
    fn send(&mut self, packet: &[u8]) -> i32;
    fn recv(&mut self, buffer: &mut [u8]) -> i32;
    fn wait(&mut self, monotonic_deadline_ns: i64) -> i32;
    fn clock(&mut self, kind: i32) -> i64;
    fn random(&mut self, destination: &mut [u8]);
}

std::thread_local! {
    static HOST: RefCell<Option<Box<dyn Host>>> = RefCell::new(None);
}

/// Restores the previously installed shim when dropped.
#[must_use]
pub struct Guard {
    previous: Option<Box<dyn Host>>,
}

/// Install a host for the current native thread.
///
/// Thread-local installation lets Rust's parallel test runner exercise
/// independent guest instances safely.
pub fn install(host: impl Host + 'static) -> Guard {
    let previous = HOST.with(|slot| slot.replace(Some(Box::new(host))));
    Guard { previous }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        HOST.with(|slot| {
            slot.replace(previous);
        });
    }
}

pub(crate) fn with<R>(operation: impl FnOnce(&mut dyn Host) -> R) -> R {
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let host = slot
            .as_deref_mut()
            .expect("blit-guest native API used without native_host::install");
        operation(host)
    })
}
