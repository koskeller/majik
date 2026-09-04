//! Crash reports the way Zed's `crashes` crate makes them: the app spawns a second copy of its
//! own binary as a crash server (`majik --crash-handler <socket>`), talks to it over a local socket
//! (a Unix socket, a named pipe on Windows), and when it panics or takes a fatal signal asks it for
//! a [minidump](https://learn.microsoft.com/en-us/windows/win32/debug/minidump-files). A separate
//! process can walk the dying one's memory after its heap is gone; the crashed process itself
//! can't. The server writes `<session_id>.dmp` (zstd-compressed) and `<session_id>.json` (a
//! [`CrashInfo`]) to the logs folder and exits; the app uploads the pair on its next launch if
//! crash reports are on (`majik_app::reliability`).
//!
//! `crash-handler` and `minidumper` (Embark) implement the signal / exception handlers and the
//! IPC on macOS, Windows and Linux, so nothing here is platform-specific beyond a few details:
//! spawning the sidecar on Windows without the busy cursor, suspending the other threads on macOS
//! so the dump is consistent, and recovering glibc's abort message on Linux.

use crash_handler::{CrashEventResult, CrashHandler};
use minidumper::{LoopAction, MinidumpBinary, Server, SocketName};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs, io, panic, process, thread};
use std::fs::File;
use std::path::{Path, PathBuf};

pub use minidumper::Client;

/// The server exits once the app has been silent this long: the app pings every 10 s, so silence
/// means it is gone without a crash (killed, or the machine slept through it).
const CRASH_HANDLER_PING_TIMEOUT: Duration = Duration::from_secs(60);
/// The server exits if nothing connects this soon after it started.
const CRASH_HANDLER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a Dev build does instead of the sidecar: print a backtrace on panic and, on macOS, exit
/// rather than abort so Apple's crash dialog stays away.
pub fn force_backtrace() {
    let old_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
        old_hook(info);
        if cfg!(target_os = "macos") {
            process::exit(1);
        }
    }));
}

/// Spawn the crash server, connect to it, install the panic hook and the signal handlers, and
/// keep the connection alive. All of it happens in the returned future, so it runs on whichever
/// executor polls it; the keepalive loop is handed to `spawn` so the caller picks its executor.
/// `socket_path` names the socket for this process id; `wait_timer` sleeps on that executor.
pub fn init<F, S, C, P>(crash_init: InitCrashHandler, spawn: S, socket_path: P, wait_timer: C) -> impl Future<Output = anyhow::Result<Arc<Client>>>
where
    F: Future<Output = ()> + Send + Sync + 'static,
    C: (Fn(Duration) -> F) + Send + Sync + 'static,
    S: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    P: FnOnce(u32) -> PathBuf,
{
    connect_and_keepalive(crash_init, socket_path, wait_timer, spawn)
}

async fn connect_and_keepalive<F, C, S, P>(crash_init: InitCrashHandler, socket_path: P, wait_timer: C, spawn: S) -> anyhow::Result<Arc<Client>>
where
    F: Future<Output = ()> + Send + Sync + 'static,
    C: (Fn(Duration) -> F) + Send + Sync + 'static,
    S: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    P: FnOnce(u32) -> PathBuf,
{
    let exe = env::current_exe()?;
    let socket_path = socket_path(process::id());
    if let Some(dir) = socket_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let _crash_handler = spawn_crash_handler(&exe, &socket_path)?;
    tracing::info!(target: "majik", "spawning the crash handler process");
    let mut elapsed = Duration::ZERO;
    let retry_frequency = Duration::from_millis(100);
    let client = loop {
        if let Ok(client) = Client::with_name(SocketName::Path(&socket_path)) {
            tracing::info!(target: "majik", "connected to the crash handler after {elapsed:?}");
            break client;
        }
        if elapsed > CRASH_HANDLER_CONNECT_TIMEOUT {
            anyhow::bail!("the crash handler did not come up within {CRASH_HANDLER_CONNECT_TIMEOUT:?}");
        }
        elapsed += retry_frequency;
        wait_timer(retry_frequency).await;
    };
    let client = Arc::new(client);

    panic::set_hook({
        let client = client.clone();
        Box::new(move |payload| panic_hook(client.clone(), payload.payload_as_str().unwrap_or("Box<Any>"), payload.location()))
    });
    let handler = CrashHandler::attach(unsafe {
        let client = client.clone();
        let handler = move |crash_context: &crash_handler::CrashContext| {
            // Set when the first minidump is requested, so a second crashing thread doesn't ask
            // for another.
            static REQUESTED_MINIDUMP: AtomicBool = AtomicBool::new(false);
            let res = if REQUESTED_MINIDUMP.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                #[cfg(target_os = "macos")]
                macos::suspend_all_other_threads();
                // On macOS the ping makes sure every `send_message` before it (the panic, the GPU)
                // has been processed before the dump is requested.
                client.ping().ok();
                let r = client.request_dump(crash_context);
                if let Err(e) = &r {
                    eprintln!("failed to request a minidump: {e:?}");
                }
                #[cfg(target_os = "macos")]
                macos::resume_all_other_threads();
                r.is_ok()
            } else {
                true
            };
            CrashEventResult::Handled(res)
        };
        crash_handler::make_crash_event(handler)
    })
    .map_err(|e| anyhow::anyhow!("attaching the crash signal handler: {e}"))?;
    tracing::info!(target: "majik", "crash signal handlers installed");
    send_crash_server_message(&client, CrashServerMessage::Init(crash_init));

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(address) = abort_message_address() {
        send_crash_server_message(&client, CrashServerMessage::AbortMessageLocation(AbortMessageLocation { pid: process::id(), address }));
    }

    #[cfg(target_os = "linux")]
    handler.set_ptracer(Some(_crash_handler.id()));

    spawn(Box::pin({
        let client = client.clone();
        async move {
            // The handler lives as long as the keepalive loop; dropping it would detach the
            // signal handlers.
            let _handler = handler;
            loop {
                if let Err(e) = client.ping() {
                    tracing::error!(target: "majik", "the crash handler stopped answering: {e:?}");
                    break;
                }
                wait_timer(Duration::from_secs(10)).await;
            }
        }
    }));
    Ok(client)
}

/// Everything the server knows when it writes the report. Serialised beside the minidump and
/// uploaded as the `metadata` part of `POST <base>/crashes`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct CrashInfo {
    pub init: InitCrashHandler,
    /// The panic, when the crash was one (a segfault has none).
    pub panic: Option<CrashPanic>,
    /// Why no usable minidump was written, if the dump failed.
    pub minidump_error: Option<String>,
    /// The diagnostic the C runtime recorded before aborting the process, e.g. glibc's
    /// "free(): invalid pointer". Only present when the crash was a runtime-initiated abort rather
    /// than a signal like SIGSEGV or a panic.
    #[serde(default)]
    pub abort_message: Option<String>,
    /// The GPU the app was drawing with, once it had a window to ask.
    pub active_gpu: Option<GpuSpecs>,
}

/// Sent by the app at startup: what to stamp the report with.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct InitCrashHandler {
    pub session_id: String,
    pub app_version: String,
    pub binary: String,
    pub release_channel: String,
    /// The commit the binary was built from, or `None` for a build without a stamp, which the
    /// uploader skips because nothing could symbolicate it.
    pub commit_sha: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct CrashPanic {
    pub message: String,
    /// `file:line` of the panic.
    pub span: String,
}

/// gpui's `GpuSpecs`, redeclared here so this crate stays free of gpui.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct GpuSpecs {
    pub is_software_emulated: bool,
    pub device_name: String,
    pub driver_name: String,
    pub driver_info: String,
}

/// Where to find the C runtime's abort diagnostic in the crashed process's memory. Sent by the
/// client at startup so that after a crash the server can recover the message with
/// `process_vm_readv`; the crashed process itself can't safely do this work, since its heap may be
/// corrupt and its allocator locks may be held by the crashed thread.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
pub struct AbortMessageLocation {
    pub pid: u32,
    pub address: u64,
}

#[derive(Serialize, Deserialize, Debug)]
enum CrashServerMessage {
    Init(InitCrashHandler),
    Panic(CrashPanic),
    GpuInfo(GpuSpecs),
    AbortMessageLocation(AbortMessageLocation),
    Shutdown,
}

fn send_crash_server_message(crash_client: &Arc<Client>, message: CrashServerMessage) {
    let data = match serde_json::to_vec(&message) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(target: "majik", "serialising a crash server message: {e:?}");
            return;
        }
    };
    if let Err(e) = crash_client.send_message(0, data) {
        tracing::warn!(target: "majik", "sending to the crash server: {e:?}");
    }
}

/// Tell the server which GPU the app draws with, once a window exists to ask.
pub fn set_gpu_info(crash_client: &Arc<Client>, specs: GpuSpecs) {
    send_crash_server_message(crash_client, CrashServerMessage::GpuInfo(specs));
}

/// Ask the server to exit: the app is quitting normally.
pub fn shutdown_crash_handler(crash_client: &Arc<Client>) {
    send_crash_server_message(crash_client, CrashServerMessage::Shutdown);
}

/// The panic hook the app installs once connected: record the panic with the server, then crash
/// for real so the signal handler produces a minidump.
pub fn panic_hook(crash_client: Arc<Client>, message: &str, location: Option<&Location>) {
    let message = strip_user_string_from_panic(message);
    let span = location.map(|loc| format!("{}:{}", loc.file(), loc.line())).unwrap_or_default();
    let current_thread = thread::current();
    let thread_name = current_thread.name().unwrap_or("<unnamed>");
    let location = location.map_or_else(|| "<unknown>".to_owned(), |location| location.to_string());
    tracing::error!(target: "majik", "thread '{thread_name}' panicked at {location}:\n{message}");
    send_crash_server_message(&crash_client, CrashServerMessage::Panic(CrashPanic { message, span }));
    tracing::error!(target: "majik", "triggering a crash to generate a minidump...");

    #[cfg(target_os = "macos")]
    macos::set_panic_thread_id();
    #[cfg(target_os = "windows")]
    {
        // https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499-
        CrashHandler.simulate_exception(Some(234)); // (MORE_DATA_AVAILABLE)
    }
    #[cfg(not(target_os = "windows"))]
    {
        process::abort();
    }
}

/// Rust's string-slicing panics embed the user's string content in the message, e.g. "byte index
/// 4 is out of bounds of `a`". Strip that suffix so a prompt never rides along in a crash report.
fn strip_user_string_from_panic(message: &str) -> String {
    const STRING_PANIC_PREFIXES: &[&str] = &[
        // Older rustc (pre-1.95):
        "byte index ",
        "begin <= end (",
        // Newer rustc (1.95+): https://github.com/rust-lang/rust/pull/145024
        "start byte index ",
        "end byte index ",
        "begin > end (",
    ];
    if (message.ends_with('`') || message.ends_with("`[...]")) && STRING_PANIC_PREFIXES.iter().any(|prefix| message.starts_with(prefix)) {
        if let Some(open) = message.find('`') {
            return format!("{} `<redacted>`", message[..open].trim_end());
        }
    }
    message.to_owned()
}

#[cfg(not(target_os = "windows"))]
fn spawn_crash_handler(exe: &Path, socket_name: &Path) -> io::Result<process::Child> {
    process::Command::new(exe).arg("--crash-handler").arg(socket_name).spawn()
}

/// Returns the handler's process id: there is no `Child` to hold on Windows, and the handles are
/// closed at once since nothing waits on the process.
#[cfg(target_os = "windows")]
fn spawn_crash_handler(exe: &Path, socket_name: &Path) -> io::Result<u32> {
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PWSTR;
    use windows::Win32::System::Threading::{CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_FORCEOFFFEEDBACK, STARTUPINFOW};

    let mut command_line: Vec<u16> =
        OsStr::new(&format!("\"{}\" --crash-handler \"{}\"", exe.display(), socket_name.display())).encode_wide().chain(once(0)).collect();
    // Windows shows a "busy" cursor for a freshly launched GUI process until it pumps window
    // messages, which the crash handler never does, so turn the feedback off.
    let startup_info = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, dwFlags: STARTF_FORCEOFFFEEDBACK, ..Default::default() };
    let mut process_info = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(None, Some(PWSTR(command_line.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, None, &startup_info, &mut process_info)
            .map_err(|e| io::Error::other(e.to_string()))?;
        windows::Win32::Foundation::CloseHandle(process_info.hProcess).ok();
        windows::Win32::Foundation::CloseHandle(process_info.hThread).ok();
    }
    Ok(process_info.dwProcessId)
}

/// The server side, run by `majik --crash-handler <socket>`: serve one client, write its dump and
/// report into `logs_dir` when it crashes, and exit.
pub fn crash_server(socket: &Path, logs_dir: PathBuf) -> anyhow::Result<()> {
    let mut server = Server::with_name(SocketName::Path(socket)).map_err(|e| anyhow::anyhow!("creating the crash server socket (is one already running?): {e}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let has_connection = Arc::new(AtomicBool::new(false));
    thread::Builder::new().name("CrashServerTimeout".to_owned()).spawn({
        let shutdown = shutdown.clone();
        let has_connection = has_connection.clone();
        move || {
            thread::sleep(CRASH_HANDLER_CONNECT_TIMEOUT);
            if !has_connection.load(Ordering::SeqCst) {
                shutdown.store(true, Ordering::SeqCst);
            }
        }
    })?;
    server
        .run(
            Box::new(CrashServer {
                initialization_params: Mutex::default(),
                panic_info: Mutex::default(),
                active_gpu: Mutex::default(),
                abort_message_location: Mutex::default(),
                shutdown: shutdown.clone(),
                has_connection,
                logs_dir,
            }),
            &shutdown,
            Some(CRASH_HANDLER_PING_TIMEOUT),
        )
        .map_err(|e| anyhow::anyhow!("running the crash server: {e}"))
}

struct CrashServer {
    initialization_params: Mutex<Option<InitCrashHandler>>,
    panic_info: Mutex<Option<CrashPanic>>,
    active_gpu: Mutex<Option<GpuSpecs>>,
    abort_message_location: Mutex<Option<AbortMessageLocation>>,
    shutdown: Arc<AtomicBool>,
    has_connection: Arc<AtomicBool>,
    logs_dir: PathBuf,
}

impl CrashServer {
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `<logs>/<session_id>.<extension>`, or a name the server can still write under when the
    /// app never introduced itself.
    fn report_path(&self, extension: &str) -> PathBuf {
        let session = Self::lock(&self.initialization_params).as_ref().map(|init| init.session_id.clone()).unwrap_or_else(|| "unknown-session".to_owned());
        self.logs_dir.join(session).with_extension(extension)
    }
}

impl minidumper::ServerHandler for CrashServer {
    fn create_minidump_file(&self) -> Result<(File, PathBuf), io::Error> {
        fs::create_dir_all(&self.logs_dir)?;
        let dump_path = self.report_path("dmp");
        let file = File::create(&dump_path)?;
        Ok((file, dump_path))
    }

    fn on_minidump_created(&self, result: Result<MinidumpBinary, minidumper::Error>) -> LoopAction {
        let minidump_error = match result {
            Ok(MinidumpBinary { mut file, path, .. }) => {
                use io::Write as _;
                file.flush().ok();
                drop(file);
                compress_in_place(&path).err().map(|e| format!("compressing the minidump: {e}"))
            }
            Err(e) => Some(format!("{e:?}")),
        };

        // The crashed process is still alive at this point: it stays parked in its signal
        // handler until the server acknowledges the dump request, which happens after this
        // callback returns.
        #[cfg(target_os = "linux")]
        let abort_message = (*Self::lock(&self.abort_message_location)).and_then(read_abort_message);
        #[cfg(not(target_os = "linux"))]
        let abort_message = None;

        let Some(init) = Self::lock(&self.initialization_params).clone() else {
            tracing::warn!(target: "majik", "a crash before the app introduced itself; no report written");
            return LoopAction::Exit;
        };
        let crash_info = CrashInfo { init, panic: Self::lock(&self.panic_info).clone(), minidump_error, abort_message, active_gpu: Self::lock(&self.active_gpu).clone() };
        let crash_data_path = self.report_path("json");
        match serde_json::to_vec(&crash_info) {
            Ok(json) => {
                if let Err(e) = fs::write(&crash_data_path, json) {
                    tracing::warn!(target: "majik", "writing {}: {e}", crash_data_path.display());
                }
            }
            Err(e) => tracing::warn!(target: "majik", "serialising the crash report: {e}"),
        }
        LoopAction::Exit
    }

    fn on_message(&self, _: u32, buffer: Vec<u8>) {
        let message: CrashServerMessage = match serde_json::from_slice(&buffer) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!(target: "majik", "an unreadable crash server message: {e}");
                return;
            }
        };
        match message {
            CrashServerMessage::Init(init_data) => {
                Self::lock(&self.initialization_params).replace(init_data);
            }
            CrashServerMessage::Panic(crash_panic) => {
                Self::lock(&self.panic_info).replace(crash_panic);
            }
            CrashServerMessage::GpuInfo(gpu_specs) => {
                Self::lock(&self.active_gpu).replace(gpu_specs);
            }
            CrashServerMessage::AbortMessageLocation(location) => {
                Self::lock(&self.abort_message_location).replace(location);
            }
            CrashServerMessage::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
            }
        }
    }

    fn on_client_disconnected(&self, _clients: usize) -> LoopAction {
        LoopAction::Exit
    }

    fn on_client_connected(&self, _clients: usize) -> LoopAction {
        self.has_connection.store(true, Ordering::SeqCst);
        LoopAction::Continue
    }
}

/// Replace the dump at `path` with its zstd-compressed bytes (a raw dump is tens of megabytes).
fn compress_in_place(path: &Path) -> io::Result<()> {
    let original = File::open(path)?;
    let compressed_path = path.with_extension("zstd");
    let compressed = File::create(&compressed_path)?;
    zstd::stream::copy_encode(original, compressed, 0)?;
    fs::rename(&compressed_path, path)
}

/// glibc records the diagnostic it prints just before aborting (malloc integrity failures like
/// "free(): invalid pointer", assertion failures, stack-smashing reports) in the private global
/// `__abort_msg`, specifically so it can be recovered post-mortem. Resolve its address here, in a
/// safe context at startup. The symbol is only exported at the GLIBC_PRIVATE version, which plain
/// `dlsym` won't resolve, and it has no stability guarantee, so a null result (e.g. musl, or a
/// future glibc removing it) just disables this diagnostic.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn abort_message_address() -> Option<u64> {
    let ptr = unsafe { libc::dlvsym(libc::RTLD_DEFAULT, c"__abort_msg".as_ptr(), c"GLIBC_PRIVATE".as_ptr()) };
    std::ptr::NonNull::new(ptr).map(|ptr| ptr.as_ptr() as u64)
}

/// Read the crashed process's abort diagnostic. `__abort_msg` points to a
/// `struct abort_msg_s { unsigned int size; char msg[]; }` that glibc allocates with mmap so that
/// it stays intact even when the heap is corrupt. `size` is the total byte size of that mapping
/// (header included, rounded up to whole pages), not the message length; the message itself is
/// NUL-terminated.
#[cfg(target_os = "linux")]
fn read_abort_message(location: AbortMessageLocation) -> Option<String> {
    let pointer_bytes = read_process_memory(location.pid, location.address, size_of::<usize>())?;
    let message_address = usize::from_ne_bytes(pointer_bytes.try_into().ok()?) as u64;
    if message_address == 0 {
        return None;
    }
    let size_bytes = read_process_memory(location.pid, message_address, size_of::<u32>())?;
    let size = u32::from_ne_bytes(size_bytes.try_into().ok()?);
    let message_bytes = read_process_memory(location.pid, message_address + size_of::<u32>() as u64, abort_message_read_len(size)?)?;
    parse_abort_message(&message_bytes)
}

/// How many message bytes to read given the `size` field of glibc's `abort_msg_s`. `size` holds
/// the total size of the mmap'd allocation, so a value that isn't a whole number of pages means
/// the layout has changed and we shouldn't trust it. Reading is capped at (one page minus the
/// header), which both bounds the work and ensures the read never extends past the end of the
/// mapping.
#[cfg(any(target_os = "linux", test))]
fn abort_message_read_len(size: u32) -> Option<usize> {
    // Every Linux page size (4 KiB, 16 KiB, 64 KiB, ...) is a multiple of 4 KiB.
    const PAGE_MULTIPLE: usize = 4096;
    const MAX_READ: usize = 4096;
    let size = size as usize;
    if size == 0 || !size.is_multiple_of(PAGE_MULTIPLE) {
        tracing::warn!(target: "majik", "__abort_msg size field {size} is not page-rounded; layout may have changed");
        return None;
    }
    Some(size.min(MAX_READ) - size_of::<u32>())
}

/// The message is NUL-terminated inside a zero-filled mapping, so truncate at the first NUL;
/// `trim` alone would keep the padding, since NUL is not whitespace.
#[cfg(any(target_os = "linux", test))]
fn parse_abort_message(bytes: &[u8]) -> Option<String> {
    let len = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    let message = String::from_utf8_lossy(&bytes[..len]).trim().to_string();
    (!message.is_empty()).then_some(message)
}

#[cfg(target_os = "linux")]
fn read_process_memory(pid: u32, address: u64, len: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    let local = libc::iovec { iov_base: buffer.as_mut_ptr().cast(), iov_len: len };
    let remote = libc::iovec { iov_base: address as *mut libc::c_void, iov_len: len };
    let bytes_read = unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if bytes_read < 0 {
        tracing::warn!(target: "majik", "process_vm_readv of {len} bytes at {address:#x} in pid {pid} failed: {}", io::Error::last_os_error());
        return None;
    }
    if bytes_read as usize != len {
        tracing::warn!(target: "majik", "process_vm_readv short read at {address:#x} in pid {pid}: {bytes_read} of {len} bytes");
        return None;
    }
    Some(buffer)
}

#[cfg(target_os = "macos")]
mod macos {
    static PANIC_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    pub(super) fn set_panic_thread_id() {
        PANIC_THREAD_ID.store(unsafe { mach2::mach_init::mach_thread_self() }, std::sync::atomic::Ordering::Release);
    }

    /// Park every other thread while the dump is taken, so the stacks it records are consistent.
    pub(super) unsafe fn suspend_all_other_threads() {
        let task = unsafe { mach2::traps::current_task() };
        let mut threads: mach2::mach_types::thread_act_array_t = std::ptr::null_mut();
        let mut count = 0;
        unsafe {
            mach2::task::task_threads(task, &raw mut threads, &raw mut count);
        }
        let current = unsafe { mach2::mach_init::mach_thread_self() };
        for i in 0..count {
            let t = unsafe { *threads.add(i as usize) };
            if t != current {
                unsafe { mach2::thread_act::thread_suspend(t) };
            }
        }
    }

    pub(super) unsafe fn resume_all_other_threads() {
        let task = unsafe { mach2::traps::current_task() };
        let mut threads: mach2::mach_types::thread_act_array_t = std::ptr::null_mut();
        let mut count = 0;
        unsafe {
            mach2::task::task_threads(task, &raw mut threads, &raw mut count);
        }
        let current = unsafe { mach2::mach_init::mach_thread_self() };
        for i in 0..count {
            let t = unsafe { *threads.add(i as usize) };
            if t != current {
                unsafe { mach2::thread_act::thread_resume(t) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_is_stripped_from_string_slicing_panics() {
        assert_eq!(strip_user_string_from_panic("byte index 4 is out of bounds of `a secret prompt`"), "byte index 4 is out of bounds of `<redacted>`");
        assert_eq!(strip_user_string_from_panic("begin <= end (4 <= 2) when slicing `hello`"), "begin <= end (4 <= 2) when slicing `<redacted>`");
        assert_eq!(strip_user_string_from_panic("start byte index 10 is out of bounds of `abc`[...]"), "start byte index 10 is out of bounds of `<redacted>`");
        assert_eq!(strip_user_string_from_panic("end byte index 1 is not a char boundary; it is inside 'é' (bytes 0..2) of `é`"), "end byte index 1 is not a char boundary; it is inside 'é' (bytes 0..2) of `<redacted>`");
        // Everything else passes through: no backticked user text, or not a slicing panic.
        assert_eq!(strip_user_string_from_panic("called `Option::unwrap()` on a `None` value"), "called `Option::unwrap()` on a `None` value");
        assert_eq!(strip_user_string_from_panic("index out of bounds: the len is 3 but the index is 7"), "index out of bounds: the len is 3 but the index is 7");
    }

    #[test]
    fn the_report_round_trips_through_json() {
        let info = CrashInfo {
            init: InitCrashHandler {
                session_id: "s".into(),
                app_version: "0.1.0".into(),
                binary: "majik".into(),
                release_channel: "stable".into(),
                commit_sha: Some("abc".into()),
            },
            panic: Some(CrashPanic { message: "boom".into(), span: "src/main.rs:1".into() }),
            minidump_error: None,
            abort_message: None,
            active_gpu: Some(GpuSpecs { is_software_emulated: false, device_name: "Apple M2".into(), driver_name: "Metal".into(), driver_info: "".into() }),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<CrashInfo>(&json).unwrap(), info);
        // A report written before `abort_message` existed still reads.
        let old = json.replace(",\"abort_message\":null", "");
        assert_eq!(serde_json::from_str::<CrashInfo>(&old).unwrap(), info);
    }

    #[test]
    fn abort_message_read_len_requires_page_rounded_total() {
        assert_eq!(abort_message_read_len(0), None);
        // A message length rather than a mapping total means the glibc layout has changed out
        // from under us.
        assert_eq!(abort_message_read_len(23), None);
        assert_eq!(abort_message_read_len(4097), None);
        // The read must stay within the mapping: one page minus the header.
        assert_eq!(abort_message_read_len(4096), Some(4092));
        // Larger totals (long messages, larger page sizes) are clamped.
        assert_eq!(abort_message_read_len(8192), Some(4092));
        assert_eq!(abort_message_read_len(65536), Some(4092));
    }

    #[test]
    fn parse_abort_message_truncates_at_nul_and_rejects_empty() {
        let mut buffer = b"free(): invalid pointer\n\0".to_vec();
        buffer.resize(4092, 0);
        assert_eq!(parse_abort_message(&buffer), Some("free(): invalid pointer".to_string()));
        assert_eq!(parse_abort_message(b"double free or corruption (out)"), Some("double free or corruption (out)".to_string()));
        assert_eq!(parse_abort_message(&[]), None);
        assert_eq!(parse_abort_message(&[0; 16]), None);
        assert_eq!(parse_abort_message(b"\n \0garbage after nul"), None);
    }

    #[test]
    fn compress_in_place_replaces_the_file_with_zstd() {
        let dir = std::env::temp_dir().join(format!("majik-crashes-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.dmp");
        fs::write(&path, vec![7u8; 100_000]).unwrap();
        compress_in_place(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() < 1000, "compressed: {} bytes", bytes.len());
        assert_eq!(zstd::decode_all(bytes.as_slice()).unwrap(), vec![7u8; 100_000]);
        assert!(!path.with_extension("zstd").exists(), "the temporary name is gone");
        fs::remove_dir_all(dir).ok();
    }

    /// End-to-end check of `read_abort_message` against a synthetic `abort_msg_s` in this very
    /// process (`process_vm_readv` may always read one's own memory). The message page is
    /// followed by a `PROT_NONE` guard page so the test fails if the read ever extends past the
    /// mapping glibc would have allocated.
    #[cfg(target_os = "linux")]
    #[test]
    fn read_abort_message_reads_glibc_layout_from_a_live_process() {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        unsafe {
            let mapping = libc::mmap(std::ptr::null_mut(), 2 * page_size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
            assert_ne!(mapping, libc::MAP_FAILED);
            assert_eq!(libc::mprotect(mapping.cast::<u8>().add(page_size).cast(), page_size, libc::PROT_NONE), 0);
            mapping.cast::<u32>().write(page_size as u32);
            let message = b"free(): invalid pointer\n\0";
            std::ptr::copy_nonoverlapping(message.as_ptr(), mapping.cast::<u8>().add(size_of::<u32>()), message.len());
            // Stands in for the `__abort_msg` global: a pointer variable whose address we hand to
            // the reader.
            let abort_msg: *mut libc::c_void = mapping;
            let location = AbortMessageLocation { pid: process::id(), address: (&raw const abort_msg) as u64 };
            assert_eq!(read_abort_message(location), Some("free(): invalid pointer".to_string()));
            libc::munmap(mapping, 2 * page_size);
        }
    }
}
