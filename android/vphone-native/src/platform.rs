//! Logging: to logcat on Android, to stderr elsewhere.

#[cfg(target_os = "android")]
mod imp {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    const ANDROID_LOG_INFO: c_int = 4;

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn log(tag: &str, line: &str) {
        let (Ok(tag), Ok(line)) = (CString::new(tag), CString::new(line.replace('\0', " "))) else {
            return;
        };
        // SAFETY: both are valid NUL-terminated strings for the call's duration.
        unsafe { __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), line.as_ptr()) };
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    pub fn log(tag: &str, line: &str) {
        eprintln!("{tag}: {line}");
    }
}

pub use imp::log;
