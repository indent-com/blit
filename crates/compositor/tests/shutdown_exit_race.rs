//! Teardown overlapping process exit must not be fatal.
//!
//! `stop()` on a thread the caller then walks away from leaves renderer
//! teardown running while this binary exits.  That overlap has killed the
//! process twice, by two separate mechanisms:
//!
//! - `vkDestroyInstance` had the loader `dlclose()` its layer and ICD
//!   libraries, stranding the thread-local destructors they had registered,
//!   and the next thread to exit jumped into freed memory.  Fixed by leaking
//!   the instance.
//! - the NVIDIA driver's own `atexit` handler frees the driver-global state
//!   `vkDestroyDevice` is walking on the compositor thread.  Fixed by an
//!   `atexit` barrier that holds exit until teardown leaves the driver.
//!
//! Its own test binary, so nothing delays the exit and closes the window.
//! Nothing to assert -- surviving is the result.  A regression shows up as
//! this binary exiting with signal 11.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use blit_compositor::spawn_compositor;

#[test]
fn teardown_overlapping_process_exit_is_survivable() {
    // Several at once, so at least one is reliably still inside teardown
    // when the harness exits.
    for _ in 0..4 {
        let handle = spawn_compositor(false, Arc::new(|| {}), "");
        std::thread::spawn(move || handle.stop());
    }
}
