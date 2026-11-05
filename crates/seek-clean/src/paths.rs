//! Canonical form and comparison of Windows paths.
//!
//! Safety rules compare paths as strings, so every spelling of the same place must reduce to one
//! string: case, `/` vs `\`, `\\?\`, `.` and `..`, trailing slashes and 8.3 short names.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetLongPathNameW;

/// Lowercased, `\\?\` stripped, `.`/`..` resolved lexically (never touching the disk), trailing
/// slash removed, and 8.3 short names expanded for the longest prefix that exists. The path
/// itself need not exist. A drive root is `c:`.
pub fn canon(p: &Path) -> String {
    let raw = p.to_string_lossy().replace('/', "\\");
    let lexical = lexical(&strip_verbatim(&raw));
    let expanded = if lexical.contains('~') { expand_short(&lexical) } else { lexical };
    expanded.to_lowercase()
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;

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
