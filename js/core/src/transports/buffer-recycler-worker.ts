// Received ArrayBuffers become unreachable when this task returns. Their
// backing stores are then swept on this worker instead of pausing the UI
// thread that presents video frames.
self.onmessage = () => {};
