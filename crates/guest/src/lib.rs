#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

mod bootstrap;
#[cfg(feature = "protocol")]
pub mod channel;
mod client;
pub mod collections;
#[cfg(feature = "protocol")]
pub mod command;
pub mod host;
#[cfg(feature = "protocol")]
pub mod terminal;
pub mod timer;

#[cfg(not(target_arch = "wasm32"))]
pub mod native_host;

pub use bootstrap::{Context, Hello};
pub use client::{Client, EXIT_BOOTSTRAP_FAILURE, Error, MonotonicInstant, Realtime, WaitOutcome};
pub use timer::{EventLoop, EventLoopEvent, EventLoopExit, TimerId};

#[cfg(feature = "protocol")]
pub use blit_remote as remote;

#[doc(hidden)]
pub use getrandom::register_custom_getrandom as __register_getrandom_02;

/// Result types accepted by [`entry!`].
pub trait EntryResult {
    /// Convert a user entry result to the extension's `i32` exit code.
    fn exit_code(self) -> i32;
}

impl EntryResult for () {
    fn exit_code(self) -> i32 {
        0
    }
}

impl EntryResult for i32 {
    fn exit_code(self) -> i32 {
        self
    }
}

impl<E> EntryResult for Result<(), E> {
    fn exit_code(self) -> i32 {
        if self.is_ok() { 0 } else { 1 }
    }
}

/// Bootstrap a client, invoke a guest function, and return its exit code.
///
/// Most guests use [`entry!`] rather than calling this directly.
pub fn run_entry<F, R>(entry: F) -> i32
where
    F: FnOnce(Client) -> R,
    R: EntryResult,
{
    match Client::bootstrap() {
        Ok(client) => entry(client).exit_code(),
        Err(_) => EXIT_BOOTSTRAP_FAILURE,
    }
}

/// Fill a buffer for the pinned `getrandom` 0.2 custom backend.
#[doc(hidden)]
pub fn __getrandom_v02(bytes: &mut [u8]) -> Result<(), getrandom::Error> {
    host::random(bytes).map_err(|_| {
        let code = core::num::NonZeroU32::new(getrandom::Error::CUSTOM_START + 1)
            .expect("custom getrandom code is non-zero");
        getrandom::Error::from(code)
    })
}

/// Install Blit's entropy source as the `getrandom` 0.2 custom backend.
///
/// Expand this once in the root guest crate if it does not use [`entry!`].
/// The SDK pins `getrandom` 0.2.17; `rand` 0.8 uses this backend without a JS
/// adapter. Newer `getrandom` major versions use a different selection model.
#[macro_export]
macro_rules! register_getrandom {
    () => {
        $crate::__register_getrandom_02!($crate::__getrandom_v02);
    };
}

/// Export a Rust function as the Wasmi contract's `blit_main: () -> i32`.
///
/// The function receives a fully bootstrapped [`Client`]. It may return `()`,
/// an `i32`, or `Result<(), E>`. Bootstrap failure exits with
/// [`EXIT_BOOTSTRAP_FAILURE`].
///
/// ```ignore
/// fn extension(mut client: blit_guest::Client) -> Result<(), blit_guest::Error> {
///     client.send(&[blit_guest::remote::C2S_PING])?;
///     Ok(())
/// }
///
/// blit_guest::entry!(extension);
/// ```
#[macro_export]
macro_rules! entry {
    ($entry:path) => {
        $crate::register_getrandom!();

        #[unsafe(export_name = "blit_main")]
        pub extern "C" fn __blit_guest_main() -> i32 {
            $crate::run_entry($entry)
        }
    };
}
