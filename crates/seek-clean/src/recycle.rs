//! Recycling through `IFileOperation`, and restoring through the Recycle Bin shell folder.
//!
//! This is the Task 1 spike: it records exactly what the progress sink reports, so later tasks
//! can decide what counts as proof that an item reached the bin.

use std::path::Path;
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, BOOL, HRESULT, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, PROPERTYKEY};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOperation, IContextMenu, IEnumShellItems, IFileOperation, IFileOperationProgressSink,
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
}

impl RecycleReport {
    /// The bin location from the first successful `PostDeleteItem` that carried one.
    pub fn bin_location(&self) -> Option<&str> {
        self.posts
            .iter()
            .filter(|p| p.hr_delete >= 0)
            .find_map(|p| p.new_path.as_deref().or(p.new_parsing_name.as_deref()))
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
}

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
        if let Ok(mut g) = self.rec.lock() {
            g.report.pre_flags.push(dwflags);
        }
        if self.abort_after_pre {
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
/// Errors if Windows fails the operation or reports a failed `hrDelete`. A success here does not
/// by itself prove the item is in the bin; the caller inspects the report.
pub fn recycle_one(path: &Path) -> Result<RecycleReport, String> {
    run(path, false)
}

/// Asks Windows to recycle `path` but refuses it in `PreDeleteItem`, so nothing is deleted.
/// Returns what the sink saw before the refusal. Used by the spike to see the flags Windows
/// gives for an item that will not fit the bin.
pub fn probe_pre_delete(path: &Path) -> Result<RecycleReport, String> {
    run(path, true)
}

fn run(path: &Path, abort_after_pre: bool) -> Result<RecycleReport, String> {
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
            op.SetOperationFlags(FOFX_RECYCLEONDELETE | FOF_NO_UI | FOFX_EARLYFAILURE)
                .map_err(|e| format!("cannot set flags: {e}"))?;
            let sink: IFileOperationProgressSink = Sink { rec: rec.clone(), abort_after_pre }.into();
            let cookie = op.Advise(&sink).map_err(|e| format!("cannot attach sink: {e}"))?;
            let result = op
                .DeleteItem(&item, None::<&IFileOperationProgressSink>)
                .and_then(|()| op.PerformOperations());
            let aborted = op.GetAnyOperationsAborted().unwrap_or(BOOL(0)).as_bool();
            let _ = op.Unadvise(cookie);
            if !abort_after_pre {
                result.map_err(|e| format!("recycle failed: {e}"))?;
                if aborted {
                    return Err("Windows aborted the operation".into());
                }
            }
        }
        let report = rec.lock().map_err(|_| "sink poisoned".to_string())?.report.clone();
        if !abort_after_pre {
            if let Some(p) = report.posts.iter().find(|p| p.hr_delete < 0) {
                return Err(format!("PostDeleteItem reported failure, hrDelete = 0x{:08X}", p.hr_delete as u32));
            }
        }
        Ok(report)
    })
}

fn bin_folder() -> Result<IShellItem, String> {
    // SAFETY: plain COM call.
    unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) }
        .map_err(|e| format!("cannot open the Recycle Bin folder: {e}"))
}

/// Restores the item whose Recycle Bin file path is `bin_path` through the bin's own `undelete`
/// verb, then checks `original` exists again. Fails (without retrying) if the item is not in the
/// bin or Windows refuses.
pub fn restore(bin_path: &str, original: &Path) -> Result<(), String> {
    let bin_path = bin_path.to_string();
    let original = original.to_path_buf();
    on_sta(move || {
        // SAFETY: COM objects stay on this thread and drop before CoUninitialize.
        unsafe {
            let folder = bin_folder()?;
            let en: IEnumShellItems = folder
                .BindToHandler(None, &BHID_EnumItems)
                .map_err(|e| format!("cannot list the Recycle Bin: {e}"))?;
            let mut found: Option<IShellItem> = None;
            loop {
                let mut batch: [Option<IShellItem>; 1] = [None];
                let mut fetched = 0u32;
                let _ = en.Next(&mut batch, Some(&mut fetched));
                if fetched == 0 {
                    break;
                }
                if let Some(it) = batch[0].take() {
                    let p = item_path(&it, SIGDN_FILESYSPATH);
                    if p.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&bin_path)) {
                        found = Some(it);
                        break;
                    }
                }
            }
            let item = found.ok_or_else(|| "No longer in the Recycle Bin".to_string())?;
            // Match on the original location too, so a reused name can never restore the wrong file.
            if let Ok(item2) = item.cast::<IShellItem2>() {
                let key = PROPERTYKEY {
                    fmtid: windows::core::GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871),
                    pid: 2, // System.Recycle.DeletedFrom
                };
                if let Some(dir) = item2.GetString(&key).ok().and_then(pwstr_to_string) {
                    let name = item_path(&item, SIGDN_NORMALDISPLAY).unwrap_or_default();
                    let reported = Path::new(&dir).join(&name);
                    let same = reported.to_string_lossy().eq_ignore_ascii_case(&original.to_string_lossy());
                    if !same && original.file_name().map(|n| n.to_string_lossy().to_lowercase())
                        != reported.file_name().map(|n| n.to_string_lossy().to_lowercase())
                    {
                        return Err(format!("bin item came from {}, not {}", reported.display(), original.display()));
                    }
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
