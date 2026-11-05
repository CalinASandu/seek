//! Running an app's own uninstaller, and knowing when it is done.
//!
//! What runs is decided here and from the registry only: [`Uninstalls::start`] takes the
//! [`FoundApp`] that `found_by_id` built, never a command, path or argument from the window. An
//! MSI product is always `<Windows folder>\System32\msiexec.exe /x{GUID}`, built by Seek; the
//! registry's other arguments are dropped. An `.exe` runs with exactly the registered path and
//! arguments.
//!
//! The program is started through the [`Launcher`] trait. Tests give it a fake, so no test ever
//! starts a program. The one real launcher, [`ShellLauncher`], is made only in the Tauri command.
//!
//! "Finished" means the process ended and the app's registry entry is gone. Many uninstallers
//! start a temporary copy of themselves and exit, so after the process ends the entry is read
//! again once a second for up to five minutes ([`GIVE_UP_MS`]) before the answer is "unresolved".
//! The clock and the entry reader are injected, so the tests don't sleep.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use serde::Serialize;

use super::{is_guid, parse_app_id, FoundApp, UninstallCommand};
use crate::claims;
use crate::model::AppRow;

/// How often the registry entry is read again once the process has ended.
pub const RECHECK_MS: u64 = 1000;
/// How long after the process ended Seek keeps looking for the entry to go.
pub const GIVE_UP_MS: u64 = 5 * 60 * 1000;

pub const NOT_LISTED: &str = "Seek can't find that app in the list of installed apps any more.";
pub const NO_COMMAND: &str = "Seek can't start this app's uninstaller.";
pub const ALREADY_RUNNING: &str = "An uninstaller is already running. Wait for it to finish first.";
pub const NO_PERMISSION: &str = "Windows didn't get permission to run the uninstaller.";
pub const NOT_STARTED: &str = "Seek hasn't started an uninstaller for that app.";

// ---- the phase ------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UninstallPhase {
    Running,
    Finished,
    Unresolved,
}

/// What the two facts say, before any waiting: finished needs the process ended and the entry
/// gone. A process that ended with its entry still there is still `Running`; only the waiting
/// (see [`Watch`]) turns that into `Unresolved`.
pub fn is_finished(process_ended: bool, entry_present: bool) -> UninstallPhase {
    if process_ended && !entry_present {
        UninstallPhase::Finished
    } else {
        UninstallPhase::Running
    }
}

/// Follows one uninstall. Pure: the time and the entry reader come in with each question.
#[derive(Debug, Default)]
pub struct Watch {
    ended_at: Option<u64>,
    last_read: Option<u64>,
    entry_present: bool,
    finished: bool,
}

impl Watch {
    /// The phase at `now_ms`. `read_entry` says whether the registry entry is still there; it is
    /// called at most once per [`RECHECK_MS`], and once more when the wait runs out.
    pub fn phase(&mut self, process_ended: bool, now_ms: u64, read_entry: &mut dyn FnMut() -> bool) -> UninstallPhase {
        if self.finished {
            return UninstallPhase::Finished;
        }
        if !process_ended {
            return UninstallPhase::Running;
        }
        let ended_at = *self.ended_at.get_or_insert(now_ms);
        let waited = now_ms.saturating_sub(ended_at);
        let due = self.last_read.is_none_or(|t| now_ms.saturating_sub(t) >= RECHECK_MS);
        if due || waited >= GIVE_UP_MS {
            self.last_read = Some(now_ms);
            self.entry_present = read_entry();
        }
        match is_finished(true, self.entry_present) {
            UninstallPhase::Finished => {
                self.finished = true;
                UninstallPhase::Finished
            }
            _ if waited >= GIVE_UP_MS => UninstallPhase::Unresolved,
            other => other,
        }
    }
}

// ---- what to run ----------------------------------------------------------------------------------

/// A program and its arguments as one string, as `ShellExecuteExW` takes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub program: String,
    pub args: String,
}

/// What to start for `cmd`. MSI is built here from the product code alone, with the Windows
/// folder as Windows spells it; an `.exe` is exactly what the registry said.
pub fn launch_request(cmd: &UninstallCommand, windows_dir: &str) -> Result<LaunchRequest, String> {
    match cmd {
        UninstallCommand::Msi { guid } => {
            if !is_guid(guid) || windows_dir.trim().is_empty() {
                return Err(NO_COMMAND.to_string());
            }
            Ok(LaunchRequest { program: format!(r"{}\System32\msiexec.exe", windows_dir.trim_end_matches('\\')), args: format!("/x{guid}") })
        }
        UninstallCommand::Exe { path, args } if !path.trim().is_empty() => Ok(LaunchRequest { program: path.clone(), args: args.clone() }),
        UninstallCommand::Exe { .. } => Err(NO_COMMAND.to_string()),
    }
}

/// A started program that can be waited for.
pub trait ProcessWait: Send {
    /// Blocks until the program has ended, then lets go of it.
    fn wait(self: Box<Self>);
}

/// Starts an uninstall command. Tests use a fake; [`ShellLauncher`] is the real one.
pub trait Launcher: Send + Sync {
    fn launch(&self, cmd: &UninstallCommand) -> Result<Box<dyn ProcessWait>, String>;
}

/// The launcher that really starts programs. Made only by the Tauri command.
pub struct ShellLauncher;

impl Launcher for ShellLauncher {
    fn launch(&self, cmd: &UninstallCommand) -> Result<Box<dyn ProcessWait>, String> {
        run_uninstaller(cmd).map(|h| Box::new(h) as Box<dyn ProcessWait>)
    }
}

impl ProcessWait for UninstallHandle {
    fn wait(self: Box<Self>) {
        self.wait_for_exit();
    }
}

// ---- the manager ----------------------------------------------------------------------------------------

/// Milliseconds on a clock that only moves forward.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// Whether the registry entry behind an app id is still there.
pub type EntryReader = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct Slot {
    app: AppRow,
    ended: Arc<AtomicBool>,
    watch: Watch,
    /// Only the tests join it.
    #[cfg_attr(not(test), allow(dead_code))]
    waiter: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<String, Slot>,
    /// A start is between "no other uninstaller runs" and "this one is recorded".
    launching: bool,
}

/// Puts `launching` back whichever way a start ends.
struct Launching<'a>(&'a Mutex<Inner>);

impl Drop for Launching<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).launching = false;
    }
}

/// Sets the flag when dropped, so a waiter that panics still counts as ended.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Every uninstall this run of the app has started, by app id. Kept in the app's managed state,
/// so it outlives the window being hidden. One runs at a time.
pub struct Uninstalls {
    inner: Mutex<Inner>,
    clock: Clock,
    entry_present: EntryReader,
}

impl Default for Uninstalls {
    fn default() -> Self {
        let epoch = std::time::Instant::now();
        Uninstalls::with(Arc::new(move || epoch.elapsed().as_millis() as u64), Arc::new(entry_present))
    }
}

impl Uninstalls {
    pub fn with(clock: Clock, entry_present: EntryReader) -> Uninstalls {
        Uninstalls { inner: Mutex::new(Inner::default()), clock, entry_present }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the uninstaller of `found`, which must be what `found_by_id` gave (so its command
    /// comes from the registry). Refused when there is no such app or command, or when another
    /// uninstaller is still running. No lock is held while the program starts (an admin prompt can
    /// take a while) or while it is waited for.
    pub fn start(&self, launcher: &dyn Launcher, found: Option<FoundApp>) -> Result<(), String> {
        let found = found.ok_or(NOT_LISTED)?;
        let cmd = found.command.clone().ok_or(NO_COMMAND)?;
        {
            let mut g = self.lock();
            if g.launching || g.slots.values().any(|s| !s.ended.load(Ordering::SeqCst)) {
                return Err(ALREADY_RUNNING.to_string());
            }
            g.launching = true;
        }
        let _launching = Launching(&self.inner);
        let waiter = launcher.launch(&cmd)?;
        let ended = Arc::new(AtomicBool::new(false));
        let flag = SetOnDrop(ended.clone());
        let thread = std::thread::Builder::new()
            .name("seek-uninstall-wait".into())
            .spawn(move || {
                let _flag = flag;
                waiter.wait();
            })
            .ok();
        if thread.is_none() {
            // The program is running but can't be watched: the registry entry decides alone.
            ended.store(true, Ordering::SeqCst);
        }
        let id = found.row.id.clone();
        self.lock().slots.insert(id, Slot { app: found.row, ended, watch: Watch::default(), waiter: thread });
        Ok(())
    }

    /// Where the uninstall of `app_id` stands.
    pub fn state(&self, app_id: &str) -> Result<UninstallPhase, String> {
        let now = (self.clock)();
        let mut g = self.lock();
        let slot = g.slots.get_mut(app_id).ok_or(NOT_STARTED)?;
        let ended = slot.ended.load(Ordering::SeqCst);
        Ok(slot.watch.phase(ended, now, &mut || (self.entry_present)(app_id)))
    }

    /// The app as it was listed when its uninstaller was started (the registry no longer has it
    /// once the uninstaller is done).
    pub fn app(&self, app_id: &str) -> Option<AppRow> {
        self.lock().slots.get(app_id).map(|s| s.app.clone())
    }

    /// Tests only: waits until the thread that watches the process has finished.
    #[cfg(test)]
    pub(crate) fn join_waiter(&self, app_id: &str) {
        let handle = self.lock().slots.get_mut(app_id).and_then(|s| s.waiter.take());
        if let Some(h) = handle {
            h.join().unwrap();
        }
    }
}

/// True if the registry still has the entry behind this app id. A malformed id has none.
pub fn entry_present(app_id: &str) -> bool {
    parse_app_id(app_id).is_some_and(|(hive, view, key)| claims::read_uninstall_entry(hive, view, key).is_some())
}

#[cfg(test)]
mod tests;

// ---- the real launch ------------------------------------------------------------------------------------

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_ELEVATION_REQUIRED, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// A process handle that is closed exactly once, when this is dropped.
struct OwnedProcess(HANDLE);

// SAFETY: a process handle is just a number the kernel keeps for this process; any thread may
// wait on it or close it.
unsafe impl Send for OwnedProcess {}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: ShellExecuteExW gave us this handle (SEE_MASK_NOCLOSEPROCESS) and this is
            // the only place that closes it.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// A started uninstaller. Dropping it closes the process handle.
pub struct UninstallHandle {
    /// `None` when Windows started the program without giving a process (the registry entry then
    /// decides alone).
    process: Option<OwnedProcess>,
}

impl UninstallHandle {
    /// Blocks until the process has ended.
    pub fn wait_for_exit(mut self) {
        if let Some(p) = self.process.take() {
            // SAFETY: `p` holds a live process handle until it is dropped below.
            unsafe {
                WaitForSingleObject(p.0, INFINITE);
            }
        }
    }
}

/// Starts the uninstaller visibly, with its process handle kept so it can be waited for. If
/// Windows says elevation is required, once more with the `runas` verb. Never called by a test.
pub fn run_uninstaller(cmd: &UninstallCommand) -> Result<UninstallHandle, String> {
    let windows_dir = crate::protect::windows_dir().ok_or("Seek can't find the Windows folder.")?;
    let req = launch_request(cmd, &windows_dir)?;
    // ShellExecuteExW wants a COM apartment on its thread.
    std::thread::spawn(move || {
        // SAFETY: balanced with CoUninitialize below, on this same thread.
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
        let started = execute(&req, None).or_else(|e| if e.is_elevation_required() { execute(&req, Some("runas")) } else { Err(e) });
        if com {
            // SAFETY: the CoInitializeEx above succeeded.
            unsafe { CoUninitialize() };
        }
        started.map_err(|e| e.text())
    })
    .join()
    .map_err(|_| "Seek couldn't start the uninstaller.".to_string())?
}

struct ExecError(windows::core::Error);

impl ExecError {
    fn is_elevation_required(&self) -> bool {
        self.0.code() == ERROR_ELEVATION_REQUIRED.to_hresult()
    }

    fn text(&self) -> String {
        if self.0.code() == ERROR_CANCELLED.to_hresult() {
            return NO_PERMISSION.to_string();
        }
        format!("Windows couldn't start the uninstaller ({}).", self.0.message().trim())
    }
}

fn execute(req: &LaunchRequest, verb: Option<&str>) -> Result<UninstallHandle, ExecError> {
    let file = HSTRING::from(req.program.as_str());
    let args = HSTRING::from(req.args.as_str());
    let verb = verb.map(HSTRING::from);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: if req.args.is_empty() { PCWSTR::null() } else { PCWSTR(args.as_ptr()) },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully set up and the strings it points to outlive the call.
    unsafe { ShellExecuteExW(&mut info) }.map_err(ExecError)?;
    let process = (!info.hProcess.is_invalid()).then(|| OwnedProcess(info.hProcess));
    Ok(UninstallHandle { process })
}
