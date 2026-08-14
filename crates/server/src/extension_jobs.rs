//! Extension-origin native job admission and cleanup tracking.
//!
//! Network connections keep their historical dispatch behavior.  An extension
//! endpoint gets one [`EndpointTracker`]: every native one-shot job first owns
//! a bounded pending record, then endpoint and process-wide active permits.
//! The serialized request-byte charge transfers with that record and is held
//! until the native call has actually returned, including after connection
//! cancellation.

use super::ConnectionCancellation;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const DEFAULT_ACTIVE_PER_ENDPOINT: usize = 32;
const DEFAULT_ACTIVE_GLOBAL: usize = 128;
const DEFAULT_PENDING_PER_ENDPOINT: usize = 32;
const DEFAULT_PENDING_GLOBAL: usize = 128;
const DEFAULT_BYTES_PER_ENDPOINT: usize = 16 * 1024 * 1024;
const DEFAULT_BYTES_GLOBAL: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct Limits {
    active_per_endpoint: usize,
    active_global: usize,
    pending_per_endpoint: usize,
    pending_global: usize,
    bytes_per_endpoint: usize,
    bytes_global: usize,
}

impl Limits {
    fn from_env() -> Self {
        Self {
            active_per_endpoint: crate::deployment_usize(
                "BLIT_EXT_JOB_MAX_PER_CLIENT",
                DEFAULT_ACTIVE_PER_ENDPOINT,
            ),
            active_global: crate::deployment_usize("BLIT_EXT_JOB_MAX", DEFAULT_ACTIVE_GLOBAL),
            pending_per_endpoint: crate::deployment_usize(
                "BLIT_EXT_JOB_PENDING_MAX_PER_CLIENT",
                DEFAULT_PENDING_PER_ENDPOINT,
            ),
            pending_global: crate::deployment_usize(
                "BLIT_EXT_JOB_PENDING_MAX",
                DEFAULT_PENDING_GLOBAL,
            ),
            bytes_per_endpoint: crate::deployment_usize(
                "BLIT_EXT_JOB_BYTES_MAX_PER_CLIENT",
                DEFAULT_BYTES_PER_ENDPOINT,
            ),
            bytes_global: crate::deployment_usize("BLIT_EXT_JOB_BYTES_MAX", DEFAULT_BYTES_GLOBAL),
        }
    }
}

#[derive(Default, Debug)]
struct Usage {
    pending: usize,
    active: usize,
    bytes: usize,
}

#[derive(Debug)]
struct GlobalInner {
    limits: Limits,
    usage: Mutex<Usage>,
    active: Arc<Semaphore>,
}

/// Process-wide half of extension native-job admission.
#[derive(Clone, Debug)]
pub(crate) struct GlobalTracker {
    inner: Arc<GlobalInner>,
}

impl GlobalTracker {
    pub(crate) fn from_env() -> Self {
        Self::new(Limits::from_env())
    }

    fn new(limits: Limits) -> Self {
        Self {
            inner: Arc::new(GlobalInner {
                limits,
                usage: Mutex::new(Usage::default()),
                active: Arc::new(Semaphore::new(limits.active_global)),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_single_active() -> Self {
        Self::new(Limits {
            active_per_endpoint: 1,
            active_global: 1,
            pending_per_endpoint: 8,
            pending_global: 8,
            bytes_per_endpoint: 64 * 1024 * 1024,
            bytes_global: 64 * 1024 * 1024,
        })
    }

    pub(crate) fn endpoint(&self, cancellation: ConnectionCancellation) -> EndpointTracker {
        EndpointTracker {
            inner: Arc::new(EndpointInner {
                global: self.clone(),
                cancellation,
                usage: Mutex::new(Usage::default()),
                active: Arc::new(Semaphore::new(self.inner.limits.active_per_endpoint)),
                tasks: AtomicUsize::new(0),
                drained: Notify::new(),
            }),
        }
    }
}

#[derive(Debug)]
struct EndpointInner {
    global: GlobalTracker,
    cancellation: ConnectionCancellation,
    usage: Mutex<Usage>,
    active: Arc<Semaphore>,
    /// Admission futures as well as launched native calls.  Incremented before
    /// spawning so cleanup can never observe a false zero between reservation
    /// and task launch.
    tasks: AtomicUsize,
    drained: Notify,
}

/// Admission exhausted before a request could be dispatched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionError;

/// Family-owned cancellation checked immediately before a pending blocking
/// call is launched. Cancellation cannot stop a call which has already begun,
/// but it can retire saturated admission without consuming a blocking thread.
#[derive(Debug, Default)]
struct LaunchCancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LaunchCancellation {
    inner: Arc<LaunchCancellationInner>,
}

impl LaunchCancellation {
    pub(crate) fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Per-extension-endpoint job tracker.
#[derive(Clone, Debug)]
pub(crate) struct EndpointTracker {
    inner: Arc<EndpointInner>,
}

impl EndpointTracker {
    /// Whether the owning logical connection has begun cleanup.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancellation.is_cancelled()
    }

    /// Resolves when the owning logical connection begins cleanup.
    pub(crate) async fn cancelled(&self) {
        self.inner.cancellation.cancelled().await;
    }

    /// Admit an asynchronous native job.  The future is not polled until both
    /// active permits are held, and remains part of the cleanup barrier until
    /// it returns.
    pub(crate) fn spawn_async<F>(&self, request_bytes: usize, work: F) -> Result<(), AdmissionError>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let pending = self.reserve(request_bytes)?;
        let task = TaskGuard::new(self.inner.clone());
        tokio::spawn(async move {
            let _task = task;
            let Some(active) = pending.activate().await else {
                return;
            };
            let _active = active;
            work.await;
        });
        Ok(())
    }

    /// Reserve pending count and retained request bytes, then launch a blocking
    /// native call once both active permits are available.  Returning `Ok`
    /// means the bounded admission record exists, not that the call has begun.
    pub(crate) fn spawn_blocking<F>(
        &self,
        request_bytes: usize,
        work: F,
    ) -> Result<(), AdmissionError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.spawn_blocking_checked(request_bytes, || true, || {}, work)
    }

    /// As [`Self::spawn_blocking`], with a family cancellation check performed
    /// after admission and immediately before launch.  This lets an explicit
    /// nonce cancel remove a pending record without consuming a blocking-pool
    /// thread.
    pub(crate) fn spawn_blocking_checked<C, S, F>(
        &self,
        request_bytes: usize,
        should_launch: C,
        skipped: S,
        work: F,
    ) -> Result<(), AdmissionError>
    where
        C: FnOnce() -> bool + Send + 'static,
        S: FnOnce() + Send + 'static,
        F: FnOnce() + Send + 'static,
    {
        let pending = self.reserve(request_bytes)?;
        let task = TaskGuard::new(self.inner.clone());
        tokio::spawn(async move {
            let _task = task;
            let Some(active) = pending.activate().await else {
                return;
            };
            if !should_launch() {
                skipped();
                return;
            }
            // The active guard intentionally lives across the JoinHandle await.
            // Dropping the async wrapper must not detach a non-cancellable
            // blocking call and release its permits early.
            let _active = active;
            let _ = tokio::task::spawn_blocking(work).await;
        });
        Ok(())
    }

    /// Admit a typed blocking call and deliver its result asynchronously.
    ///
    /// The completion runs exactly once for every successful admission,
    /// including connection/family cancellation before launch and a panicked
    /// blocking call. It is deliberately invoked by the tracked wrapper, so a
    /// caller can transfer the result into its reader-owned state without
    /// awaiting an active permit on that reader.
    pub(crate) fn spawn_blocking_result<D, F, R>(
        &self,
        request_bytes: usize,
        launch_cancellation: LaunchCancellation,
        complete: D,
        work: F,
    ) -> Result<(), AdmissionError>
    where
        D: FnOnce(Result<R, RunError>) + Send + 'static,
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let pending = self.reserve(request_bytes)?;
        let task = TaskGuard::new(self.inner.clone());
        tokio::spawn(async move {
            let _task = task;
            let outcome = match pending.activate_with(&launch_cancellation).await {
                Some(active) if !launch_cancellation.is_cancelled() => {
                    // Keep both permits until the native call has returned.
                    let _active = active;
                    tokio::task::spawn_blocking(work)
                        .await
                        .map_err(|_| RunError::Panicked)
                }
                Some(_) | None => Err(RunError::Cancelled),
            };
            complete(outcome);
        });
        Ok(())
    }

    /// Stop pending launches and wait until every admitted call has actually
    /// returned.  Active blocking calls are joined rather than aborted.
    pub(crate) async fn cancel_and_drain(&self) {
        self.inner.cancellation.cancel();
        loop {
            let notified = self.inner.drained.notified();
            if self.inner.tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn reserve(&self, bytes: usize) -> Result<PendingGuard, AdmissionError> {
        // Lock order is always endpoint then global.  No path takes the locks
        // in reverse, and neither is held over an await.
        let mut endpoint = self.inner.usage.lock().unwrap();
        let mut global = self.inner.global.inner.usage.lock().unwrap();
        let limits = self.inner.global.inner.limits;
        let fits = endpoint.pending < limits.pending_per_endpoint
            && global.pending < limits.pending_global
            && endpoint.bytes.saturating_add(bytes) <= limits.bytes_per_endpoint
            && global.bytes.saturating_add(bytes) <= limits.bytes_global;
        if !fits {
            drop(global);
            drop(endpoint);
            self.inner.cancellation.cancel_resource_limit();
            return Err(AdmissionError);
        }
        endpoint.pending += 1;
        endpoint.bytes += bytes;
        global.pending += 1;
        global.bytes += bytes;
        drop(global);
        drop(endpoint);
        Ok(PendingGuard {
            endpoint: self.inner.clone(),
            bytes,
            reserved: true,
        })
    }

    #[cfg(test)]
    fn usage(&self) -> (usize, usize, usize) {
        let usage = self.inner.usage.lock().unwrap();
        (usage.pending, usage.active, usage.bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunError {
    Cancelled,
    Panicked,
}

struct PendingGuard {
    endpoint: Arc<EndpointInner>,
    bytes: usize,
    reserved: bool,
}

impl PendingGuard {
    async fn activate(mut self) -> Option<ActiveGuard> {
        self.activate_with_optional(None).await
    }

    async fn activate_with(mut self, cancellation: &LaunchCancellation) -> Option<ActiveGuard> {
        self.activate_with_optional(Some(cancellation)).await
    }

    async fn activate_with_optional(
        &mut self,
        cancellation: Option<&LaunchCancellation>,
    ) -> Option<ActiveGuard> {
        let family_cancelled = async {
            match cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(family_cancelled);
        // Endpoint first: a saturated endpoint never hoards a server-wide
        // permit while waiting for its own slot.
        let endpoint_permit = tokio::select! {
            _ = self.endpoint.cancellation.cancelled() => return None,
            _ = &mut family_cancelled => return None,
            permit = self.endpoint.active.clone().acquire_owned() => permit.ok()?,
        };
        if self.endpoint.cancellation.is_cancelled()
            || cancellation.is_some_and(LaunchCancellation::is_cancelled)
        {
            return None;
        }
        let global_permit = tokio::select! {
            _ = self.endpoint.cancellation.cancelled() => return None,
            _ = &mut family_cancelled => return None,
            permit = self.endpoint.global.inner.active.clone().acquire_owned() => permit.ok()?,
        };
        if self.endpoint.cancellation.is_cancelled()
            || cancellation.is_some_and(LaunchCancellation::is_cancelled)
        {
            return None;
        }

        let mut endpoint = self.endpoint.usage.lock().unwrap();
        let mut global = self.endpoint.global.inner.usage.lock().unwrap();
        debug_assert!(endpoint.pending > 0 && global.pending > 0);
        endpoint.pending -= 1;
        endpoint.active += 1;
        global.pending -= 1;
        global.active += 1;
        drop(global);
        drop(endpoint);
        self.reserved = false;
        Some(ActiveGuard {
            endpoint: self.endpoint.clone(),
            bytes: self.bytes,
            _endpoint_permit: endpoint_permit,
            _global_permit: global_permit,
        })
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.reserved {
            return;
        }
        let mut endpoint = self.endpoint.usage.lock().unwrap();
        let mut global = self.endpoint.global.inner.usage.lock().unwrap();
        debug_assert!(endpoint.pending > 0 && global.pending > 0);
        debug_assert!(endpoint.bytes >= self.bytes && global.bytes >= self.bytes);
        endpoint.pending -= 1;
        endpoint.bytes -= self.bytes;
        global.pending -= 1;
        global.bytes -= self.bytes;
    }
}

struct ActiveGuard {
    endpoint: Arc<EndpointInner>,
    bytes: usize,
    _endpoint_permit: OwnedSemaphorePermit,
    _global_permit: OwnedSemaphorePermit,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut endpoint = self.endpoint.usage.lock().unwrap();
        let mut global = self.endpoint.global.inner.usage.lock().unwrap();
        debug_assert!(endpoint.active > 0 && global.active > 0);
        debug_assert!(endpoint.bytes >= self.bytes && global.bytes >= self.bytes);
        endpoint.active -= 1;
        endpoint.bytes -= self.bytes;
        global.active -= 1;
        global.bytes -= self.bytes;
    }
}

struct TaskGuard {
    endpoint: Arc<EndpointInner>,
}

impl TaskGuard {
    fn new(endpoint: Arc<EndpointInner>) -> Self {
        endpoint.tasks.fetch_add(1, Ordering::AcqRel);
        Self { endpoint }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let previous = self.endpoint.tasks.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        if previous == 1 {
            self.endpoint.drained.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn limits() -> Limits {
        Limits {
            active_per_endpoint: 1,
            active_global: 1,
            pending_per_endpoint: 2,
            pending_global: 3,
            bytes_per_endpoint: 8,
            bytes_global: 12,
        }
    }

    #[tokio::test]
    async fn pending_active_and_bytes_transfer_without_a_gap() {
        let global = GlobalTracker::new(limits());
        let endpoint = global.endpoint(ConnectionCancellation::default());
        let first = endpoint.reserve(5).unwrap().activate().await.unwrap();
        assert_eq!(endpoint.usage(), (0, 1, 5));

        let second = endpoint.reserve(3).unwrap();
        assert_eq!(endpoint.usage(), (1, 1, 8));
        assert!(endpoint.reserve(1).is_err());
        assert_eq!(
            endpoint.inner.cancellation.failure(),
            Some(super::super::ConnectionFailure::ResourceLimit)
        );

        drop(second);
        assert_eq!(endpoint.usage(), (0, 1, 5));
        drop(first);
        assert_eq!(endpoint.usage(), (0, 0, 0));
    }

    #[tokio::test]
    async fn global_limits_cover_distinct_endpoints() {
        let global = GlobalTracker::new(limits());
        let one = global.endpoint(ConnectionCancellation::default());
        let two = global.endpoint(ConnectionCancellation::default());
        let active = one.reserve(7).unwrap().activate().await.unwrap();
        let pending = two.reserve(5).unwrap();
        assert!(two.reserve(1).is_err());
        drop(pending);
        drop(active);
        assert_eq!(global.inner.usage.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn cancellation_drops_pending_work_before_launch() {
        let global = GlobalTracker::new(limits());
        let cancellation = ConnectionCancellation::default();
        let endpoint = global.endpoint(cancellation.clone());
        let active = endpoint.reserve(1).unwrap().activate().await.unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_work = ran.clone();
        endpoint
            .spawn_blocking(1, move || {
                ran_work.store(true, Ordering::Release);
            })
            .unwrap();
        cancellation.cancel();
        drop(active);
        endpoint.cancel_and_drain().await;
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(endpoint.usage(), (0, 0, 0));
    }

    #[tokio::test]
    async fn typed_completion_cancels_while_active_admission_is_saturated() {
        let global = GlobalTracker::new(limits());
        let endpoint = global.endpoint(ConnectionCancellation::default());
        let active = endpoint.reserve(1).unwrap().activate().await.unwrap();
        let cancellation = LaunchCancellation::default();
        let launch_cancellation = cancellation.clone();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_work = ran.clone();
        let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
        endpoint
            .spawn_blocking_result(
                2,
                launch_cancellation,
                move |outcome| completed_tx.send(outcome).unwrap(),
                move || ran_work.store(true, Ordering::Release),
            )
            .unwrap();
        while endpoint.usage().0 != 1 {
            tokio::task::yield_now().await;
        }
        assert_eq!(endpoint.usage(), (1, 1, 3));
        assert!(completed_rx.try_recv().is_err());

        cancellation.cancel();
        drop(active);
        assert_eq!(completed_rx.recv().await, Some(Err(RunError::Cancelled)));
        endpoint.cancel_and_drain().await;
        assert!(!ran.load(Ordering::Acquire));
        assert_eq!(endpoint.usage(), (0, 0, 0));
    }

    #[tokio::test]
    async fn cleanup_joins_a_non_cooperative_blocking_call() {
        let global = GlobalTracker::new(limits());
        let endpoint = global.endpoint(ConnectionCancellation::default());
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let started_work = started.clone();
        let release_work = release.clone();
        endpoint
            .spawn_blocking(1, move || {
                started_work.store(true, Ordering::Release);
                while !release_work.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            })
            .unwrap();
        while !started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        let draining = endpoint.clone();
        let mut drain = Box::pin(async move { draining.cancel_and_drain().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut drain)
                .await
                .is_err()
        );
        release.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .unwrap();
        assert_eq!(endpoint.usage(), (0, 0, 0));
    }
}
