//! The WeChat OCR engine, driven without Python.
//!
//! Upstream reached this engine through a Python package that (a) loaded `mmmojo_64.dll`, (b) launched
//! `WeChatOCR.exe` through it, and (c) exchanged two protobuf messages over the resulting channel, with
//! the recognized text arriving on a callback from the DLL's own thread. The binaries were never in this
//! repository and the interpreter is gone, so the engine had been listed as "registered, not driveable" —
//! honest, but still a feature a user cannot have.
//!
//! Three decisions shape this module.
//!
//! **It is a service, not a command line.** Every other engine here is "spawn, feed one image, read
//! stdout". WeChat OCR loads ~21 MB of models per process, so a per-frame spawn would cost far more than
//! the recognition: the design is one resident child, reused for every frame. That is why [`Engine`]
//! carries an execution kind instead of pretending this one is an argv, and why the handle is
//! process-global — a recorder, an indexer and a settings page each starting their own child would run
//! three model-loaded processes on a user's machine.
//!
//! **`mmmojo_64.dll` owns the transport, so nothing here has to.** The DLL creates the named pipe, spawns
//! the child with the right `--mojo-named-platform-channel-pipe` argument, and runs the handshake; the
//! caller sets callbacks, hands over a request blob, and receives one. So there is no pipe code in this
//! file, only the fourteen entry points that are actually used — `Dll::open` resolves them by name, so the
//! list is the whole ABI this module depends on — declared inline for the reason `autostart.rs` gives:
//! this workspace's offline build is the gate, and no FFI crate is being added to it.
//!
//! **The wire format is written out, not generated.** `OcrRequest` is three fields and `OcrResponse` is a
//! nested list of strings; a protobuf compiler and runtime would be a great deal of machinery for that,
//! and the encoding is pinned against the bytes the reference client itself produces — see
//! [`tests::a_request_is_the_bytes_the_reference_client_makes`].
//!
//! ## What has to be on disk
//!
//! `ocr_lib/wxocr-binary/` holding `WeChatOCR.exe`, `mmmojo_64.dll` and `Model/`. That is upstream's own
//! layout, so an install that got WeChat support from the old extension scripts needs no copy step.
//! [`Install::probe`] names whichever piece is missing, and the settings page shows that sentence instead
//! of offering a row that cannot work.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Where the engine's files live under an install root — upstream's path, kept so a migrated install works.
pub const BINARY_DIR: &str = "ocr_lib/wxocr-binary";
const EXE: &str = "WeChatOCR.exe";
const DLL: &str = "mmmojo_64.dll";
const MODELS: &str = "Model";

/// How long to wait for one frame. The reference client waited five seconds and gave up; a cold model load
/// inside the first task is slower than that, and losing a frame is worse than waiting for it.
pub const TASK_TIMEOUT: Duration = Duration::from_secs(60);

/// Frames lost in a row before a caller should stop treating this engine as available.
///
/// The number is here rather than in either caller because it is a property of [`TASK_TIMEOUT`]: a
/// command-line engine fails in well under a second, so retrying it costs nothing, while one minute per
/// frame turns a broken engine into a stopped recorder wearing the costume of a slow one. The live
/// recorder backs off and the back-index gives up; both count against this.
pub const GIVE_UP_AFTER: u32 = 3;

/// Frames a caller should skip before asking a silent engine once more. Long enough that a library is not
/// held to one frame a minute, short enough that an engine which comes back — a WeChat update finishing,
/// the process being restarted by hand — is noticed within a couple of minutes.
pub const RETRY_AFTER_FRAMES: u64 = 40;

/// How many frames in a row this process has lost. A growing number is an engine that should not have been
/// switched on, and a caller that records for hours has to stop asking rather than grind through a library
/// one timeout at a time.
pub fn consecutive_failures() -> u32 {
    slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).failures
}

/// Whether the engine has stopped answering at all. `false` forever on a machine that never used it, since
/// the counter belongs to this engine alone.
pub fn has_gone_quiet() -> bool {
    consecutive_failures() >= GIVE_UP_AFTER
}

/// The channel the OCR request travels on (`RequestIdOCR::OCRPush` upstream).
const OCR_PUSH: u32 = 1;
/// Upstream's task-id pool: 1..=32, one outstanding frame per id.
const TASK_SLOTS: u32 = 32;

// mmmojo's enums, spelled out because the header is not in this repository and the numbers are the ABI.
const METHOD_PUSH: i32 = 1;
const CB_USER_DATA: i32 = 0;
const CB_READ_PUSH: i32 = 1;
const CB_READ_PULL: i32 = 2;
const CB_READ_SHARED: i32 = 3;
const CB_REMOTE_CONNECT: i32 = 4;
const CB_REMOTE_DISCONNECT: i32 = 5;
const CB_PROCESS_LAUNCHED: i32 = 6;
const CB_PROCESS_LAUNCH_FAILED: i32 = 7;
const CB_REMOTE_MOJO_ERROR: i32 = 8;
const PARAM_HOST_PROCESS: i32 = 0;
const PARAM_EXE_PATH: i32 = 2;

/// What this install can run, and which file says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub dir: PathBuf,
    pub exe: PathBuf,
    pub dll: PathBuf,
    /// `None` when everything is present; otherwise the sentence the picker and the note line show.
    pub missing: Option<String>,
}

impl Install {
    /// Look at the disk. Nothing is loaded and no process is started, so a settings page can call this
    /// while it is only deciding whether to offer a row.
    pub fn probe(root: &Path) -> Install {
        let dir = root.join(BINARY_DIR);
        let exe = dir.join(EXE);
        let dll = dir.join(DLL);
        let missing = if !dir.is_dir() {
            Some(format!("{} does not exist", dir.display()))
        } else if !exe.is_file() {
            Some(format!("{EXE} is missing from {}", dir.display()))
        } else if !dll.is_file() {
            Some(format!("{DLL} is missing from {}", dir.display()))
        } else if !dir.join(MODELS).is_dir() {
            Some(format!("the {MODELS} folder is missing from {}", dir.display()))
        } else {
            None
        };
        Install { dir, exe, dll, missing }
    }

    pub fn is_usable(&self) -> bool {
        self.missing.is_none()
    }
}

/// Recognize one image file that is already on disk.
///
/// One process-wide service is shared by every caller, so a recorder and a settings page never fight over
/// two copies of the model. A frame that times out or finds the child gone restarts it for the next one:
/// the recorder indexes for hours, and nothing about WeChat's child promises it will last.
pub fn recognize(install: &Install, image: &Path, timeout: Duration) -> Result<String, String> {
    if let Some(why) = &install.missing {
        return Err(why.clone());
    }
    let absolute = absolute(image)?;
    if !absolute.is_file() {
        return Err(format!("no picture at {absolute:?}"));
    }
    let mut slot = slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.service.is_none() {
        slot.service = Some(Service::start(install)?);
    }
    // The outcome is owned before the counter is touched: the borrow of the service has to end before the
    // same guard can be used to drop it.
    let outcome = slot.service.as_mut().expect("set just above").frame(&absolute, timeout);
    match outcome {
        Ok(text) => {
            slot.failures = 0;
            Ok(text)
        }
        Err(why) => {
            slot.failures += 1;
            // Dropping is stopping, and stopping once. A service that just failed a frame is not reused.
            slot.service = None;
            Err(why)
        }
    }
}

/// Tear the child down. A [`Service`] does it when dropped, and a process that exits without dropping one
/// leaves `WeChatOCR.exe` behind — which is why `windrec` and `wind-reindex` call this on their way out.
pub fn shutdown() {
    let mut slot = slot().lock().unwrap_or_else(|p| p.into_inner());
    slot.service = None;
}

fn slot() -> &'static Mutex<Slot> {
    static SLOT: Mutex<Slot> = Mutex::new(Slot { service: None, failures: 0 });
    &SLOT
}

struct Slot {
    service: Option<Service>,
    failures: u32,
}

// --- the service ---------------------------------------------------------------------------

/// The state the DLL's thread writes and the indexing thread waits on.
struct Shared {
    /// The OCR channel is open, which is what says "a request will be answered".
    connected: bool,
    /// The child process exists. Distinct from connected: a launch that succeeded and a channel that
    /// never opened are different failures to tell a user about.
    launched: bool,
    launch_error: Option<i32>,
    /// The last thing the channel itself complained about, which is the only explanation a user gets for a
    /// child that connected and then stopped answering.
    channel_error: Option<String>,
    /// task id -> the response blob, exactly as it arrived.
    results: HashMap<u32, Vec<u8>>,
    /// task ids in flight, so a slot is not handed out twice.
    busy: [bool; TASK_SLOTS as usize],
}

/// Everything a callback needs, reached through the `user_data` pointer the DLL hands back.
struct Hub {
    state: Mutex<Shared>,
    /// Woken by every callback that can end a wait: an answer, a launch failure, a disconnect.
    ready: Condvar,
    read_request: GetReadRequest,
    drop_read: RemoveReadInfo,
}

/// One resident `WeChatOCR.exe`, addressed through `mmmojo_64.dll`.
struct Service {
    dll: Dll,
    /// Taken on stop, so the second call — the one `Drop` makes after an explicit stop on a failure path —
    /// has nothing left to tear down. `StopMMMojoEnvironment` twice on one handle is a fault inside the
    /// DLL, not a no-op.
    env: Cell<*mut c_void>,
    hub: &'static Hub,
}

// The environment handle is touched only while the process-wide slot lock is held, and the hub's state has
// its own mutex for the callback thread. That is the whole argument; `Service` is not `Sync` by itself.
unsafe impl Send for Service {}

impl Service {
    fn start(install: &Install) -> Result<Service, String> {
        let dll = Dll::open(&install.dll)?;
        // No argv: the DLL's own command-line handling has nothing to do with this process's arguments.
        unsafe { (dll.initialize)(0, std::ptr::null()) };
        let env = unsafe { (dll.create_environment)() };
        if env.is_null() {
            return Err(format!("{} gave no mmmojo environment", DLL));
        }
        // Leaked on purpose: the DLL can still be inside a callback when the environment is stopped, and a
        // freed `user_data` under it is a crash on a thread this process does not own. One small box per
        // restart of an engine a user restarts by hand is the cheaper problem.
        let hub: &'static Hub = Box::leak(Box::new(Hub {
            state: Mutex::new(Shared {
                connected: false,
                launched: false,
                launch_error: None,
                channel_error: None,
                results: HashMap::new(),
                busy: [false; TASK_SLOTS as usize],
            }),
            ready: Condvar::new(),
            read_request: dll.read_request,
            drop_read: dll.drop_read,
        }));
        let user = hub as *const Hub as *mut c_void;
        let exe = wide(&install.exe);
        let lib = wide(&install.dir);
        // The child's `--user-lib-dir`, which is how it finds the models when they sit beside it in this
        // install rather than inside a WeChat folder.
        let key = b"user-lib-dir\0";
        unsafe {
            // `kMMUserData` first: every callback below is handed this pointer, and setting one after the
            // callbacks would leave a window in which the DLL could call back with a null user data.
            (dll.set_callbacks)(env, CB_USER_DATA, user);
            // Every slot gets a function. The DLL calls whichever type it needs, and an unset one is a
            // jump to null inside a thread this process cannot recover from — which is the whole reason
            // the unused channels below are wired to something that does nothing rather than left alone.
            (dll.set_callbacks)(env, CB_READ_PUSH, on_read_push as *const c_void);
            (dll.set_callbacks)(env, CB_READ_PULL, on_read_pull as *const c_void);
            (dll.set_callbacks)(env, CB_READ_SHARED, on_read_shared as *const c_void);
            (dll.set_callbacks)(env, CB_REMOTE_CONNECT, on_connect as *const c_void);
            (dll.set_callbacks)(env, CB_REMOTE_DISCONNECT, on_disconnect as *const c_void);
            (dll.set_callbacks)(env, CB_PROCESS_LAUNCHED, on_launched as *const c_void);
            (dll.set_callbacks)(env, CB_PROCESS_LAUNCH_FAILED, on_launch_failed as *const c_void);
            (dll.set_callbacks)(env, CB_REMOTE_MOJO_ERROR, on_mojo_error as *const c_void);
            (dll.set_params)(env, PARAM_HOST_PROCESS, 1usize as *mut c_void);
            (dll.set_params)(env, PARAM_EXE_PATH, exe.as_ptr() as *mut c_void);
            (dll.append_switch)(env, key.as_ptr(), lib.as_ptr());
            (dll.start)(env);
        }
        Ok(Service { dll, env: Cell::new(env), hub })
    }

    /// Ask for one frame and wait for the callback that answers it.
    fn frame(&self, image: &Path, timeout: Duration) -> Result<String, String> {
        self.await_connection(timeout)?;
        let task_id = self.claim_slot()?;
        let request = encode_request(task_id, &image.to_string_lossy());
        {
            let mut state = self.hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(busy) = state.busy.get_mut((task_id - 1) as usize) {
                *busy = true;
            }
        }
        if let Err(why) = self.send(&request) {
            self.release(task_id);
            return Err(why);
        }

        let deadline = Instant::now() + timeout;
        let mut guard = self.hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let answer = loop {
            if let Some(bytes) = guard.results.remove(&task_id) {
                break Ok(bytes);
            }
            if let Some(code) = guard.launch_error {
                break Err(format!("{DLL} could not start {EXE} (error code {code})"));
            }
            if let Some(why) = guard.channel_error.clone() {
                break Err(format!("the mmmojo channel reported: {why}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(if !guard.connected {
                    format!(
                        "{EXE} {}within {}s of the request",
                        if guard.launched { "never opened its OCR channel " } else { "never started " },
                        timeout.as_secs()
                    )
                } else {
                    format!("no answer for {} within {}s", image.display(), timeout.as_secs())
                });
            }
            let (next, _wait) = self
                .hub
                .ready
                .wait_timeout(guard, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        };
        drop(guard);
        self.release(task_id);
        match answer {
            Ok(bytes) => decode_response(&bytes)
                .ok_or_else(|| format!("the answer for {} was not an OcrResponse", image.display())),
            Err(why) => Err(why),
        }
    }

    /// Wait until the child's channel is open.
    ///
    /// `StartMMMojoEnvironment` returns while the child is still coming up, and a request sent before the
    /// connect callback is refused by the channel outright. The reference client sleeps in a loop for the
    /// same thing; this waits on the condition instead, and reports the launch failure the DLL gave us
    /// rather than the timeout the wait would otherwise reach.
    fn await_connection(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if guard.connected {
                    return Ok(());
            }
            if let Some(code) = guard.launch_error {
                return Err(format!("{DLL} could not start {EXE} (error code {code})"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(if guard.launched {
                    format!("{EXE} started but never opened its OCR channel within {}s", timeout.as_secs())
                } else {
                    format!("{EXE} never started within {}s", timeout.as_secs())
                });
            }
            let (next, _wait) = self
                .hub
                .ready
                .wait_timeout(guard, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
    }

    fn send(&self, bytes: &[u8]) -> Result<(), String> {
        unsafe {
            let write_info = (self.dll.create_write_info)(METHOD_PUSH, false, OCR_PUSH);
            if write_info.is_null() {
                return Err(format!("{DLL} refused to create a write info"));
            }
            let buffer = (self.dll.write_request)(write_info, bytes.len() as u32);
            if buffer.is_null() {
                (self.dll.drop_write_info)(write_info);
                return Err(format!("{DLL} gave no request buffer"));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
            // The send takes the structure. `RemoveMMMojoWriteInfo` after it is a double free, and the
            // fault it produces lands inside the DLL a moment later, with no frame of ours on the stack —
            // so this comment is the only trace of the hour it took.
            if !(self.dll.send)(self.env.get(), write_info) {
                return Err(format!("{EXE} did not accept the request"));
            }
        }
        Ok(())
    }

    /// One of the 32 task ids, or the reason there is none free.
    fn claim_slot(&self) -> Result<u32, String> {
        let mut state = self.hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for index in 0..TASK_SLOTS as usize {
            if !state.busy[index] {
                state.busy[index] = true;
                return Ok(index as u32 + 1);
            }
        }
        Err(format!("all {TASK_SLOTS} {EXE} task slots are busy"))
    }

    fn release(&self, task_id: u32) {
        let mut state = self.hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(busy) = state.busy.get_mut((task_id - 1) as usize) {
            *busy = false;
        }
        drop(state);
        self.hub.ready.notify_all();
    }

    fn stop(&self) {
        // Swapping the handle out is what makes this idempotent.
        let env = self.env.replace(std::ptr::null_mut());
        if env.is_null() {
            return;
        }
        unsafe {
            (self.dll.stop)(env);
            (self.dll.remove)(env);
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- the callbacks ---------------------------------------------------------------------------

/// `void OnReadPush(uint32_t request_id, const void* read_info, void* user_data)`
///
/// `request_id` identifies the *channel* (1 for OCR), not the frame: the task id is inside the blob, which
/// is why a response is keyed by what it says rather than by what this was handed.
unsafe extern "C" fn on_read_push(request_id: u32, read_info: *mut c_void, user: *mut c_void) {
    if read_info.is_null() || user.is_null() {
        return;
    }
    // The channel id is the only thing that says what kind of message this is, and the child opens with
    // acknowledgements on other ids whose fields collide with an `OcrResponse`'s. Upstream filters on
    // exactly this, and a port that does not will answer a real request with an empty one.
    if request_id != OCR_PUSH {
        let hub = &*(user as *const Hub);
        unsafe { (hub.drop_read)(read_info) };
        return;
    }
    let hub = &*(user as *const Hub);
    let mut length = 0u32;
    let bytes = unsafe {
        let data = (hub.read_request)(read_info, &mut length);
        let out = if data.is_null() || length == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data.cast::<u8>(), length as usize).to_vec()
        };
        (hub.drop_read)(read_info);
        out
    };
    if bytes.is_empty() {
        return;
    }
    // A push with no task id in it is not an OCR answer; ignoring it is the same choice the reference
    // client makes when `m_id_path` has no entry for the id.
    let Some(task_id) = response_task_id(&bytes) else { return };
    let mut state = hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.results.insert(task_id, bytes);
    drop(state);
    hub.ready.notify_all();
}

/// The pull and shared channels, which this protocol does not use for OCR — the request is a push and the
/// answer arrives on the push channel. They still have to be answered, because the DLL is free to call
/// them, and the read info must be released either way or every such message leaks its buffer.
unsafe extern "C" fn on_read_pull(_request_id: u32, read_info: *mut c_void, user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    if read_info.is_null() {
        return;
    }
    unsafe { (hub.drop_read)(read_info) };
}

/// As above: a shared message is dropped, not ignored with its buffer still held.
unsafe extern "C" fn on_read_shared(_request_id: u32, read_info: *mut c_void, user: *mut c_void) {
    on_read_pull(_request_id, read_info, user);
}

/// `void OnRemoteMojoError(const void* errorbuf, int errorsize, void* user_data)`
///
/// Recorded rather than printed, because the recorder's log is where a user is told something is wrong.
unsafe extern "C" fn on_mojo_error(errorbuf: *const c_void, errorsize: i32, user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    if errorbuf.is_null() || errorsize <= 0 {
        return;
    }
    let text = unsafe {
        let bytes = std::slice::from_raw_parts(errorbuf.cast::<u8>(), errorsize as usize);
        String::from_utf8_lossy(bytes).into_owned()
    };
    let mut state = hub.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.channel_error = Some(text.trim_end_matches('\0').to_string());
    drop(state);
    hub.ready.notify_all();
}

/// `void OnRemoteConnect(bool is_connected, void* user_data)`
unsafe extern "C" fn on_connect(is_connected: bool, user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    let mut state = hub.state.lock().unwrap_or_else(|p| p.into_inner());
    state.connected = is_connected;
    if is_connected {
        hub.ready.notify_all();
    }
}

/// `void OnRemoteDisconnect(void* user_data)`
unsafe extern "C" fn on_disconnect(user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    let mut state = hub.state.lock().unwrap_or_else(|p| p.into_inner());
    state.connected = false;
    // Waking here matters: a frame in flight would otherwise wait out the whole timeout for a child that
    // is already gone.
    drop(state);
    hub.ready.notify_all();
}

unsafe extern "C" fn on_launched(user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    let mut state = hub.state.lock().unwrap_or_else(|p| p.into_inner());
    state.launched = true;
    drop(state);
    hub.ready.notify_all();
}

unsafe extern "C" fn on_launch_failed(error_code: i32, user: *mut c_void) {
    let Some(hub) = (unsafe { (user as *const Hub).as_ref() }) else { return };
    let mut state = hub.state.lock().unwrap_or_else(|p| p.into_inner());
    state.launch_error = Some(error_code);
    drop(state);
    hub.ready.notify_all();
}

// --- the wire --------------------------------------------------------------------------------

/// `OcrRequest { 1: unknow int32, 2: task_id int32, 3: pic_path { 1: repeated string } }`
///
/// `unknow` is not written: proto3 omits defaults, and the reference client's own bytes for
/// `unknow = 0` omit it too. The pinned vector below is what that client produces.
pub fn encode_request(task_id: u32, path: &str) -> Vec<u8> {
    let mut inner = vec![0x0a];
    inner.extend(varint(path.len() as u64));
    inner.extend(path.as_bytes());
    let mut out = vec![0x10];
    out.extend(varint(u64::from(task_id)));
    out.push(0x1a);
    out.extend(varint(inner.len() as u64));
    out.extend(inner);
    out
}

/// The task id inside an `OcrResponse` — field 2 — without decoding anything else.
pub fn response_task_id(bytes: &[u8]) -> Option<u32> {
    // Field 4 is `ocr_result`, and an answer without it is a status message that happens to carry a
    // number in the same slot the task id lives in.
    let mut task_id = None;
    let mut carried_result = false;
    for field in Fields::new(bytes) {
        match (field.number, field.value, field.body) {
            (2, Some(value), _) if task_id.is_none() => task_id = Some(value as u32),
            (4, _, Some(_)) => carried_result = true,
            _ => {}
        }
    }
    if carried_result { task_id } else { None }
}

/// `OcrResponse { 1: type, 2: task_id, 3: err_code, 4: ocr_result { 1: repeated single_result } }`, where a
/// `single_result` carries its text in field 2, `single_str_utf8`, typed `bytes`.
///
/// `bytes` and `string` are the same thing on the wire; this engine sends UTF-8 in a `bytes` field, which
/// is why the reference client has to base64-decode text that protobuf had already handed it over raw.
///
/// `None` means the blob is not an OCR response at all. A response that is simply empty is `Ok("")`, which
/// is the same answer the other engines give for a frame with nothing readable in it.
pub fn decode_response(bytes: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut task_id = None;
    let mut broken = false;
    let mut fields = Fields::new(bytes);
    while let Some(field) = fields.next() {
        match (field.number, field.value, field.body) {
            (2, Some(value), _) => task_id = Some(value as u32),
            (4, _, Some(body)) => {
                let mut results = Fields::new(body);
                while let Some(result) = results.next() {
                    if result.number != 1 {
                        continue;
                    }
                    let Some(single) = result.body else { continue };
                    let mut leaves = Fields::new(single);
                    while let Some(leaf) = leaves.next() {
                        if leaf.number != 2 {
                            continue;
                        }
                        let Some(text) = leaf.body else { continue };
                        // Unreadable UTF-8 in one region costs that region and nothing else: a frame half
                        // of which is legible is a frame worth indexing.
                        if let Ok(text) = std::str::from_utf8(text) {
                            let text = text.trim();
                            if !text.is_empty() {
                                lines.push(text.to_string());
                            }
                        }
                    }
                    broken |= leaves.broke();
                }
                broken |= results.broke();
            }
            _ => {}
        }
    }
    broken |= fields.broke();
    if broken {
        return None;
    }
    task_id.map(|_| lines.join("\n"))
}

/// One protobuf field: a number, and whichever of the two shapes it carries.
#[derive(Debug, Clone, Copy)]
struct Field<'a> {
    number: u64,
    /// The integer for a varint field.
    value: Option<u64>,
    /// The body for a length-delimited field.
    body: Option<&'a [u8]>,
}

/// A protobuf reader small enough to audit. Anything it does not understand ends the walk rather than
/// being guessed at, because a half-read message must not be mistaken for a frame with no text in it.
struct Fields<'a> {
    bytes: &'a [u8],
    at: usize,
    broken: bool,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Fields<'a> {
        Fields { bytes, at: 0, broken: false }
    }

    /// Did the walk stop because the bytes ran out rather than because the message ended? A truncated
    /// response is not an empty one, and the difference is whether a frame gets indexed with no text.
    fn broke(&self) -> bool {
        self.broken
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                self.broken = true;
                return None;
            }
        }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(length)?;
        if end > self.bytes.len() {
            self.broken = true;
            return None;
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Some(out)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Field<'a>;

    fn next(&mut self) -> Option<Field<'a>> {
        if self.broken || self.at >= self.bytes.len() {
            return None;
        }
        let key = self.varint()?;
        let number = key >> 3;
        match key & 7 {
            0 => self.varint().map(|value| Field { number, value: Some(value), body: None }),
            2 => {
                let length = self.varint()? as usize;
                self.take(length).map(|body| Field { number, value: None, body: Some(body) })
            }
            5 => self.take(4).map(|body| Field { number, value: None, body: Some(body) }),
            1 => self.take(8).map(|body| Field { number, value: None, body: Some(body) }),
            // Groups and the unknown wire types this protocol never uses end the walk.
            _ => {
                self.broken = true;
                None
            }
        }
    }
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// The absolute path the child will read.
///
/// `std::path::absolute` rather than `canonicalize`: the child resolves the name itself, and a `\\?\`
/// prefix is a thing to hand a third-party binary only if it has to be. Upstream used `os.path.abspath`,
/// which is the same no-syscall rule.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let base = std::env::current_dir().map_err(|e| format!("no working directory to resolve {path:?}: {e}"))?;
    Ok(base.join(path))
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

// --- mmmojo's surface ------------------------------------------------------------------------

type Init = unsafe extern "C" fn(argc: i32, argv: *const *const u8);
type CreateEnvironment = unsafe extern "C" fn() -> *mut c_void;
type SetCallbacks = unsafe extern "C" fn(env: *mut c_void, kind: i32, value: *const c_void);
type SetParams = unsafe extern "C" fn(env: *mut c_void, kind: i32, value: *mut c_void);
type AppendSwitch = unsafe extern "C" fn(env: *mut c_void, key: *const u8, value: *const u16);
type StartEnvironment = unsafe extern "C" fn(env: *mut c_void);
type StopEnvironment = unsafe extern "C" fn(env: *mut c_void);
type RemoveEnvironment = unsafe extern "C" fn(env: *mut c_void);
type CreateWriteInfo = unsafe extern "C" fn(method: i32, sync: bool, request_id: u32) -> *mut c_void;
type GetWriteRequest = unsafe extern "C" fn(write_info: *mut c_void, size: u32) -> *mut c_void;
type RemoveWriteInfo = unsafe extern "C" fn(write_info: *mut c_void);
type SendWriteInfo = unsafe extern "C" fn(env: *mut c_void, write_info: *mut c_void) -> bool;
type GetReadRequest = unsafe extern "C" fn(read_info: *mut c_void, size: *mut u32) -> *const c_void;
type RemoveReadInfo = unsafe extern "C" fn(read_info: *mut c_void);

/// A loaded `mmmojo_64.dll` with every entry point this module uses already resolved.
///
/// Resolving at open time is what lets the callbacks carry their own function pointers in the hub instead
/// of reaching for a global, and it means a DLL that is present but not the version this protocol expects
/// fails here, with the name of the missing export, rather than on first use mid-recording.
struct Dll {
    module: *mut c_void,
    initialize: Init,
    create_environment: CreateEnvironment,
    set_callbacks: SetCallbacks,
    set_params: SetParams,
    append_switch: AppendSwitch,
    start: StartEnvironment,
    stop: StopEnvironment,
    remove: RemoveEnvironment,
    create_write_info: CreateWriteInfo,
    write_request: GetWriteRequest,
    /// Resolved and deliberately never called — see [`Service::send`]. Looking it up anyway is the point:
    /// a build of the library that does not export it is a build this module should not claim to speak.
    #[allow(dead_code)]
    drop_write_info: RemoveWriteInfo,
    send: SendWriteInfo,
    read_request: GetReadRequest,
    drop_read: RemoveReadInfo,
}

// `GetProcAddress` is documented as callable from any thread, and the module handle is never used to call
// into the DLL by itself — only to look symbols up.
unsafe impl Sync for Dll {}
unsafe impl Send for Dll {}

impl Dll {
    fn open(path: &Path) -> Result<Dll, String> {
        let wide_path = wide(path);
        let module = unsafe { LoadLibraryExW(wide_path.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if module.is_null() {
            return Err(format!(
                "cannot load {}: Windows reported {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Dll {
            module,
            // The one global call the reference client makes before anything else. Skipping it is not an
            // error the caller can see: the environment factory reads module state that is not there yet
            // and the process faults. Found by running this, not by reading it.
            initialize: unsafe { symbol(module, "InitializeMMMojo")? },
            create_environment: unsafe { symbol(module, "CreateMMMojoEnvironment")? },
            set_callbacks: unsafe { symbol(module, "SetMMMojoEnvironmentCallbacks")? },
            set_params: unsafe { symbol(module, "SetMMMojoEnvironmentInitParams")? },
            append_switch: unsafe { symbol(module, "AppendMMSubProcessSwitchNative")? },
            start: unsafe { symbol(module, "StartMMMojoEnvironment")? },
            stop: unsafe { symbol(module, "StopMMMojoEnvironment")? },
            remove: unsafe { symbol(module, "RemoveMMMojoEnvironment")? },
            create_write_info: unsafe { symbol(module, "CreateMMMojoWriteInfo")? },
            write_request: unsafe { symbol(module, "GetMMMojoWriteInfoRequest")? },
            drop_write_info: unsafe { symbol(module, "RemoveMMMojoWriteInfo")? },
            send: unsafe { symbol(module, "SendMMMojoWriteInfo")? },
            read_request: unsafe { symbol(module, "GetMMMojoReadInfoRequest")? },
            drop_read: unsafe { symbol(module, "RemoveMMMojoReadInfo")? },
        })
    }
}

impl Drop for Dll {
    fn drop(&mut self) {
        // Deliberately not `FreeLibrary`: the DLL runs a message-loop thread of its own, and unloading the
        // module it executes from is a crash on a thread this process does not own. The engine's library is
        // loaded once per process and stays.
        let _ = self.module;
    }
}

/// `GetProcAddress`, typed. The size check is the one that keeps a wrong signature from becoming a call
/// through a pointer of the wrong shape.
unsafe fn symbol<T: Copy>(module: *mut c_void, name: &str) -> Result<T, String> {
    let mut ascii = name.as_bytes().to_vec();
    ascii.push(0);
    let address = GetProcAddress(module, ascii.as_ptr());
    if address.is_null() {
        return Err(format!("{name} is not exported by {DLL}"));
    }
    if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
        return Err(format!("{name} is not a function pointer"));
    }
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&address))
}

/// `LoadLibraryExW` with `LOAD_WITH_ALTERED_SEARCH_PATH`, so the child's sibling DLLs are found beside
/// `mmmojo_64.dll` and not by whatever else is on `PATH`.
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference client's own bytes for `OcrRequest(unknow=0, task_id=7,
    /// pic_path=["C:\\tmp\\frame.jpg"])`, produced by `wechat_ocr` 0.0.3. If this ever differs, the port is
    /// speaking a different protocol and the child will answer nothing.
    #[test]
    fn a_request_is_the_bytes_the_reference_client_makes() {
        let bytes = encode_request(7, r"C:\tmp\frame.jpg");
        assert_eq!(hex(&bytes), "10071a120a10433a5c746d705c6672616d652e6a7067", "{bytes:?}");
    }

    #[test]
    fn a_task_id_and_a_path_both_longer_than_a_byte_still_encode() {
        let bytes = encode_request(300, &"x".repeat(300));
        // An *answer* is a task id plus an `ocr_result`; the same number on its own is a status message,
        // and reading one as the other is how a frame gets indexed with nothing in it.
        let mut answer = vec![0x10, 0xac, 0x02, 0x22, 0x02, 0x0a, 0x00];
        assert_eq!(response_task_id(&answer), Some(300));
        answer.pop();
        answer.pop();
        assert_eq!(response_task_id(&answer), None, "an empty ocr_result is still a result; no result is not");
        assert_eq!(response_task_id(&[0x10, 0xac, 0x02]), None);
        // A 300-character path makes the inner length a two-byte varint too: `0x1a`, then `len`, then
        // `0x0a + len(300) + 300` = 303 = 0x8f|0x80, 0x02.
        assert_eq!(bytes[3], 0x1a);
        assert_eq!(&bytes[4..6], &[0xaf, 0x02], "the outer length must be 303, not truncated to one byte");
        assert_eq!(bytes.len(), 6 + 303);
    }

    /// The reference client's `OcrResponse(type=1, task_id=7, single_result=[{text: 季度营收}])`.
    #[test]
    fn a_response_is_read_back_as_the_text_the_model_saw() {
        let bytes = unhex("0801100722240a22120ce5ada3e5baa6e890a5e694b62d0000803f35000000403d000040404500008040");
        assert_eq!(response_task_id(&bytes), Some(7));
        assert_eq!(decode_response(&bytes).as_deref(), Some("季度营收"));
    }

    #[test]
    fn several_regions_come_back_in_the_order_the_model_returned_them() {
        let mut body = Vec::new();
        for text in ["first", "第二行", "third"] {
            let mut single = vec![0x12];
            single.extend(varint(text.len() as u64));
            single.extend(text.as_bytes());
            let mut wrapped = vec![0x0a];
            wrapped.extend(varint(single.len() as u64));
            wrapped.extend(single);
            body.extend(wrapped);
        }
        let mut response = vec![0x08, 0x01, 0x10, 0x07, 0x22];
        response.extend(varint(body.len() as u64));
        response.extend(body);
        assert_eq!(decode_response(&response).as_deref(), Some("first\n第二行\nthird"));
    }

    #[test]
    fn a_frame_with_nothing_readable_is_empty_text_and_not_a_failure() {
        assert_eq!(decode_response(&unhex("080110071800")).as_deref(), Some(""));
    }

    #[test]
    fn bytes_that_are_not_an_ocr_response_are_refused_instead_of_panicking() {
        assert_eq!(decode_response(b"not protobuf at all"), None, "no task id, so no response");
        assert_eq!(decode_response(&[]), None);
        assert_eq!(response_task_id(&[]), None);
        // A length that runs past the buffer is a truncated answer, not an empty one.
        assert_eq!(decode_response(&[0x10, 0x07, 0x22, 0x80, 0x40]), None);
        // A well-formed response with no text in it is the empty answer.
        assert_eq!(decode_response(&[0x10, 0x07, 0x22, 0x00]), Some(String::new()));
    }

    #[test]
    fn an_install_without_its_models_says_which_piece_is_missing() {
        let dir = std::env::temp_dir().join(format!("windbase-wxocr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let binary = dir.join(BINARY_DIR);
        std::fs::create_dir_all(&binary).unwrap();
        assert!(Install::probe(&dir).missing.unwrap().contains(EXE), "the exe is named first");

        std::fs::write(binary.join(EXE), b"MZ").unwrap();
        assert!(Install::probe(&dir).missing.unwrap().contains(DLL), "then the channel library");

        std::fs::write(binary.join(DLL), b"MZ").unwrap();
        assert!(Install::probe(&dir).missing.unwrap().contains(MODELS), "then the models");

        std::fs::create_dir_all(binary.join(MODELS)).unwrap();
        assert!(Install::probe(&dir).is_usable(), "with all three, the engine is offered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_dll_names_the_file_it_could_not_load() {
        // Dll has no `Debug` (it owns a module handle and fourteen function pointers, none of which are
        // worth printing), so the error is destructured rather than unwrapped.
        let Err(error) = Dll::open(Path::new("Z:/definitely/not/here/mmmojo_64.dll")) else {
            panic!("a library that does not exist must not open");
        };
        assert!(error.contains("definitely/not/here"), "{error}");
    }

    #[test]
    fn a_relative_picture_path_is_resolved_before_the_child_is_asked_to_read_it() {
        let here = std::env::current_dir().unwrap();
        let resolved = absolute(Path::new("frame.jpg")).unwrap();
        assert_eq!(resolved, here.join("frame.jpg"));
        assert_eq!(absolute(&here).unwrap(), here, "an absolute path is already the answer");
    }

    /// The live test, off the default run because it starts a 36 MB child and reads real pictures.
    ///
    /// `cargo test -p wind-base -- --ignored` is how the port is proved against the engine itself; every
    /// other test in this module pins the wire format and the disk rules, which is all a machine without
    /// the binaries can honestly say.
    #[test]
    #[ignore = "starts WeChatOCR.exe and reads the __assets__ fixtures"]
    fn the_real_service_reads_a_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).unwrap();
        let install = Install::probe(root);
        assert!(install.is_usable(), "{:?}", install.missing);
        // Every fixture the install ships, in one process: the second and third frames are the ones a
        // single-image test never reaches, and the service is what has to survive them.
        let mut read = 0;
        for name in [
            "OCR_test_1080_zh-Hans-CN.png",
            "OCR_test_1080_en-US.png",
            "OCR_test_1080_ja-jp.png",
        ] {
            let image = root.join("__assets__").join(name);
            if !image.is_file() {
                continue;
            }
            let started = Instant::now();
            let text = recognize(&install, &image, Duration::from_secs(90)).expect("recognized");
            println!("{name}: {} chars in {:?}: {:?}", text.chars().count(), started.elapsed(), text.chars().take(40).collect::<String>());
            assert!(text.chars().count() > 20, "too short to be a recognition in {name}: {text:?}");
            read += 1;
        }
        assert!(read >= 2, "the fixtures were not all read: {read}");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|at| u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap())
            .collect()
    }
}
