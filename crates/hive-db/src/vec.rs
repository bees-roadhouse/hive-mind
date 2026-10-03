//! The one `unsafe` in the host: registering sqlite-vec with the engine
//! (D41 §3). Once per process, before any connection is opened, and the
//! engine then loads it into every connection it makes, so a file with a
//! `vec0` table in it reads on any connection this process holds.
//!
//! The gate holds the rest of the workspace to no `unsafe` at all
//! (`hive-repodocs`); this module is the named exception, and a second one
//! means editing that test and saying why.
#![allow(unsafe_code)]

use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

type InitFn = extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> c_int;

/// Registers sqlite-vec as an auto-extension. Idempotent and cheap after
/// the first call.
pub(crate) fn register() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        // SAFETY: `sqlite3_vec_init` is the extension's documented entry
        // point with the signature `sqlite3_auto_extension` expects; the
        // transmute only renames the type the `sqlite-vec` crate exports it
        // under. Registering before any connection exists is the documented
        // use, and the engine calls it with a live handle of its own.
        unsafe {
            let init: InitFn =
                std::mem::transmute::<*const (), InitFn>(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(init));
        }
    });
}
