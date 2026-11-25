//! Mac App Store edition, macOS only: thin safe wrappers over `native/mas.m`.
use crate::bookmarks::Backend;
use std::ffi::{c_char, c_int, CString};

extern "C" {
    fn backer_free(p: *mut u8);
    fn backer_bookmark_create(path: *const c_char, len: *mut usize) -> *mut u8;
    fn backer_bookmark_open(bytes: *const u8, len: usize) -> c_int;
    fn backer_login_item_status() -> c_int;
    fn backer_login_item_set(on: c_int) -> c_int;
    fn backer_launched_as_login_item() -> c_int;
    fn backer_notify(title: *const c_char, body: *const c_char);
}

/// Security-scoped bookmarks through the system.
pub struct SystemBookmarks;

impl Backend for SystemBookmarks {
    fn create(&self, path: &str) -> Option<Vec<u8>> {
        let p = CString::new(path).ok()?;
        let mut len = 0usize;
        let ptr = unsafe { backer_bookmark_create(p.as_ptr(), &mut len) };
        if ptr.is_null() {
            return None;
        }
        let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        unsafe { backer_free(ptr) };
        Some(data)
    }
    fn open(&self, data: &[u8]) -> Option<bool> {
        match unsafe { backer_bookmark_open(data.as_ptr(), data.len()) } {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItem {
    Off,
    On,
    /// Registered, but the user still has to allow it in System Settings, Login Items.
    NeedsApproval,
}

pub fn login_item() -> LoginItem {
    match unsafe { backer_login_item_status() } {
        1 => LoginItem::On,
        2 => LoginItem::NeedsApproval,
        _ => LoginItem::Off,
    }
}

pub fn set_login_item(on: bool) -> bool {
    unsafe { backer_login_item_set(on as c_int) == 1 }
}

pub fn launched_as_login_item() -> bool {
    unsafe { backer_launched_as_login_item() == 1 }
}

pub fn notify(title: &str, body: &str) {
    if let (Ok(t), Ok(b)) = (CString::new(title), CString::new(body)) {
        unsafe { backer_notify(t.as_ptr(), b.as_ptr()) };
    }
}
