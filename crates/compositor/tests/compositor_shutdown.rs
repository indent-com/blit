//! Stopping the compositor must not take the process down with it.
//!
//! Renderer teardown used to end in `vkDestroyInstance`, where the Vulkan
//! loader `dlclose()`s its layer and ICD libraries -- libraries that have
//! registered thread-local destructors on every thread that touched them.
//! Unmapping them left those destructors dangling, so a thread exiting
//! afterwards jumped into freed memory.  The instance is now leaked on
//! purpose instead.
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
