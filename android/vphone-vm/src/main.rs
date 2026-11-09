//! The VM process: an ordinary executable the creation service starts for each boot, shipped as
//! `libvphone_vm.so` because Android only runs programs from the native library directory. It
//! loads the app's library and hands over the boot's spec file.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

const RTLD_NOW: c_int = 2;
const M_BIONIC_SET_HEAP_TAGGING_LEVEL: c_int = -204;
const M_HEAP_TAGGING_LEVEL_NONE: c_int = 0;

extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn mallopt(option: c_int, value: c_int) -> c_int;
}

fn dl_error() -> String {
    // SAFETY: dlerror returns null or a valid C string.
    unsafe {
        let e = dlerror();
        if e.is_null() { "unknown".into() } else { CStr::from_ptr(e).to_string_lossy().into_owned() }
    }
}

fn main() {
    let Some(spec) = std::env::args().nth(1) else {
        eprintln!("usage: libvphone_vm.so <spec file>");
        std::process::exit(2);
    };
    // SAFETY: called before any other thread exists. QEMU's generated code uses whole 64-bit host
    // pointers, which tagged heap pointers would break.
    unsafe { mallopt(M_BIONIC_SET_HEAP_TAGGING_LEVEL, M_HEAP_TAGGING_LEVEL_NONE) };
    // By full path: a dlopen by bare name is not guaranteed to search LD_LIBRARY_PATH.
    let dir = std::env::var("LD_LIBRARY_PATH")
        .ok()
        .and_then(|p| p.split(':').next().map(str::to_string))
        .filter(|p| !p.is_empty())
        .or_else(|| {
            let exe = std::fs::read_link("/proc/self/exe").ok()?;
            Some(exe.parent()?.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());
    let lib = CString::new(format!("{dir}/libvphone_native.so")).unwrap();
    // SAFETY: a valid path; the library stays loaded for the life of the process.
    let handle = unsafe { dlopen(lib.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        eprintln!("cannot load libvphone_native.so: {}", dl_error());
        std::process::exit(3);
    }
    let symbol = CString::new("vphone_vm_main").unwrap();
    // SAFETY: a valid handle and name.
    let main_fn = unsafe { dlsym(handle, symbol.as_ptr()) };
    if main_fn.is_null() {
        eprintln!("libvphone_native.so has no vphone_vm_main: {}", dl_error());
        std::process::exit(4);
    }
    let spec = CString::new(spec).unwrap();
    // SAFETY: the symbol is `extern "C" fn(*const c_char) -> c_int` in the app's library.
    let run: extern "C" fn(*const c_char) -> c_int = unsafe { std::mem::transmute(main_fn) };
    std::process::exit(run(spec.as_ptr()));
}
