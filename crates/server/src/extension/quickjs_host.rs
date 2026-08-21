//! Native QuickJS implementation of the extension guest contract.
//!
//! JavaScript source is compiled eagerly on the attempt's dedicated thread,
//! then evaluated only after the ordinary logical-client bootstrap reaches
//! `EXT_INFO(INIT)`. Packet I/O uses the same bounded handoffs and the same
//! `blit-guest` bootstrap/reassembly implementation as a Wasmi guest.

use super::wasmi_host::{
    AttemptCancellation, AttemptFailure, AttemptOutcome, AttemptShared, FailureKind, HostBridge,
    LifecycleError, NativeHost, WasmiHostConfig, new_attempt_shared,
};
use crate::thread_name::{ThreadNames, extension_thread_names};
use blit_guest::{Client, WaitOutcome, native_host};
use blit_remote::extension::{EXT_EVENT_LOG, msg_extension_event};
use rquickjs::{
    Array, BigInt, CatchResultExt, Context as JsContext, Ctx, Function, Module, Object, Runtime,
    TypedArray, Value, WriteOptions, function::Func, promise::MaybePromise,
};
use std::{
    cell::RefCell,
    fmt,
    rc::Rc,
    sync::{Arc, MutexGuard, atomic::Ordering},
    thread,
    time::Duration,
};
use tokio::sync::oneshot;

const SOURCE_NAME: &str = "extension.js";
const RANDOM_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AttemptSpec {
    pub source: Arc<[u8]>,
    pub module_hash: [u8; 32],
    pub extension_id: u64,
    pub label: Option<String>,
    pub config: WasmiHostConfig,
}

#[derive(Debug)]
pub enum SpawnError {
    InvalidConfig(super::wasmi_host::ConfigError),
    InvalidExtensionId,
    Thread(std::io::Error),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid QuickJS host configuration: {error}"),
            Self::InvalidExtensionId => f.write_str("extension ID must be non-zero"),
            Self::Thread(error) => write!(f, "failed to spawn extension thread: {error}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Owner of one dedicated native QuickJS attempt thread.
#[derive(Debug)]
pub struct QuickJsAttempt {
    names: ThreadNames,
    shared: Arc<AttemptShared>,
    bridge: HostBridge,
    prepared_rx: Option<oneshot::Receiver<Result<(), AttemptFailure>>>,
    prepared: bool,
    started: bool,
    thread: Option<thread::JoinHandle<AttemptOutcome>>,
}

impl QuickJsAttempt {
    pub fn thread_names(&self) -> &ThreadNames {
        &self.names
    }

    pub fn cancellation(&self) -> AttemptCancellation {
        AttemptCancellation {
            inner: Arc::clone(&self.shared),
        }
    }

    pub fn bridge(&self) -> HostBridge {
        self.bridge.clone()
    }

    pub async fn wait_prepared(&mut self) -> Result<(), AttemptFailure> {
        let receiver = self.prepared_rx.take().ok_or_else(|| {
            AttemptFailure::new(
                FailureKind::HostFailure,
                LifecycleError::PreparationAlreadyObserved.to_string(),
            )
        })?;
        let result = receiver.await.map_err(|_| {
            AttemptFailure::new(
                FailureKind::HostFailure,
                LifecycleError::PreparationChannelClosed.to_string(),
            )
        })?;
        if result.is_ok() {
            self.prepared = true;
        }
        result
    }

    pub fn start(&mut self) -> Result<(), LifecycleError> {
        if !self.prepared {
            return Err(LifecycleError::NotPrepared);
        }
        if self.started {
            return Err(LifecycleError::AlreadyStarted);
        }
        self.started = true;
        *lock_unpoison(&self.shared.start) = true;
        self.shared.start_cv.notify_all();
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancellation().cancel();
    }

    pub async fn join(mut self) -> Result<AttemptOutcome, LifecycleError> {
        let handle = self.thread.take().ok_or(LifecycleError::JoinAlreadyTaken)?;
        tokio::task::spawn_blocking(move || {
            handle.join().map_err(|_| LifecycleError::ThreadPanicked)
        })
        .await
        .map_err(|_| LifecycleError::JoinTaskCancelled)?
    }
}

impl Drop for QuickJsAttempt {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.cancel();
        }
    }
}

pub fn spawn_attempt(spec: AttemptSpec) -> Result<QuickJsAttempt, SpawnError> {
    spec.config.validate().map_err(SpawnError::InvalidConfig)?;
    if spec.extension_id == 0 {
        return Err(SpawnError::InvalidExtensionId);
    }
    let names = extension_thread_names(spec.label.as_deref(), &spec.module_hash, spec.extension_id);
    let shared = new_attempt_shared();
    let bridge = HostBridge {
        shared: Arc::clone(&shared),
    };
    let (prepared_tx, prepared_rx) = oneshot::channel();
    let thread_shared = Arc::clone(&shared);
    let stack_size = spec.config.native_stack_bytes;
    let thread = thread::Builder::new()
        .name(names.os.clone())
        .stack_size(stack_size)
        .spawn(move || attempt_thread(spec, thread_shared, prepared_tx))
        .map_err(SpawnError::Thread)?;
    Ok(QuickJsAttempt {
        names,
        shared,
        bridge,
        prepared_rx: Some(prepared_rx),
        prepared: false,
        started: false,
        thread: Some(thread),
    })
}

/// Compile JavaScript without evaluating it. Upload admission uses this on the
/// same bounded validation pool as Wasmi translation.
pub fn validate_source(source: &[u8], config: &WasmiHostConfig) -> Result<(), AttemptFailure> {
    config
        .validate()
        .map_err(|error| AttemptFailure::new(FailureKind::Validation, error.to_string()))?;
    let source = source_text(source)?;
    let (_runtime, _context, _bytecode) = prepare_runtime(source, config, None)?;
    Ok(())
}

fn attempt_thread(
    spec: AttemptSpec,
    shared: Arc<AttemptShared>,
    prepared_tx: oneshot::Sender<Result<(), AttemptFailure>>,
) -> AttemptOutcome {
    let source = match source_text(&spec.source) {
        Ok(source) => source,
        Err(error) => {
            let _ = prepared_tx.send(Err(error.clone()));
            shared.io.abort_handoffs();
            return AttemptOutcome::Failed(error);
        }
    };
    let runner = match PreparedRunner::new(source, &spec.config, Arc::clone(&shared)) {
        Ok(runner) => runner,
        Err(error) => {
            let _ = prepared_tx.send(Err(error.clone()));
            shared.io.abort_handoffs();
            return AttemptOutcome::Failed(error);
        }
    };
    if prepared_tx.send(Ok(())).is_err() {
        shared.io.abort_handoffs();
        return AttemptOutcome::Cancelled;
    }
    let mut started = lock_unpoison(&shared.start);
    while !*started && !shared.io.cancelled.load(Ordering::Acquire) {
        started = shared
            .start_cv
            .wait(started)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    drop(started);
    if shared.io.cancelled.load(Ordering::Acquire) {
        shared.io.abort_handoffs();
        return AttemptOutcome::Cancelled;
    }
    let outcome = runner.run();
    match &outcome {
        AttemptOutcome::Returned(_) => {
            shared.io.outgoing.seal_producer();
            shared.io.incoming.close_consumer();
        }
        AttemptOutcome::Cancelled | AttemptOutcome::Failed(_) => shared.io.abort_handoffs(),
    }
    outcome
}

struct PreparedRunner {
    runtime: Runtime,
    context: JsContext,
    bytecode: Vec<u8>,
    shared: Arc<AttemptShared>,
}

impl PreparedRunner {
    fn new(
        source: &str,
        config: &WasmiHostConfig,
        shared: Arc<AttemptShared>,
    ) -> Result<Self, AttemptFailure> {
        let (runtime, context, bytecode) =
            prepare_runtime(source, config, Some(Arc::clone(&shared)))?;
        Ok(Self {
            runtime,
            context,
            bytecode,
            shared,
        })
    }

    fn run(self) -> AttemptOutcome {
        let Self {
            runtime,
            context,
            bytecode,
            shared,
        } = self;
        let _host = native_host::install(NativeHost::new(Arc::clone(&shared.io)));
        let client = match Client::bootstrap() {
            Ok(client) => Rc::new(RefCell::new(client)),
            Err(error) => {
                return AttemptOutcome::Failed(AttemptFailure::new(
                    FailureKind::AbiMisuse,
                    format!("QuickJS bootstrap failed: {error}"),
                ));
            }
        };
        let result = context.with(|ctx| {
            let result = (|| {
                install_bindings(&ctx, Rc::clone(&client))?;
                // The bytes were produced by this exact QuickJS runtime during
                // preparation and have not crossed a trust boundary.
                let module = unsafe { Module::load(ctx.clone(), &bytecode)? };
                let (module, evaluated) = module.eval()?;
                evaluated.finish::<()>()?;
                let default = module.get::<_, Option<Function>>("default")?;
                let Some(default) = default else {
                    return Ok(0);
                };
                let returned = default.call::<_, MaybePromise>(())?;
                let returned = returned.finish::<Value>()?;
                if returned.is_undefined() {
                    return Ok(0);
                }
                returned.as_int().ok_or_else(|| {
                    rquickjs::Exception::throw_type(
                        &ctx,
                        "default export must return an i32 or undefined",
                    )
                })
            })();
            result.catch(&ctx).map_err(|error| error.to_string())
        });
        drop(client);
        drop(context);
        drop(runtime);
        match result {
            Ok(code) if shared.io.cancelled.load(Ordering::Acquire) => {
                let _ = code;
                AttemptOutcome::Cancelled
            }
            Ok(code) => AttemptOutcome::Returned(code),
            Err(_) if shared.io.cancelled.load(Ordering::Acquire) => AttemptOutcome::Cancelled,
            Err(detail) => AttemptOutcome::Failed(AttemptFailure::new(
                FailureKind::Trap,
                format!("QuickJS exception: {detail}"),
            )),
        }
    }
}

fn prepare_runtime(
    source: &str,
    config: &WasmiHostConfig,
    shared: Option<Arc<AttemptShared>>,
) -> Result<(Runtime, JsContext, Vec<u8>), AttemptFailure> {
    let runtime = Runtime::new().map_err(|error| {
        AttemptFailure::new(
            FailureKind::Instantiation,
            format!("create QuickJS runtime: {error}"),
        )
    })?;
    runtime.set_memory_limit(config.memory_bytes);
    runtime.set_max_stack_size(config.value_stack_bytes);
    if let Some(shared) = shared {
        runtime.set_interrupt_handler(Some(Box::new(move || {
            shared.io.cancelled.load(Ordering::Acquire)
        })));
    }
    let context = JsContext::full(&runtime).map_err(|error| {
        AttemptFailure::new(
            FailureKind::Instantiation,
            format!("create QuickJS context: {error}"),
        )
    })?;
    let bytecode = context.with(|ctx| {
        let result = Module::declare(ctx.clone(), SOURCE_NAME, source)
            .and_then(|module| module.write(WriteOptions::default()));
        result.catch(&ctx).map_err(|error| error.to_string())
    });
    let bytecode = bytecode.map_err(|detail| {
        AttemptFailure::new(
            FailureKind::Validation,
            format!("compile QuickJS source: {detail}"),
        )
    })?;
    Ok((runtime, context, bytecode))
}

fn source_text(source: &[u8]) -> Result<&str, AttemptFailure> {
    std::str::from_utf8(source).map_err(|error| {
        AttemptFailure::new(
            FailureKind::Validation,
            format!("QuickJS source is not UTF-8: {error}"),
        )
    })
}

fn install_bindings<'js>(ctx: &Ctx<'js>, client: Rc<RefCell<Client>>) -> rquickjs::Result<()> {
    let blit = Object::new(ctx.clone())?;
    let context = Object::new(ctx.clone())?;
    let guest = client.borrow();
    let info = guest.context();
    context.set(
        "extensionId",
        BigInt::from_u64(ctx.clone(), info.extension_id)?,
    )?;
    context.set(
        "definitionRevision",
        BigInt::from_u64(ctx.clone(), info.definition_revision)?,
    )?;
    context.set("attempt", BigInt::from_u64(ctx.clone(), info.attempt)?)?;
    context.set("taskId", info.task_id)?;
    context.set("moduleHash", hex_hash(&info.module_hash))?;
    context.set("name", info.name.clone())?;
    let args = Array::new(ctx.clone())?;
    for (index, argument) in info.args.iter().enumerate() {
        args.set(index, argument.as_str())?;
    }
    context.set("args", args)?;
    context.set("detached", info.detached)?;
    context.set("persistent", info.persistent)?;
    context.set("enabled", info.enabled)?;
    context.set("desiredRunning", info.desired_running)?;
    context.set("protocolVersion", info.hello.protocol_version)?;
    context.set("features", info.hello.features)?;
    context.set(
        "bootGeneration",
        info.hello
            .boot_generation
            .map(|value| BigInt::from_u64(ctx.clone(), value))
            .transpose()?,
    )?;
    context.set("serverVersion", info.hello.server_version.clone())?;
    drop(guest);
    blit.set("context", context)?;

    let send_client = Rc::clone(&client);
    blit.set(
        "send",
        Func::from(move |ctx: Ctx<'js>, packet: TypedArray<'js, u8>| {
            send_client
                .borrow_mut()
                .send(packet.as_ref())
                .map_err(|error| js_error(&ctx, "send", error))
        }),
    )?;

    let recv_client = Rc::clone(&client);
    blit.set(
        "recv",
        Func::from(move |ctx: Ctx<'js>| {
            let packet = recv_client
                .borrow_mut()
                .recv()
                .map_err(|error| js_error(&ctx, "recv", error))?;
            packet
                .map(|bytes| TypedArray::new(ctx.clone(), bytes))
                .transpose()
        }),
    )?;

    let wait_client = Rc::clone(&client);
    blit.set(
        "wait",
        Func::from(move |ctx: Ctx<'js>| {
            wait_client
                .borrow()
                .wait()
                .map(wait_code)
                .map_err(|error| js_error(&ctx, "wait", error))
        }),
    )?;

    let wait_until_client = Rc::clone(&client);
    blit.set(
        "waitUntil",
        Func::from(move |ctx: Ctx<'js>, deadline: BigInt<'js>| {
            let deadline = deadline
                .to_i64()
                .map_err(|error| js_error(&ctx, "waitUntil", error))?;
            wait_until_client
                .borrow()
                .wait_until(blit_guest::MonotonicInstant::from_raw_nanos(deadline))
                .map(wait_code)
                .map_err(|error| js_error(&ctx, "waitUntil", error))
        }),
    )?;

    let realtime_client = Rc::clone(&client);
    blit.set(
        "realtimeNow",
        Func::from(move |ctx: Ctx<'js>| {
            BigInt::from_i64(
                ctx,
                realtime_client
                    .borrow()
                    .realtime_now()
                    .unix_timestamp_nanos(),
            )
        }),
    )?;

    let monotonic_client = Rc::clone(&client);
    blit.set(
        "monotonicNow",
        Func::from(move |ctx: Ctx<'js>| {
            BigInt::from_i64(ctx, monotonic_client.borrow().monotonic_now().raw_nanos())
        }),
    )?;

    let random_client = Rc::clone(&client);
    blit.set(
        "random",
        Func::from(move |ctx: Ctx<'js>, length: u32| {
            let length = length as usize;
            if length > RANDOM_MAX_BYTES {
                return Err(rquickjs::Exception::throw_range(
                    &ctx,
                    "random length exceeds 16 MiB",
                ));
            }
            let mut bytes = vec![0; length];
            random_client
                .borrow()
                .random(&mut bytes)
                .map_err(|error| js_error(&ctx, "random", error))?;
            TypedArray::new(ctx, bytes)
        }),
    )?;

    let sleep_client = Rc::clone(&client);
    blit.set(
        "sleep",
        Func::from(move |ctx: Ctx<'js>, milliseconds: f64| {
            if !milliseconds.is_finite() || milliseconds < 0.0 {
                return Err(rquickjs::Exception::throw_range(
                    &ctx,
                    "sleep duration must be a finite non-negative number",
                ));
            }
            let duration = Duration::try_from_secs_f64(milliseconds / 1_000.0).map_err(|_| {
                rquickjs::Exception::throw_range(&ctx, "sleep duration is out of range")
            })?;
            sleep_client
                .borrow_mut()
                .sleep(duration)
                .map_err(|error| js_error(&ctx, "sleep", error))
        }),
    )?;

    let log_client = Rc::clone(&client);
    blit.set(
        "log",
        Func::from(move |ctx: Ctx<'js>, message: String| {
            let packet =
                msg_extension_event(EXT_EVENT_LOG, message.as_bytes()).ok_or_else(|| {
                    rquickjs::Exception::throw_range(&ctx, "log message exceeds protocol limits")
                })?;
            log_client
                .borrow_mut()
                .send(&packet)
                .map_err(|error| js_error(&ctx, "log", error))
        }),
    )?;

    ctx.globals().set("blit", blit)?;
    ctx.eval::<(), _>(
        "globalThis.console = Object.freeze({\n\
         log: (...values) => blit.log(values.map(String).join(' ')),\n\
         error: (...values) => blit.log(values.map(String).join(' '))\n\
         }); Object.freeze(blit.context);",
    )?;
    Ok(())
}

fn wait_code(outcome: WaitOutcome) -> i32 {
    match outcome {
        WaitOutcome::Deadline => 0,
        WaitOutcome::Packet => 1,
        WaitOutcome::Closed => 2,
    }
}

fn js_error(ctx: &Ctx<'_>, operation: &str, error: impl fmt::Display) -> rquickjs::Error {
    rquickjs::Exception::throw_message(ctx, &format!("blit.{operation}: {error}"))
}

fn hex_hash(hash: &[u8; 32]) -> String {
    use fmt::Write as _;
    hash.iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn lock_unpoison<T>(mutex: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::extension::{ExtensionInit, FEATURE_EXTENSION, msg_extension_init};

    const HASH: [u8; 32] = [0x2a; 32];

    fn spec(source: &str) -> AttemptSpec {
        AttemptSpec {
            source: Arc::from(source.as_bytes()),
            module_hash: HASH,
            extension_id: 7,
            label: Some("quickjs-test".into()),
            config: WasmiHostConfig::default(),
        }
    }

    fn hello() -> Vec<u8> {
        let mut packet = vec![0x07];
        packet.extend_from_slice(&1_u16.to_le_bytes());
        packet.extend_from_slice(&FEATURE_EXTENSION.to_le_bytes());
        packet
    }

    fn init() -> Vec<u8> {
        msg_extension_init(&ExtensionInit {
            extension_id: 7,
            definition_revision: 3,
            attempt: 2,
            task_id: 11,
            flags: 0b1111,
            hash: HASH,
            name: "quickjs-test",
            args: vec![b"alpha"],
        })
        .unwrap()
    }

    async fn boot(attempt: &mut QuickJsAttempt) -> HostBridge {
        attempt.wait_prepared().await.unwrap();
        let bridge = attempt.bridge();
        attempt.start().unwrap();
        for packet in [hello(), vec![0x09], init()] {
            bridge
                .reserve_to_guest(packet.len())
                .await
                .unwrap()
                .commit(packet)
                .unwrap();
        }
        bridge
    }

    #[test]
    fn source_validation_rejects_syntax_and_non_utf8() {
        validate_source(b"export default () => 1", &WasmiHostConfig::default()).unwrap();
        let syntax = validate_source(b"export default (", &WasmiHostConfig::default()).unwrap_err();
        assert_eq!(syntax.kind, FailureKind::Validation);
        assert!(syntax.detail.contains("compile QuickJS source"));
        let utf8 = validate_source(&[0xff], &WasmiHostConfig::default()).unwrap_err();
        assert!(utf8.detail.contains("not UTF-8"));
    }

    #[tokio::test]
    async fn default_export_sees_context_and_sends_packets() {
        let mut attempt = spawn_attempt(spec(
            r#"
                export default function () {
                    if (blit.context.extensionId !== 7n) throw new Error("bad id");
                    if (blit.context.definitionRevision !== 3n) throw new Error("bad revision");
                    if (blit.context.args[0] !== "alpha") throw new Error("bad args");
                    blit.send(new Uint8Array([0x44, 0x55]));
                    return 9;
                }
            "#,
        ))
        .unwrap();
        let bridge = boot(&mut attempt).await;
        let packet = bridge.recv_from_guest().await.unwrap();
        assert_eq!(packet.packet(), &[0x44, 0x55]);
        packet.acknowledge();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(9));
    }

    #[tokio::test]
    async fn top_level_only_module_returns_zero() {
        let mut attempt = spawn_attempt(spec("globalThis.quickjsRan = true;")).unwrap();
        boot(&mut attempt).await;
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(0));
    }

    #[tokio::test]
    async fn async_default_export_runs_jobs_and_returns_code() {
        let mut attempt = spawn_attempt(spec(
            "export default async function () { await Promise.resolve(); return 23; }",
        ))
        .unwrap();
        boot(&mut attempt).await;
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(23));
    }

    #[tokio::test]
    async fn non_integer_return_is_a_trap() {
        let mut attempt =
            spawn_attempt(spec("export default function () { return 1.5; }")).unwrap();
        boot(&mut attempt).await;
        let AttemptOutcome::Failed(error) = attempt.join().await.unwrap() else {
            panic!("expected failed attempt");
        };
        assert_eq!(error.kind, FailureKind::Trap);
        assert!(error.detail.contains("must return an i32"));
    }

    #[tokio::test]
    async fn out_of_range_sleep_is_a_trap() {
        let mut attempt = spawn_attempt(spec(
            "export default function () { blit.sleep(Number.MAX_VALUE); }",
        ))
        .unwrap();
        boot(&mut attempt).await;
        let AttemptOutcome::Failed(error) = attempt.join().await.unwrap() else {
            panic!("expected failed attempt");
        };
        assert_eq!(error.kind, FailureKind::Trap);
        assert!(error.detail.contains("sleep duration is out of range"));
    }

    #[tokio::test]
    async fn thrown_exception_is_a_trap() {
        let mut attempt = spawn_attempt(spec(
            "export default function () { throw new Error('broken'); }",
        ))
        .unwrap();
        boot(&mut attempt).await;
        let AttemptOutcome::Failed(error) = attempt.join().await.unwrap() else {
            panic!("expected failed attempt");
        };
        assert_eq!(error.kind, FailureKind::Trap);
        assert!(error.detail.contains("broken"));
    }

    #[tokio::test]
    async fn interrupt_handler_cancels_compute_loop() {
        let mut attempt =
            spawn_attempt(spec("export default function () { while (true) {} }")).unwrap();
        boot(&mut attempt).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        attempt.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(2), attempt.join())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, AttemptOutcome::Cancelled);
    }
}
