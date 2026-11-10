//! The JNI entry points behind `com.vphone.android.Native`.

use crate::bundle;
use crate::config::{self, VmConfig};
use crate::import;
use crate::meminfo::MemInfo;
use crate::paths::Paths;
use crate::runtime;
use jni::objects::{JObject, JString};
use jni::sys::{jfloat, jint, jintArray, jlong, jstring};
use jni::JNIEnv;
use serde_json::json;
use std::fs::File;
use std::os::fd::FromRawFd;
use std::os::raw::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[link(name = "android")]
extern "C" {
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
}

fn text(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(String::from).unwrap_or_default()
}

fn out(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map_or(std::ptr::null_mut(), |j| j.into_raw())
}

/// A panic must not cross into the JVM: it becomes the returned text.
fn guarded(env: &mut JNIEnv, f: impl FnOnce(&mut JNIEnv) -> String) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| f(env)));
    let s = result.unwrap_or_else(|_| json!({"error": "internal error; see logcat"}).to_string());
    out(env, &s)
}

fn quiet(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

fn result_text(r: Result<(), String>) -> String {
    r.err().unwrap_or_default()
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_setup(
    mut env: JNIEnv,
    _this: JObject,
    files: JString,
    cache: JString,
    external: JString,
    native_lib: JString,
) {
    let paths = Paths {
        files: text(&mut env, &files).into(),
        cache: text(&mut env, &cache).into(),
        external: text(&mut env, &external).into(),
        native_lib: text(&mut env, &native_lib).into(),
    };
    quiet(|| {
        std::panic::set_hook(Box::new(|info| {
            crate::platform::log("vphone-panic", &info.to_string())
        }));
        let _ = std::fs::create_dir_all(paths.vms());
        let _ = std::fs::create_dir_all(paths.vms_internal());
        // Only the main process: the creation processes share this Application.
        let main_process = std::fs::read("/proc/self/cmdline").is_ok_and(|c| !c.contains(&b':'));
        if main_process {
            import::remove_stale(&paths.vms());
        }
        Paths::set(paths);
    });
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_listVms(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| {
        let p = Paths::get();
        serde_json::to_string(&bundle::list_in(&[p.vms_internal(), p.vms()])).unwrap_or_default()
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_deviceInfo(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| {
        let mem = MemInfo::read();
        json!({
            "totalMib": mem.total_mib,
            "availableMib": mem.available_mib,
            "cpus": config::online_cpus(),
            "defaults": VmConfig::default(),
        })
        .to_string()
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_saveVmConfig(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
    config_json: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        let json = text(env, &config_json);
        match serde_json::from_str::<VmConfig>(&json) {
            Ok(cfg) => result_text(cfg.save(&Paths::get().vm_dir(&name))),
            Err(e) => e.to_string(),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_preflight(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        serde_json::to_string(&runtime::preflight(&name)).unwrap_or_default()
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_boot(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        result_text(runtime::boot(&name))
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_state(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| {
        serde_json::to_string(&runtime::state()).unwrap_or_default()
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_takeNotice(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| runtime::take_notice())
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_stop(_env: JNIEnv, _this: JObject) {
    quiet(runtime::stop);
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_lastRun(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| {
        runtime::last_run().map_or_else(String::new, |r| {
            serde_json::to_string(&r).unwrap_or_default()
        })
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_recordDeath(
    mut env: JNIEnv,
    _this: JObject,
    reason: JString,
) {
    let reason = text(&mut env, &reason);
    quiet(|| runtime::record_death(&reason));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_logLine(
    mut env: JNIEnv,
    _this: JObject,
    line: JString,
) {
    let line = text(&mut env, &line);
    quiet(|| runtime::log_line(&line));
}

/// The active network's DNS servers, as Android gives them, for the VM's NAT (`crate::net`).
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_setDnsServers(
    mut env: JNIEnv,
    _this: JObject,
    servers: JString,
) {
    let servers = text(&mut env, &servers);
    quiet(|| {
        if let Some(line) = crate::net::set_dns_servers(Paths::get(), &servers) {
            runtime::log_line(&line);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_logTail(
    mut env: JNIEnv,
    _this: JObject,
    path: JString,
    bytes: jlong,
) -> jstring {
    guarded(&mut env, |env| {
        let path = text(env, &path);
        runtime::log_tail(&path, bytes.max(0) as u64)
    })
}

/// The Surface for the guest's screen, or null when it is gone.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_setSurface(
    env: JNIEnv,
    _this: JObject,
    surface: JObject,
) {
    quiet(|| {
        let window = if surface.is_null() {
            std::ptr::null_mut()
        } else {
            // SAFETY: env and surface are the live JNI references of this call.
            unsafe { ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast()) }
        };
        let old = runtime::swap_window(window as usize);
        if old != 0 {
            // SAFETY: swap_window handed back the reference this function acquired earlier.
            unsafe { ANativeWindow_release(old as *mut c_void) };
        }
    });
}

/// The image's `[left, top, width, height]` in a container; touches then come in that view's size.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_layoutImage(
    env: JNIEnv,
    _this: JObject,
    w: jint,
    h: jint,
) -> jintArray {
    let s = runtime::state();
    let rect = runtime::fit(w, h, s.width, s.height);
    runtime::set_view(rect[2], rect[3]);
    match env.new_int_array(4) {
        Ok(array) => {
            let _ = env.set_int_array_region(&array, 0, &rect);
            array.into_raw()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_touch(
    _env: JNIEnv,
    _this: JObject,
    action: jint,
    x: jfloat,
    y: jfloat,
    event_ns: jni::sys::jlong,
) {
    quiet(|| runtime::touch(action, x, y, event_ns));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_home(_env: JNIEnv, _this: JObject) {
    quiet(runtime::home);
}

/// Whether iOS is playing a sound now.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_audioPlaying(_env: JNIEnv, _this: JObject) -> jni::sys::jboolean {
    std::panic::catch_unwind(runtime::audio_playing).unwrap_or(false) as jni::sys::jboolean
}

/// Silences what the phone plays of the VM, or lets it be heard again.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_setAudioMuted(_env: JNIEnv, _this: JObject, muted: jni::sys::jboolean) {
    quiet(|| runtime::set_audio_muted(muted != 0));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_lock(_env: JNIEnv, _this: JObject) {
    quiet(runtime::lock);
}

/// Starts an import; `{"name": ...}` or `{"error": ...}`.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importBegin(
    mut env: JNIEnv,
    _this: JObject,
    folder: JString,
    total: jlong,
) -> jstring {
    guarded(&mut env, |env| {
        let folder = text(env, &folder);
        match import::begin(&Paths::get().vms(), &folder, total.max(0) as u64) {
            Ok(name) => json!({"name": name}).to_string(),
            Err(e) => json!({"error": e}).to_string(),
        }
    })
}

/// Copies one file, taking ownership of `fd`. Blocks until done; an error's text, or "".
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importFile(
    mut env: JNIEnv,
    _this: JObject,
    fd: jint,
    rel: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let rel = text(env, &rel);
        // SAFETY: Kotlin detached this descriptor from its ParcelFileDescriptor and gave it up.
        let file = unsafe { File::from_raw_fd(fd) };
        result_text(import::file(file, &rel))
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importFinish(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| result_text(import::finish()))
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importAbort(_env: JNIEnv, _this: JObject) {
    quiet(import::abort);
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importCancel(_env: JNIEnv, _this: JObject) {
    quiet(import::cancel);
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_importProgress(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| {
        serde_json::to_string(&import::progress()).unwrap_or_default()
    })
}

// ------------------------------------------------------------------- sending a VM (share.rs)

/// What sending `name` would hold: `{"files": n, "dataBytes": n}` or `{"error": ...}`.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareEstimate(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        match crate::share::estimate(&Paths::get().vm_dir(&name)) {
            Ok((files, data)) => json!({"files": files.len(), "dataBytes": data}).to_string(),
            Err(e) => json!({"error": e}).to_string(),
        }
    })
}

/// Writes VM `name` as a `.vphone` file to `fd`, taking ownership of it. Blocks until done:
/// `{"bytes": n}` or `{"error": ...}`.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareExport(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
    fd: jint,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        // SAFETY: Kotlin detached this descriptor from its ParcelFileDescriptor and gave it up.
        let out = unsafe { File::from_raw_fd(fd) };
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).saturating_sub(2).clamp(1, 6) as u32;
        match crate::share::export(&Paths::get().vm_dir(&name), &name, out, threads) {
            Ok(bytes) => json!({"bytes": bytes}).to_string(),
            Err(e) => json!({"error": e}).to_string(),
        }
    })
}

/// Imports a `.vphone` file from `fd`, taking ownership of it. Blocks until done:
/// `{"name": ...}` or `{"error": ...}`.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareImport(
    mut env: JNIEnv,
    _this: JObject,
    fd: jint,
) -> jstring {
    guarded(&mut env, |_| {
        // SAFETY: as in shareExport.
        let src = unsafe { File::from_raw_fd(fd) };
        let vms = Paths::get().vms();
        let _ = std::fs::create_dir_all(&vms);
        let free = crate::create::engine::free_bytes(&vms);
        match crate::share::import(&vms, src, free) {
            Ok(name) => json!({"name": name}).to_string(),
            Err(e) => json!({"error": e}).to_string(),
        }
    })
}

/// The path a Share export of `name` goes to in `dir`, after clearing older ones there.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_sharePath(
    mut env: JNIEnv,
    _this: JObject,
    dir: JString,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let dir = std::path::PathBuf::from(text(env, &dir));
        let name = text(env, &name);
        let _ = std::fs::create_dir_all(&dir);
        crate::share::share_path(&dir, &name).to_string_lossy().into_owned()
    })
}

/// Deletes exports in `dir` older than a day, unless one is being written.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareCleanup(
    mut env: JNIEnv,
    _this: JObject,
    dir: JString,
) {
    let dir = text(&mut env, &dir);
    quiet(|| crate::share::remove_old_exports(std::path::Path::new(&dir), 24 * 3600));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareCancel(_env: JNIEnv, _this: JObject) {
    quiet(crate::share::cancel);
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_shareProgress(
    mut env: JNIEnv,
    _this: JObject,
) -> jstring {
    guarded(&mut env, |_| serde_json::to_string(&crate::share::progress()).unwrap_or_default())
}
