//! JNI entry points for creation (`com.vphone.android.Native.create*`) and the calls back into
//! Kotlin for the VM process, which only the Android framework can start.

use super::vm::VmProcess;
use crate::paths::Paths;
use jni::objects::{GlobalRef, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jstring, JNI_TRUE};
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
struct Bridge {
    vm: JavaVM,
    class: GlobalRef,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

fn bridge(env: &mut JNIEnv) -> Result<&'static Bridge, String> {
    if let Some(b) = BRIDGE.get() {
        return Ok(b);
    }
    let vm = env.get_java_vm().map_err(|e| e.to_string())?;
    let class = env
        .find_class("com/vphone/android/CreateBridge")
        .map_err(|e| e.to_string())?;
    let class = env.new_global_ref(class).map_err(|e| e.to_string())?;
    Ok(BRIDGE.get_or_init(|| Bridge { vm, class }))
}

struct JniProcess;

impl JniProcess {
    fn call<T>(
        f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        let b = BRIDGE.get().ok_or("the bridge is not initialized")?;
        let mut env = b.vm.attach_current_thread().map_err(|e| e.to_string())?;
        let class: &JClass = b.class.as_obj().into();
        f(&mut env, class).map_err(|e| e.to_string())
    }
}

impl VmProcess for JniProcess {
    fn start(&self) -> Result<(), String> {
        let message = Self::call(|env, class| {
            let v = env.call_static_method(class, "startVm", "()Ljava/lang/String;", &[])?.l()?;
            env.get_string(&JString::from(v)).map(String::from)
        })?;
        if message.is_empty() {
            Ok(())
        } else {
            Err(message)
        }
    }

    fn release(&self) {
        let _ = Self::call(|env, class| {
            env.call_static_method(class, "releaseVm", "()V", &[]).map(|_| ())
        });
    }

    fn exit_reason(&self) -> String {
        Self::call(|env, class| {
            let v = env
                .call_static_method(class, "vmExitReason", "()Ljava/lang/String;", &[])?
                .l()?;
            env.get_string(&JString::from(v)).map(String::from)
        })
        .unwrap_or_default()
    }
}

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
        if let Err(e) = bridge(env) {
            return e;
        }
        result_text(super::run(Paths::get(), &name, Arc::new(JniProcess)))
    })
}

#[no_mangle]
pub extern "system" fn Java_com_vphone_android_Native_createVmRun(mut env: JNIEnv, _this: JObject) -> jstring {
    guarded(&mut env, |_| result_text(super::vm::run_from_spec(Paths::get())))
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

