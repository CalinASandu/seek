//! Canonical form and comparison of Windows paths.
//!
//! Safety rules compare paths as strings, so every spelling of the same place must reduce to one
//! string: case, `/` vs `\`, `\\?\`, `.` and `..`, trailing slashes and 8.3 short names.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, GetLongPathNameW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Lowercased, `\\?\` stripped, `.`/`..` resolved lexically (never touching the disk), trailing
/// slash removed, and 8.3 short names expanded for the longest prefix that exists. The path
/// itself need not exist. A drive root is `c:`.
pub fn canon(p: &Path) -> String {
    let raw = p.to_string_lossy().replace('/', "\\");
    let lexical = lexical(&strip_verbatim(&raw));
    let expanded = if lexical.contains('~') { expand_short(&lexical) } else { lexical };
    expanded.to_lowercase()
}

/// Like `canon` but never touches the disk: 8.3 short names stay as written. For callers that
/// must not wait on a drive that may be gone.
pub fn canon_lexical(p: &Path) -> String {
    let raw = p.to_string_lossy().replace('/', "\\");
    lexical(&strip_verbatim(&raw)).to_lowercase()
}

/// True if `child` is `parent` or lies under it, compared by whole segments. Both must already
/// be in `canon` form. `c:\program files extra` is not inside `c:\program files`.
pub fn is_inside(child: &str, parent: &str) -> bool {
    let parent = parent.trim_end_matches('\\');
    if parent.is_empty() {
        return false;
    }
    child == parent
        || (child.len() > parent.len()
            && child.starts_with(parent)
            && child.as_bytes()[parent.len()] == b'\\')
}

fn strip_verbatim(raw: &str) -> String {
    if let Some(head) = raw.get(..8) {
        if head.eq_ignore_ascii_case("\\\\?\\UNC\\") {
            // `\\?\UNC\server\share` is `\\server\share`.
            return format!("\\\\{}", &raw[8..]);
        }
    }
    if raw.starts_with("\\\\?\\") || raw.starts_with("\\\\.\\") {
        return raw[4..].to_string();
    }
    raw.to_string()
}

/// Resolve `.`/`..` and drop empty segments and trailing dots and spaces (which Win32 ignores).
fn lexical(raw: &str) -> String {
    let unc = raw.starts_with("\\\\");
    let mut segs: Vec<&str> = raw.split('\\').filter(|s| !s.is_empty()).collect();
    // The part that `..` can never pop: `\\server\share` or `c:`.
    let keep = if unc {
        segs.len().min(2)
    } else if segs.first().is_some_and(|s| s.len() == 2 && s.as_bytes()[1] == b':') {
        1
    } else {
        0
    };
    let rooted = keep > 0;
    let tail = segs.split_off(keep);
    let mut out: Vec<&str> = Vec::new();
    for s in tail {
        if s == "." {
            continue;
        }
        if s == ".." {
            if !out.is_empty() && *out.last().unwrap() != ".." {
                out.pop();
            } else if !rooted {
                out.push("..");
            }
            continue;
        }
        // An alternate data stream (`dir:x`, `dir::$INDEX_ALLOCATION`) names the same item.
        let s = s.split(':').next().unwrap_or("");
        let trimmed = s.trim_end_matches(['.', ' ']);
        if trimmed.is_empty() {
            continue;
        }
        out.push(trimmed);
    }
    let mut result = String::new();
    if unc {
        result.push_str("\\\\");
    }
    result.push_str(&segs.join("\\"));
    for s in out {
        if !result.is_empty() && !result.ends_with("\\\\") {
            result.push('\\');
        }
        result.push_str(s);
    }
    result
}

/// Expand short names using the longest prefix that exists on disk. Prefixes with no `~` cannot
/// contain a short name, so the search stops there.
fn expand_short(path: &str) -> String {
    let mut cuts: Vec<usize> = path.match_indices('\\').map(|(i, _)| i).collect();
    cuts.push(path.len());
    for &cut in cuts.iter().rev() {
        let prefix = &path[..cut];
        if prefix.len() <= 2 {
            break;
        }
        if !prefix.contains('~') {
            break;
        }
        if let Some(long) = long_path(prefix) {
            return format!("{}{}", long, &path[cut..]);
        }
    }
    path.to_string()
}

fn long_path(path: &str) -> Option<String> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `wide` is NUL-terminated and outlives the call; `buf` is a valid buffer.
        let n = unsafe { GetLongPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// A handle opened only to ask questions (no data access), closed on drop.
pub(crate) struct QueryHandle(pub(crate) HANDLE);

impl Drop for QueryHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by CreateFileW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Opens a file or folder without reading its contents and without following a link in the
/// last segment. Parent links are still followed by Windows, which `real_location` relies on.
pub(crate) fn open_query(path: &Path) -> Option<QueryHandle> {
    open_flags(path, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
}

fn open_flags(path: &Path, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Option<QueryHandle> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    };
    h.ok().map(QueryHandle)
}

/// Where an existing path really is, in `canon` form, with every link in its parents resolved
/// (a junction into `C:\Windows` shows up as `c:\windows\...`). A link in the last segment is
/// reported as itself. `None` if it can't be opened.
pub fn real_location(path: &Path) -> Option<String> {
    final_path(&open_query(path)?)
}

/// Like `real_location`, but a link in the last segment is followed to its target too.
pub fn target_location(path: &Path) -> Option<String> {
    final_path(&open_flags(path, FILE_FLAG_BACKUP_SEMANTICS)?)
}

fn final_path(h: &QueryHandle) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `h` is a valid handle and `buf` a valid buffer.
        let n = unsafe { GetFinalPathNameByHandleW(h.0, &mut buf, FILE_NAME_NORMALIZED) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            let s = String::from_utf16_lossy(&buf[..n]);
            return Some(canon(Path::new(&s)));
        }
        buf.resize(n + 1, 0);
    }
}

/// Test helper: makes a junction `link` -> `target` with `mklink /J`. Tests must `assert!` the
/// result: a junction test that quietly skips proves nothing.
#[cfg(test)]
pub(crate) fn make_junction(link: &Path, target: &Path) -> bool {
    // `mklink` reads a `/` as a switch, so only backslashes.
    let fix = |p: &Path| p.to_string_lossy().replace('/', "\\");
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(fix(link))
        .arg(fix(target))
        .output();
    match out {
        Ok(o) if o.status.success() => true,
        other => {
            eprintln!("mklink /J failed: {other:?}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;

    #[test]
    fn real_location_resolves_a_junction_in_the_parents() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("f.txt"), b"x").unwrap();
        let junc = dir.path().join("junc");
        assert!(make_junction(&junc, &real));
        let via = real_location(&junc.join("f.txt")).unwrap();
        let direct = real_location(&real.join("f.txt")).unwrap();
        assert_eq!(via, direct);
        assert!(via.ends_with(r"\real\f.txt"), "{via}");
        assert!(real_location(&dir.path().join("missing")).is_none());
    }

    fn c(p: &str) -> String {
        canon(Path::new(p))
    }

    fn short_of(path: &str) -> Option<String> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
    }

    #[test]
    fn spellings_reduce_to_one_form() {
        let want = r"c:\windows\system32\x";
        for p in [
            r"C:\Windows\System32\x",
            r"c:\WINDOWS\system32\x",
            r"C:/Windows/System32/x",
            r"\\?\C:\Windows\System32\x",
            r"C:\Windows\.\System32\x\",
            r"C:\Users\me\..\..\Windows\System32\x",
            r"C:\Windows\System32\x\..\x",
        ] {
            assert_eq!(c(p), want, "{p}");
        }
    }

    #[test]
    fn roots_unc_and_overshooting_dotdot() {
        assert_eq!(c(r"C:\"), "c:");
        assert_eq!(c(r"C:"), "c:");
        assert_eq!(c(r"C:\..\..\Windows"), r"c:\windows");
        assert_eq!(c(r"\\?\UNC\Server\Share\a\..\b"), r"\\server\share\b");
        assert_eq!(c(r"\\Server\Share\"), r"\\server\share");
        assert_eq!(c(r"C:\Windows.\x "), r"c:\windows\x");
    }

    #[test]
    fn is_inside_compares_whole_segments() {
        assert!(is_inside(r"c:\program files\app", r"c:\program files"));
        assert!(is_inside(r"c:\program files", r"c:\program files"));
        assert!(!is_inside(r"c:\program files extra\x", r"c:\program files"));
        assert!(!is_inside(r"c:\program", r"c:\program files"));
        assert!(is_inside(r"c:\x", "c:"));
        assert!(!is_inside(r"d:\x", "c:"));
        assert!(!is_inside(r"c:\x", ""));
    }

    #[test]
    fn short_name_of_a_missing_file_under_program_files_expands() {
        let long = std::env::var("ProgramFiles").expect("ProgramFiles");
        let Some(short) = short_of(&long) else {
            panic!("GetShortPathNameW failed on Program Files");
        };
        if short.eq_ignore_ascii_case(&long) {
            eprintln!("8.3 names are off for Program Files here; expansion not asserted");
            assert_eq!(c(&format!(r"{long}\nope\x")), format!(r"{}\nope\x", long.to_lowercase()));
            return;
        }
        assert_eq!(c(&format!(r"{short}\nope\x")), format!(r"{}\nope\x", long.to_lowercase()));
    }

    #[test]
    fn short_name_expansion_against_a_real_short_name() {
        let dir = tempfile::tempdir().unwrap();
        let long_dir = dir.path().join("a long folder name here");
        std::fs::create_dir(&long_dir).unwrap();
        let long_str = long_dir.to_string_lossy().into_owned();
        let Some(short) = short_of(&long_str) else {
            eprintln!("no short name available; skipped");
            return;
        };
        if short.eq_ignore_ascii_case(&long_str) {
            eprintln!("8.3 names are off on the temp drive; skipped");
            return;
        }
        let from_short = canon(&Path::new(&short).join("missing").join("f.txt"));
        let from_long = canon(&long_dir.join("missing").join("f.txt"));
        assert_eq!(from_short, from_long);
        assert!(!from_short.contains('~') || long_str.contains('~'), "{from_short}");
    }
}
