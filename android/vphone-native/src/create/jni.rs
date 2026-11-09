//! JNI entry points for creation (`com.vphone.android.Native.create*`) and the calls back into
//! Kotlin for the VM process, which only the Android framework can start.

use super::vm::VmProcess;
use crate::paths::Paths;
use jni::objects::{GlobalRef, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};

fn text(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(String::from).unwrap_or_default()
}

fn out(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).map_or(std::ptr::null_mut(), |j| j.into_raw())
}

fn guarded(env: &mut JNIEnv, f: impl FnOnce(&mut JNIEnv) -> String) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| f(env)))
        .unwrap_or_else(|_| "internal error; see logcat".to_string());
    out(env, &result)
}

fn quiet(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

fn result_text(r: Result<(), String>) -> String {
    r.err().unwrap_or_default()
}

/// The class holding the callbacks, resolved on a Java thread where the app's loader is on hand.

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createCatalog(mut env: JNIEnv, _this: JObject) -> jstring {
    guarded(&mut env, |_| {
        serde_json::to_string(&super::compat::wizard_choices()).unwrap_or_default()
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createPreflight(
    mut env: JNIEnv,
    _this: JObject,
    spec: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let spec = text(env, &spec);
        super::preflight_json(Paths::get(), &spec)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createBegin(
    mut env: JNIEnv,
    _this: JObject,
    spec: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let spec = text(env, &spec);
        result_text(super::begin(Paths::get(), &spec))
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createPending(mut env: JNIEnv, _this: JObject) -> jstring {
    guarded(&mut env, |_| super::pending(Paths::get()).unwrap_or_default())
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createStatus(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        super::status_json(Paths::get(), &name)
    })
}

/// Blocks until the creation ends; the service runs it on its own thread.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createRun(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        result_text(super::run(Paths::get(), &name, Arc::new(super::vm::ExecVm::new(Paths::get()))))
    })
}


#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createCancel(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) {
    let name = text(&mut env, &name);
    quiet(|| super::request_cancel(Paths::get(), &name));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createPause(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
    paused: jni::sys::jboolean,
) {
    let name = text(&mut env, &name);
    quiet(|| super::request_pause(Paths::get(), &name, paused != 0));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createAllowCellular(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
    allow: jboolean,
) {
    let name = text(&mut env, &name);
    quiet(|| super::set_cellular(Paths::get(), &name, allow == JNI_TRUE));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createNetwork(
    _env: JNIEnv,
    _this: JObject,
    connected: jboolean,
    unmetered: jboolean,
) {
    quiet(|| super::ctl::set_network(connected == JNI_TRUE, unmetered == JNI_TRUE));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createThermal(_env: JNIEnv, _this: JObject, status: jint) {
    quiet(|| super::ctl::set_thermal(status));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createDelete(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
    also_firmware: jboolean,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        result_text(super::delete(Paths::get(), &name, also_firmware == JNI_TRUE))
    })
}

// ------------------------------------------------------------------ the storage screen

/// Measures everything, so called off the main thread.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_scan(mut env: JNIEnv, _this: JObject) -> jstring {
    guarded(&mut env, |_| super::storage::scan_json(Paths::get()))
}

/// What a confirmation shows for an action. Measures it, so called off the main thread.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_plan(
    mut env: JNIEnv,
    _this: JObject,
    op: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let op = text(env, &op);
        super::storage::plan_json(Paths::get(), &op)
    })
}

/// Starts an action on its own thread; an error is its text.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_start(
    mut env: JNIEnv,
    _this: JObject,
    op: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let op = text(env, &op);
        result_text(super::storage::start(Paths::get(), &op))
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_progress(mut env: JNIEnv, _this: JObject) -> jstring {
    guarded(&mut env, |_| super::storage::progress_json())
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_takeResult(_env: JNIEnv, _this: JObject) {
    quiet(super::storage::take_result);
}

/// Goes up whenever an action ends, so a list on screen knows to read again.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_StorageNative_generation(_env: JNIEnv, _this: JObject) -> jlong {
    catch_unwind(super::storage::generation).unwrap_or(0) as jlong
}

// ------------------------------------------------------------------ the log screen

/// The progress screen's panels; reads the step's log, so called off the main thread.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLive(
    mut env: JNIEnv,
    _this: JObject,
    name: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let name = text(env, &name);
        super::live_json(Paths::get(), &name)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogOpen(
    mut env: JNIEnv,
    _this: JObject,
    path: JString,
) -> jlong {
    let path = text(&mut env, &path);
    catch_unwind(AssertUnwindSafe(|| super::logview::open_view(&path))).unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogClose(_env: JNIEnv, _this: JObject, view: jlong) {
    quiet(|| super::logview::close_view(view));
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogSync(
    mut env: JNIEnv,
    _this: JObject,
    view: jlong,
    run_state: JString,
    paused: jboolean,
) -> jstring {
    guarded(&mut env, |env| {
        let run_state = text(env, &run_state);
        super::logview::sync_view(view, &run_state, paused == JNI_TRUE)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogRows(
    mut env: JNIEnv,
    _this: JObject,
    view: jlong,
    first: jint,
    count: jint,
) -> jstring {
    guarded(&mut env, |_| {
        super::logview::with_view(view, |v| v.rows_json(first.max(0) as u32, count.max(0) as u32))
            .unwrap_or_else(|| "[]".to_string())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogFilter(
    mut env: JNIEnv,
    _this: JObject,
    view: jlong,
    filter: jint,
    search: JString,
) {
    let search = text(&mut env, &search);
    quiet(|| {
        super::logview::with_view(view, |v| v.set_filter(filter, &search));
    });
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogToggle(
    _env: JNIEnv,
    _this: JObject,
    view: jlong,
    stage: jint,
) {
    quiet(|| {
        super::logview::with_view(view, |v| v.toggle(stage.max(0) as u32));
    });
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogOpenAll(
    _env: JNIEnv,
    _this: JObject,
    view: jlong,
    open: jboolean,
) {
    quiet(|| {
        super::logview::with_view(view, |v| v.open_all(open == JNI_TRUE));
    });
}

/// A stage's lines, or the whole view for a stage of -1.
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogCopy(
    mut env: JNIEnv,
    _this: JObject,
    view: jlong,
    stage: jint,
) -> jstring {
    guarded(&mut env, |_| {
        super::logview::with_view(view, |v| v.copy_text(stage)).unwrap_or_default()
    })
}

/// Writes the current view to a file for sharing: a one-line summary, or "error: ...".
#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createLogExport(
    mut env: JNIEnv,
    _this: JObject,
    view: jlong,
    dest: JString,
) -> jstring {
    guarded(&mut env, |env| {
        let dest = text(env, &dest);
        super::logview::with_view(view, |v| match v.export(std::path::Path::new(&dest)) {
            Ok(summary) => summary,
            Err(e) => format!("error: {e}"),
        })
        .unwrap_or_else(|| "error: the log is closed".to_string())
    })
}
