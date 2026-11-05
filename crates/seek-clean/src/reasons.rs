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
        Lock::NoRecycleBin => "This drive has no Recycle Bin that can hold it, so it was left in place.",
        Lock::CloudPlaceholder => "This is an online-only cloud file, so it was left in place.",
        Lock::Protected => "You marked this, or something inside it, as always protected, so it was left in place.",
        Lock::LinkedPc => "This is on a linked PC. Seek only removes files on this PC, so it was left in place.",
        Lock::UnsupportedPath => "Seek cannot remove this kind of path, so it was left in place.",
        Lock::ReparsePoint => "This is a link to somewhere else, so it was left in place.",
        Lock::Claimed => "An app or a running program is using this folder, so it was left in place.",
    }
}
