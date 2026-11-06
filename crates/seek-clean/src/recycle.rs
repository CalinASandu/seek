//! Recycling through `IFileOperation`, and restoring through the Recycle Bin shell folder.
//!
//! This is the Task 1 spike: it records exactly what the progress sink reports, so later tasks
//! can decide what counts as proof that an item reached the bin.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::facts::Recycler;

use windows::core::{implement, Interface, BOOL, HRESULT, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, PROPERTYKEY};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOperation, FILEOPERATION_FLAGS, IContextMenu, IEnumShellItems, IFileOperation, IFileOperationProgressSink,
    IFileOperationProgressSink_Impl, IShellItem, IShellItem2, SHCreateItemFromParsingName,
    SHGetKnownFolderItem, BHID_EnumItems, BHID_SFUIObject, CMINVOKECOMMANDINFO,
    FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FOF_NO_UI, FOLDERID_RecycleBinFolder,
    SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu};

/// One `PostDeleteItem` call.
#[derive(Debug, Clone, Default)]
pub struct PostRecord {
    /// `dwFlags` (TSF_*).
    pub flags: u32,
    /// `hrDelete` as a raw HRESULT.
    pub hr_delete: i32,
    /// `psiNewlyCreated`'s file system path, when Windows gave one.
    pub new_path: Option<String>,
    /// `psiNewlyCreated`'s parsing name.
    pub new_parsing_name: Option<String>,
    /// True when `psiNewlyCreated` was null.
    pub new_item_missing: bool,
}

/// What Windows reported for one recycled item, call by call.
#[derive(Debug, Clone, Default)]
pub struct RecycleReport {
    /// `dwFlags` of every `PreDeleteItem` call (TSF_*; `TSF_DELETE_RECYCLE_IF_POSSIBLE` = 0x80).
    pub pre_flags: Vec<u32>,
    /// Every `PostDeleteItem` call, in order.
    pub posts: Vec<PostRecord>,
    /// The error `DeleteItem`/`PerformOperations` returned, if any.
    pub perform_error: Option<String>,
    /// The raw HRESULT of `perform_error`.
    pub perform_hr: Option<i32>,
    /// The sink refused an item in `PreDeleteItem` because Windows did not say it would recycle it.
    pub refused_no_recycle: bool,
    /// `GetAnyOperationsAborted`.
    pub aborted: bool,
}

impl RecycleReport {
    /// The bin location from the first successful `PostDeleteItem` that carried one.
    pub fn bin_location(&self) -> Option<&str> {
        self.posts
            .iter()
            .filter(|p| p.hr_delete >= 0)
            .find_map(|p| p.new_path.as_deref().or(p.new_parsing_name.as_deref()))
    }

    /// The bin location, but only if everything Windows reported backs it up: at least one
    /// `PostDeleteItem`, and every successful one carrying a location under `$Recycle.Bin`.
    /// A successful post without a location may be a permanent delete hiding next to a real one.
    pub fn confirmed_location(&self) -> Result<&str, String> {
        if self.posts.is_empty() {
            return Err("Windows reported no result for the item".into());
        }
        let mut first = None;
        for p in self.posts.iter().filter(|p| p.hr_delete >= 0) {
            match p.new_path.as_deref().or(p.new_parsing_name.as_deref()) {
                Some(loc) if loc.to_ascii_lowercase().contains("$recycle.bin") => {
                    first.get_or_insert(loc);
                }
                Some(loc) => return Err(format!("Windows reported a location outside the Recycle Bin: {loc}")),
                None => return Err("Windows reported no Recycle Bin location for the item".into()),
            }
        }
        first.ok_or_else(|| "Windows reported no Recycle Bin location for the item".into())
    }
}

#[derive(Default)]
struct Rec {
    report: RecycleReport,
}

#[implement(IFileOperationProgressSink)]
struct Sink {
    rec: Arc<Mutex<Rec>>,
    /// Probe mode: refuse the item right after `PreDeleteItem` is recorded, so nothing is deleted.
    abort_after_pre: bool,
    /// Refuse any item whose `PreDeleteItem` flags lack `TSF_DELETE_RECYCLE_IF_POSSIBLE`.
    require_recycle_bit: bool,
}

/// `TSF_DELETE_RECYCLE_IF_POSSIBLE`: Windows intends to recycle, not delete for good.
const TSF_DELETE_RECYCLE_IF_POSSIBLE: u32 = 0x80;

fn pwstr_to_string(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: shell returned a NUL-terminated CoTaskMem string; freed right after copying.
    let s = unsafe { p.to_string().ok() };
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn item_path(item: &IShellItem, kind: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    // SAFETY: plain COM call on a live item.
    unsafe { item.GetDisplayName(kind).ok().and_then(pwstr_to_string) }
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hrresult: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _n: &PCWSTR, _hr: HRESULT, _c: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR, _hr: HRESULT, _c: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(&self, _f: u32, _i: windows::core::Ref<IShellItem>, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR, _hr: HRESULT, _c: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, dwflags: u32, _psiitem: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        let refuse = self.abort_after_pre
            || (self.require_recycle_bit && dwflags & TSF_DELETE_RECYCLE_IF_POSSIBLE == 0);
        if let Ok(mut g) = self.rec.lock() {
            g.report.pre_flags.push(dwflags);
            if self.require_recycle_bit && dwflags & TSF_DELETE_RECYCLE_IF_POSSIBLE == 0 {
                g.report.refused_no_recycle = true;
            }
        } else {
            return Err(windows::Win32::Foundation::E_ABORT.into());
        }
        if refuse {
            return Err(windows::Win32::Foundation::E_ABORT.into());
        }
        Ok(())
    }
    fn PostDeleteItem(&self, dwflags: u32, _psiitem: windows::core::Ref<IShellItem>, hrdelete: HRESULT, psinewlycreated: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        let mut new_path = None;
        let mut new_parsing = None;
        let mut missing = true;
        if let Some(item) = psinewlycreated.as_ref() {
            missing = false;
            new_path = item_path(item, SIGDN_FILESYSPATH);
            new_parsing = item_path(item, windows::Win32::UI::Shell::SIGDN_DESKTOPABSOLUTEPARSING);
        }
        if let Ok(mut g) = self.rec.lock() {
            g.report.posts.push(PostRecord {
                flags: dwflags,
                hr_delete: hrdelete.0,
                new_path,
                new_parsing_name: new_parsing,
                new_item_missing: missing,
            });
        }
        Ok(())
    }
    fn PreNewItem(&self, _f: u32, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(&self, _f: u32, _d: windows::core::Ref<IShellItem>, _n: &PCWSTR, _t: &PCWSTR, _a: u32, _hr: HRESULT, _i: windows::core::Ref<IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _t: u32, _s: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Runs `f` on a fresh thread whose COM apartment is single-threaded.
fn on_sta<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    std::thread::spawn(move || {
        // SAFETY: balanced with CoUninitialize below; this thread owns its apartment.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() {
            return Err(format!("CoInitializeEx failed: {hr:?}"));
        }
        let out = f();
        unsafe { CoUninitialize() };
        out
    })
    .join()
    .map_err(|_| "COM thread panicked".to_string())?
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Sends one path to the Recycle Bin and returns what the progress sink reported.
///
/// Errors if Windows fails the operation, reports a failed `hrDelete`, or never reports a
/// location under `$Recycle.Bin`. Success without a reported bin location is a failure, because
/// the item may then have been permanently deleted. NOTE: the file may already be gone when this
/// returns `Err` for a missing location; callers must treat that as "possibly destroyed".
pub fn recycle_one(path: &Path) -> Result<RecycleReport, String> {
    if !is_plain_absolute(path) {
        return Err(format!(
            "Seek only recycles files given by a full drive path, not {}",
            path.display()
        ));
    }
    let report = run(path, false, 0, true)?;
    report.confirmed_location()?;
    Ok(report)
}

/// True for `X:\...` or `X:/...` only. Relative paths, UNC and `\\?\` paths, `::{CLSID}` shell
/// names and `shell:` names are all refused before Windows gets to resolve them.
fn is_plain_absolute(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// The real recycler: one `IFileOperation` per path, in order.
pub struct ShellRecycler;

impl Recycler for ShellRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>> {
        recycle_in_order(paths, |p| {
            let report = recycle_one(p)?;
            report.confirmed_location().map(str::to_string)
        })
    }
}

/// Runs `one` for each path in order and stops at the first failure: every later path is an
/// `Err` that says it was not attempted.
fn recycle_in_order(
    paths: &[PathBuf],
    mut one: impl FnMut(&Path) -> Result<String, String>,
) -> Vec<Result<String, String>> {
    let mut out = Vec::with_capacity(paths.len());
    let mut stopped = false;
    for p in paths {
        if stopped {
            out.push(Err("Not attempted, because an earlier item could not be confirmed in the Recycle Bin".to_string()));
            continue;
        }
        let r = one(p);
        stopped = r.is_err();
        out.push(r);
    }
    out
}

/// The cause of one item not reaching the bin, as a clause with no full stop and no word on what
/// became of the item: the engine alone knows whether it was left in place or lost.
pub(crate) fn failure_text(path: &Path, hr: i32) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match hr as u32 {
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, COPYENGINE_E_SHARING_VIOLATION_SRC
        // (0x80270027 is what a file held open with no sharing really gives, seen on this PC).
        0x8007_0020 | 0x8007_0021 | 0x8027_0027 => {
            format!("{name} is in use by another program")
        }
        // E_ACCESSDENIED, COPYENGINE_E_ACCESS_DENIED_SRC
        0x8007_0005 | 0x8027_0021 => format!("Windows denied access to {name}"),
        h => format!("Windows reported error 0x{h:08X} for {name}"),
    }
}

/// Asks Windows to recycle `path` but refuses it in `PreDeleteItem`, so nothing is deleted.
/// Returns what the sink saw before the refusal. Used by the spike to see the flags Windows
/// gives for an item that will not fit the bin.
pub fn probe_pre_delete(path: &Path) -> Result<RecycleReport, String> {
    run(path, true, 0, false)
}

/// Spike only (cargo feature `spike`, so production code cannot reach it): runs the same recycle
/// with `extra_flags` added to the operation flags and never turns a missing location or a failed
/// operation into an error, so tests can see what the sink reports when Windows permanently
/// deletes. The deleted file is gone for good. Never use this outside tests on files the test
/// created.
#[cfg(feature = "spike")]
pub fn observe_recycle(path: &Path, extra_flags: u32) -> Result<RecycleReport, String> {
    run(path, false, extra_flags, false)
}

fn run(path: &Path, abort_after_pre: bool, extra_flags: u32, strict: bool) -> Result<RecycleReport, String> {
    let path = path.to_path_buf();
    on_sta(move || {
        let rec = Arc::new(Mutex::new(Rec::default()));
        let w_path = wide(&path);
        // SAFETY: all COM objects live on this thread and are dropped before CoUninitialize.
        unsafe {
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w_path.as_ptr()), None)
                .map_err(|e| format!("cannot open item: {e}"))?;
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
                .map_err(|e| format!("cannot create IFileOperation: {e}"))?;
            op.SetOperationFlags(
                FOFX_RECYCLEONDELETE | FOF_NO_UI | FOFX_EARLYFAILURE | FILEOPERATION_FLAGS(extra_flags),
            )
                .map_err(|e| format!("cannot set flags: {e}"))?;
            let sink: IFileOperationProgressSink = Sink {
                rec: rec.clone(),
                abort_after_pre,
                require_recycle_bit: strict,
            }
            .into();
            let cookie = op.Advise(&sink).map_err(|e| format!("cannot attach sink: {e}"))?;
            let result = op
                .DeleteItem(&item, None::<&IFileOperationProgressSink>)
                .and_then(|()| op.PerformOperations());
            let aborted = op.GetAnyOperationsAborted().unwrap_or(BOOL(0)).as_bool();
            let _ = op.Unadvise(cookie);
            let mut g = rec.lock().map_err(|_| "sink poisoned".to_string())?;
            g.report.perform_error = result.as_ref().err().map(|e| e.to_string());
            g.report.perform_hr = result.as_ref().err().map(|e| e.code().0);
            g.report.aborted = aborted;
            if strict {
                if g.report.refused_no_recycle {
                    return Err(format!(
                        "Windows would delete {} for good instead of recycling it",
                        path.display()
                    ));
                }
                if let Err(e) = &result {
                    return Err(failure_text(&path, e.code().0));
                }
                if aborted {
                    return Err("Windows aborted the operation".into());
                }
            }
        }
        let report = rec.lock().map_err(|_| "sink poisoned".to_string())?.report.clone();
        if strict {
            if let Some(p) = report.posts.iter().find(|p| p.hr_delete < 0) {
                return Err(failure_text(&path, p.hr_delete));
            }
        }
        Ok(report)
    })
}

/// `CMIC_MASK_FLAG_NO_UI`: the verb must not show a dialog (e.g. on a name conflict).
const CMIC_MASK_FLAG_NO_UI: u32 = 0x0000_0400;

/// True when a bin item deleted from `deleted_from` and shown as `display_name` is the file at
/// `original`. The comparison is of the full path, case-insensitive. Explorer's "hide file
/// extensions" may drop the extension from `display_name`, so the extension of the item's `$R`
/// bin file (`bin_path`) is tried when the display name lacks it.
fn matches_original(deleted_from: &str, display_name: &str, bin_path: &str, original: &Path) -> bool {
    let want = original.to_string_lossy().to_lowercase().replace('/', "\\");
    let dir = deleted_from.trim_end_matches(['\\', '/']);
    // Observed on Windows 11: for a bin item the "normal display" name is the full original
    // path, not just the file name. Keep only the last component either way.
    let display_name = display_name.rsplit(['\\', '/']).next().unwrap_or(display_name);
    let mut names = vec![display_name.to_string()];
    if let Some(ext) = Path::new(bin_path).extension().map(|e| e.to_string_lossy().to_string()) {
        let suffix = format!(".{ext}").to_lowercase();
        if !display_name.to_lowercase().ends_with(&suffix) {
            names.push(format!("{display_name}{suffix}"));
        }
    }
    names
        .iter()
        .any(|n| format!("{dir}\\{n}").to_lowercase().replace('/', "\\") == want)
}

fn bin_folder() -> Result<IShellItem, String> {
    // SAFETY: plain COM call.
    unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) }
        .map_err(|e| format!("cannot open the Recycle Bin folder: {e}"))
}

/// The bin item whose file path is `bin_path`, if the bin still lists it. Must run on an STA thread.
///
/// # Safety
/// COM objects returned stay on the calling thread.
unsafe fn find_in_bin(bin_path: &str) -> Result<Option<IShellItem>, String> {
    let folder = bin_folder()?;
    let en: IEnumShellItems = folder
        .BindToHandler(None, &BHID_EnumItems)
        .map_err(|e| format!("cannot list the Recycle Bin: {e}"))?;
    loop {
        let mut batch: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        let _ = en.Next(&mut batch, Some(&mut fetched));
        if fetched == 0 {
            return Ok(None);
        }
        if let Some(it) = batch[0].take() {
            let p = item_path(&it, SIGDN_FILESYSPATH);
            if p.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(bin_path)) {
                return Ok(Some(it));
            }
        }
    }
}

/// True when the Recycle Bin still lists an item whose file path is `bin_path`. A bin that can't
/// be listed counts as "no".
pub fn in_bin(bin_path: &str) -> bool {
    let bin_path = bin_path.to_string();
    // SAFETY: COM objects stay on the STA thread and drop before CoUninitialize.
    on_sta(move || unsafe { find_in_bin(&bin_path).map(|i| i.is_some()) }).unwrap_or(false)
}

/// `System.Recycle.DateDeleted` (fmtid 9B174B33-..., pid 3) of a bin item, in unix epoch ms.
fn deleted_ms(item2: &IShellItem2) -> Option<i64> {
    let key = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871),
        pid: 3,
    };
    // SAFETY: plain COM call on a live item.
    let ft = unsafe { item2.GetFileTime(&key) }.ok()?;
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    // FILETIME counts 100 ns from 1601; unix time from 1970 (116_444_736_000_000_000 ticks later).
    (ticks as i64).checked_sub(116_444_736_000_000_000).map(|t| t / 10_000)
}

/// How long before the removal was recorded its deletion may have happened (a big batch takes a
/// while), and how far after (clock jitter).
pub const DELETED_BEFORE_MS: i64 = 30 * 60 * 1000;
pub const DELETED_AFTER_MS: i64 = 2 * 60 * 1000;

/// Whether a deletion date fits a removal recorded at `near_ms`.
pub fn deletion_fits(deleted_ms: i64, near_ms: i64) -> bool {
    deleted_ms >= near_ms - DELETED_BEFORE_MS && deleted_ms <= near_ms + DELETED_AFTER_MS
}

/// Restores the item whose Recycle Bin file path is `bin_path` through the bin's own `undelete`
/// verb, then checks `original` exists again. Fails (without retrying) if the item is not in the
/// bin or Windows refuses. The item's deletion date is not checked; see `restore_near`.
pub fn restore(bin_path: &str, original: &Path) -> Result<(), String> {
    restore_checked(bin_path, original, None)
}

/// `restore`, and the bin item must also have been deleted within [`deletion_fits`] of `near_ms`
/// (the time the removal was recorded). An item whose date can't be read is refused.
pub fn restore_near(bin_path: &str, original: &Path, near_ms: i64) -> Result<(), String> {
    restore_checked(bin_path, original, Some(near_ms))
}

fn restore_checked(bin_path: &str, original: &Path, near_ms: Option<i64>) -> Result<(), String> {
    let bin_path = bin_path.to_string();
    let original = original.to_path_buf();
    on_sta(move || {
        // SAFETY: COM objects stay on this thread and drop before CoUninitialize.
        unsafe {
            let item = find_in_bin(&bin_path)?.ok_or_else(|| "No longer in the Recycle Bin".to_string())?;
            // Match on the full original path too, so a same-named file from another folder can
            // never be restored by mistake.
            let item2 = item
                .cast::<IShellItem2>()
                .map_err(|e| format!("cannot read the bin item's properties: {e}"))?;
            let key = PROPERTYKEY {
                fmtid: windows::core::GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871),
                pid: 2, // System.Recycle.DeletedFrom
            };
            let dir = item2
                .GetString(&key)
                .ok()
                .and_then(pwstr_to_string)
                .ok_or_else(|| "the bin item does not say where it was deleted from".to_string())?;
            let display = item_path(&item, SIGDN_NORMALDISPLAY).unwrap_or_default();
            if !matches_original(&dir, &display, &bin_path, &original) {
                return Err(format!(
                    "bin item came from {dir} as {display}, which is not {}",
                    original.display()
                ));
            }
            // The deletion date too: a same-path item from an older removal is another removal.
            if let Some(near) = near_ms {
                let when = deleted_ms(&item2).ok_or_else(|| {
                    "Windows doesn't say when the item in the Recycle Bin was deleted, so it wasn't put back.".to_string()
                })?;
                if !deletion_fits(when, near) {
                    return Err("The item in the Recycle Bin is from a different removal.".to_string());
                }
            }
            let menu: IContextMenu = item
                .BindToHandler(None, &BHID_SFUIObject)
                .map_err(|e| format!("cannot get the item's menu: {e}"))?;
            let hmenu = CreatePopupMenu().map_err(|e| format!("cannot create a menu: {e}"))?;
            let q = menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, 0);
            let invoked = if q.is_err() {
                Err(format!("QueryContextMenu failed: {q:?}"))
            } else {
                let info = CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                    fMask: CMIC_MASK_FLAG_NO_UI,
                    hwnd: HWND::default(),
                    lpVerb: PCSTR(c"undelete".as_ptr().cast()),
                    ..Default::default()
                };
                menu.InvokeCommand(&info).map_err(|e| format!("undelete verb failed: {e}"))
            };
            let _ = DestroyMenu(hmenu);
            invoked?;
        }
        if original.exists() {
            Ok(())
        } else {
            Err(format!("restore ran but {} did not come back", original.display()))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{failure_text, is_plain_absolute, matches_original, recycle_in_order, recycle_one};
    use std::path::{Path, PathBuf};

    #[test]
    fn recycle_one_refuses_odd_paths_before_resolving_them() {
        for bad in [
            "relative.txt",
            r"..\x.txt",
            r"C:x.txt",
            r"\\server\share\f.txt",
            r"\\?\C:\f.txt",
            r"\\localhost\C$\f.txt",
            "::{645FF040-5081-101B-9F08-00AA002F954E}",
            "shell:RecycleBinFolder",
            "",
        ] {
            assert!(!is_plain_absolute(Path::new(bad)), "{bad}");
            assert!(recycle_one(Path::new(bad)).is_err(), "{bad}");
        }
        assert!(is_plain_absolute(Path::new(r"C:\a\b.txt")));
        assert!(is_plain_absolute(Path::new("d:/a/b.txt")));
    }

    #[test]
    fn a_batch_stops_at_the_first_failure_and_marks_the_rest() {
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(PathBuf::from).collect();
        let mut seen = Vec::new();
        let out = recycle_in_order(&paths, |p| {
            seen.push(p.to_path_buf());
            if p == Path::new("b") {
                Err("b failed".into())
            } else {
                Ok(format!("bin-{}", p.display()))
            }
        });
        assert_eq!(seen, vec![PathBuf::from("a"), PathBuf::from("b")], "c and d never attempted");
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], Ok("bin-a".to_string()));
        assert_eq!(out[1], Err("b failed".to_string()));
        assert!(out[2].as_ref().unwrap_err().contains("Not attempted"));
        assert!(out[3].is_err());
    }

    #[test]
    fn failure_text_names_the_file_and_says_in_use() {
        let t = failure_text(Path::new(r"C:\x\locked.bin"), 0x8007_0020_u32 as i32);
        assert!(t.contains("locked.bin") && t.contains("in use"), "{t}");
        let t = failure_text(Path::new(r"C:\x\locked.bin"), 0x8027_0027_u32 as i32);
        assert!(t.contains("in use"), "{t}");
        for hr in [0x8007_0020_u32, 0x8027_0027, 0x8007_0005, 0x8027_0021, 0x8027_0000] {
            let t = failure_text(Path::new(r"C:\x\f.bin"), hr as i32);
            assert!(!t.contains("left in place") && !t.ends_with('.'), "cause only: {t}");
        }
    }

    fn post(hr: i32, loc: Option<&str>) -> super::PostRecord {
        super::PostRecord {
            flags: 0x282,
            hr_delete: hr,
            new_path: loc.map(str::to_string),
            new_parsing_name: None,
            new_item_missing: loc.is_none(),
        }
    }

    fn report(posts: Vec<super::PostRecord>) -> super::RecycleReport {
        super::RecycleReport {
            posts,
            ..Default::default()
        }
    }

    #[test]
    fn a_result_needs_every_success_post_to_name_a_bin_location() {
        const OK: i32 = 0x0027_0008;
        let bin = r"C:\$Recycle.Bin\S-1\$RAAAAAA.txt";
        assert_eq!(report(vec![post(OK, Some(bin))]).confirmed_location(), Ok(bin));
        assert_eq!(report(vec![post(OK, Some(bin)), post(OK, Some(bin))]).confirmed_location(), Ok(bin));
        // No posts at all, a missing location, one missing among good ones, or a location
        // outside the bin: all refused.
        assert!(report(vec![]).confirmed_location().is_err());
        assert!(report(vec![post(OK, None)]).confirmed_location().is_err());
        assert!(report(vec![post(OK, Some(bin)), post(OK, None)]).confirmed_location().is_err());
        assert!(report(vec![post(OK, None), post(OK, Some(bin))]).confirmed_location().is_err());
        assert!(report(vec![post(OK, Some(r"C:\Users\a\x.txt"))]).confirmed_location().is_err());
        // A failed post alone confirms nothing.
        assert!(report(vec![post(0x8000_4004_u32 as i32, None)]).confirmed_location().is_err());
    }

    const BIN: &str = r"C:\$Recycle.Bin\S-1\$RAB12CD.txt";

    #[test]
    fn full_path_matches_ignoring_case() {
        assert!(matches_original(r"C:\Users\A\Docs", "Report.TXT", BIN, Path::new(r"c:\users\a\docs\report.txt")));
    }

    #[test]
    fn same_name_from_another_folder_is_refused() {
        assert!(!matches_original(r"C:\Users\A\Other", "report.txt", BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
    }

    #[test]
    fn extensionless_display_name_matches_through_the_bin_extension() {
        assert!(matches_original(r"C:\Users\A\Docs", "report", BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
    }

    #[test]
    fn extensionless_display_name_does_not_match_a_different_file() {
        assert!(!matches_original(r"C:\Users\A\Docs", "report", BIN, Path::new(r"C:\Users\A\Docs\other.txt")));
        assert!(!matches_original(r"C:\Users\A\Other", "report", BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
    }

    #[test]
    fn display_name_that_is_the_full_path_is_reduced_to_its_file_name() {
        // What Windows 11 really returns for a bin item's normal display name.
        let shown = r"C:\Users\A\Docs\report.txt";
        assert!(matches_original(r"C:\Users\A\Docs", shown, BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
        assert!(!matches_original(r"C:\Users\A\Other", shown, BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
    }

    #[test]
    fn trailing_separator_on_the_folder_is_fine() {
        assert!(matches_original("C:\\Users\\A\\Docs\\", "report.txt", BIN, Path::new(r"C:\Users\A\Docs\report.txt")));
    }
}
