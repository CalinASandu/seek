//! The plain sentences the engine gives when it leaves something in place.

use crate::model::Lock;

pub(crate) const GONE: &str = "It is no longer there, so nothing was removed.";
pub(crate) const CHANGED: &str = "This changed after you reviewed it, so it was left in place.";
pub(crate) const LAST_COPY: &str = "This is the last copy, so it was left in place.";
pub(crate) const NO_TWIN: &str =
    "No other copy could be confirmed as identical and unchanged, so it was left in place.";
pub(crate) const NEEDS_ADMIN: &str = "Windows admin rights are needed for this.";
pub(crate) const NESTED: &str =
    "This is inside a folder that is also being removed, so it was left in place.";
pub(crate) const STOPPED: &str = "Removal stopped after an item could not be confirmed in the Recycle Bin, so this was left in place.";
pub(crate) const DESTROYED: &str = "This is gone or partly gone, but Windows did not confirm it in the Recycle Bin, so it may have been deleted for good.";
pub(crate) const ADMIN_SILENT: &str =
    "The admin step gave no answer for this. Check the Recycle Bin before trying again.";

pub(crate) fn lock_reason(l: Lock) -> &'static str {
    match l {
        Lock::SystemFolder => "This is, or holds, a part of Windows or of your user folders, so it was left in place.",
        Lock::ProgramFiles => "This is in Program Files, so it was left in place.",
        Lock::OtherUser => "This belongs to another user, so it was left in place.",
        Lock::AppData => "This is in an app data folder, so it was left in place.",
        Lock::ToolFolder => "This is inside a tool folder (.git, node_modules and the like), so it was left in place.",
        Lock::NoRecycleBin => "Windows might delete this for good because of how this drive or Windows is set up, so it was left in place.",
        Lock::BinFull => "The Recycle Bin on this drive is too full to hold this. Empty it first, then try again. It was left in place.",
        Lock::PathTooLong => "Some paths in this are too long for the Recycle Bin to hold reliably, so it was left in place.",
        Lock::CloudPlaceholder => "This is an online-only cloud file, so it was left in place.",
        Lock::Protected => "You marked this, or something inside it, as always protected, so it was left in place.",
        Lock::LinkedPc => "This is on a linked PC. Seek only removes files on this PC, so it was left in place.",
        Lock::UnsupportedPath => "Seek cannot remove this kind of path, so it was left in place.",
        Lock::ReparsePoint => "This is a link to somewhere else, so it was left in place.",
        Lock::Claimed => "An app or a running program is using this folder, so it was left in place.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every lock has its own non-empty sentence; adding a variant breaks the match below.
    #[test]
    fn every_lock_has_a_distinct_reason() {
        let all = [
            Lock::SystemFolder,
            Lock::ProgramFiles,
            Lock::OtherUser,
            Lock::AppData,
            Lock::ToolFolder,
            Lock::NoRecycleBin,
            Lock::BinFull,
            Lock::PathTooLong,
            Lock::CloudPlaceholder,
            Lock::Protected,
            Lock::LinkedPc,
            Lock::UnsupportedPath,
            Lock::ReparsePoint,
            Lock::Claimed,
        ];
        for l in all {
            match l {
                Lock::SystemFolder | Lock::ProgramFiles | Lock::OtherUser | Lock::AppData
                | Lock::ToolFolder | Lock::NoRecycleBin | Lock::BinFull | Lock::PathTooLong
                | Lock::CloudPlaceholder | Lock::Protected | Lock::LinkedPc
                | Lock::UnsupportedPath | Lock::ReparsePoint | Lock::Claimed => {}
            }
            assert!(lock_reason(l).len() > 20, "{l:?}");
        }
        let texts: std::collections::HashSet<_> = all.iter().map(|l| lock_reason(*l)).collect();
        assert_eq!(texts.len(), all.len(), "two locks share a sentence");
        assert!(lock_reason(Lock::BinFull).contains("Empty it first"));
        assert!(!lock_reason(Lock::NoRecycleBin).contains("full"));
    }
}
