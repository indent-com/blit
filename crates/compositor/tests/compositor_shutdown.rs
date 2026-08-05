//! Stopping the compositor has to finish before the caller moves on.
//!
//! Teardown ends in `vkDestroyInstance`, and the Vulkan loader `dlclose()`s
//! its layer libraries on the way out.  Left to run on a detached thread
//! while the process is exiting, that unmaps code from under whatever the
//! main thread is still executing and the process dies with a SIGSEGV that
//! has no stack to read.  `stop()` joins, which keeps the unloading inside
//! the caller's lifetime.
//!
//! If this regresses, the symptom is the whole test binary exiting with
//! signal 11 rather than a failed assertion.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use blit_compositor::spawn_compositor;

#[test]
fn stop_tears_the_compositor_down_before_returning() {
    // Twice, because a teardown that corrupts process state tends to show up
    // on the way into the next one.
    for round in 0..2 {
        let handle = spawn_compositor(false, Arc::new(|| {}), "");
        let socket = handle.socket_name.clone();
        assert!(
            UnixStream::connect(&socket).is_ok(),
            "round {round}: compositor should be accepting connections"
        );

        handle.stop();

        // stop() returned, so the thread is joined and its Vulkan teardown is
        // already done -- on this thread's watch, not racing process exit.
        assert!(
            UnixStream::connect(&socket).is_err(),
            "round {round}: socket at {socket} still accepts connections after \
             stop(), so the compositor thread was not actually joined"
        );
    }
}
