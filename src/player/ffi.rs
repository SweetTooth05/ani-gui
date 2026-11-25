//! Minimal libmpv bindings, resolved at runtime with `libloading`.
//! Constants and layouts follow `mpv/client.h` (API 2.5) and `mpv/render.h`.

use std::ffi::{CStr, c_char, c_double, c_int, c_void};

use libloading::Library;

#[repr(C)]
pub struct MpvHandle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct MpvRenderContext {
    _private: [u8; 0],
}

pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_DOUBLE: c_int = 5;

pub const MPV_ERROR_INVALID_PARAMETER: c_int = -4;

pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_END_FILE_REASON_ERROR: c_int = 4;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
pub const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
pub const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
pub const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

pub const MPV_RENDER_API_TYPE_SW: &CStr = c"sw";
pub const MPV_RENDER_UPDATE_FRAME: u64 = 1 << 0;

#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct MpvRenderParam {
    pub kind: c_int,
    pub data: *mut c_void,
}

pub type UpdateFn = unsafe extern "C" fn(ctx: *mut c_void);

/// Function pointers into a loaded libmpv; valid as long as `_lib` lives.
pub struct Api {
    pub create: unsafe extern "C" fn() -> *mut MpvHandle,
    pub initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    pub set_option_string:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    pub set_property_string:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    #[cfg(test)]
    pub get_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char,
    #[cfg(test)]
    pub free: unsafe extern "C" fn(*mut c_void),
    pub command: unsafe extern "C" fn(*mut MpvHandle, *mut *const c_char) -> c_int,
    pub observe_property: unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    pub wait_event: unsafe extern "C" fn(*mut MpvHandle, c_double) -> *mut MpvEvent,
    pub wakeup: unsafe extern "C" fn(*mut MpvHandle),
    pub terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    pub render_context_create: unsafe extern "C" fn(
        *mut *mut MpvRenderContext,
        *mut MpvHandle,
        *mut MpvRenderParam,
    ) -> c_int,
    pub render_context_set_update_callback:
        unsafe extern "C" fn(*mut MpvRenderContext, Option<UpdateFn>, *mut c_void),
    pub render_context_update: unsafe extern "C" fn(*mut MpvRenderContext) -> u64,
    pub render_context_render:
        unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int,
    pub render_context_free: unsafe extern "C" fn(*mut MpvRenderContext),
    pub error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    _lib: Library,
}

impl Api {
    pub fn load(name: &str) -> Result<Api, libloading::Error> {
        // SAFETY: loading libmpv runs no initialisers with preconditions, and
        // every symbol type below matches its declaration in the mpv headers.
        unsafe {
            let lib = Library::new(name)?;
            Ok(Api {
                create: *lib.get(b"mpv_create\0")?,
                initialize: *lib.get(b"mpv_initialize\0")?,
                set_option_string: *lib.get(b"mpv_set_option_string\0")?,
                set_property_string: *lib.get(b"mpv_set_property_string\0")?,
                #[cfg(test)]
                get_property_string: *lib.get(b"mpv_get_property_string\0")?,
                #[cfg(test)]
                free: *lib.get(b"mpv_free\0")?,
                command: *lib.get(b"mpv_command\0")?,
                observe_property: *lib.get(b"mpv_observe_property\0")?,
                wait_event: *lib.get(b"mpv_wait_event\0")?,
                wakeup: *lib.get(b"mpv_wakeup\0")?,
                terminate_destroy: *lib.get(b"mpv_terminate_destroy\0")?,
                render_context_create: *lib.get(b"mpv_render_context_create\0")?,
                render_context_set_update_callback: *lib
                    .get(b"mpv_render_context_set_update_callback\0")?,
                render_context_update: *lib.get(b"mpv_render_context_update\0")?,
                render_context_render: *lib.get(b"mpv_render_context_render\0")?,
                render_context_free: *lib.get(b"mpv_render_context_free\0")?,
                error_string: *lib.get(b"mpv_error_string\0")?,
                _lib: lib,
            })
        }
    }

    pub fn error_message(&self, code: c_int) -> String {
        // SAFETY: mpv_error_string returns a static string for any code.
        let msg = unsafe { (self.error_string)(code) };
        if msg.is_null() {
            return format!("mpv error {code}");
        }
        // SAFETY: non-null result is a NUL-terminated static string.
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}
