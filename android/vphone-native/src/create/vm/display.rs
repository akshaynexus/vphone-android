//! Whether a creation boot draws to a screen, per VM. The switch is on by default; a boot whose
//! display died turns it off for that VM with the reason recorded beside it.

use std::fs;
use std::path::Path;

/// The person turned the screen off for this VM.
pub const NO_SCREEN_FILE: &str = ".create-no-screen";
/// A boot with the display failed: the rest of this creation runs headless. Holds why.
pub const HEADLESS_FILE: &str = ".create-headless";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    On,
    Off,
    Headless(String),
}

pub fn setting(vm: &Path) -> Setting {
    if vm.join(NO_SCREEN_FILE).exists() {
        return Setting::Off;
    }
    match fs::read_to_string(vm.join(HEADLESS_FILE)) {
        Ok(reason) => Setting::Headless(reason.trim().to_string()),
        Err(_) => Setting::On,
    }
}

impl Setting {
    /// The `display` field of the vitals file.
    pub fn name(&self) -> &'static str {
        match self {
            Setting::On => "on",
            Setting::Off => "off",
            Setting::Headless(_) => "headless",
        }
    }
}

/// The per-VM switch. Turning it on also forgets an earlier failure, so the person can try again.
pub fn set_screen_on(vm: &Path, on: bool) -> std::io::Result<()> {
    if on {
        let _ = fs::remove_file(vm.join(NO_SCREEN_FILE));
        let _ = fs::remove_file(vm.join(HEADLESS_FILE));
        Ok(())
    } else {
        fs::write(vm.join(NO_SCREEN_FILE), b"")
    }
}

/// Later boots of this VM go headless, and why, in the file the UI shows.
pub fn turn_off_after_failure(vm: &Path, why: &str) {
    let _ = fs::write(vm.join(HEADLESS_FILE), why);
}

#[cfg(target_os = "android")]
mod jni_api {
    use super::*;
    use crate::paths::Paths;
    use jni::objects::{JObject, JString};
    use jni::sys::{jboolean, jstring, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;

    fn vm_dir(env: &mut JNIEnv, name: &JString) -> std::path::PathBuf {
        let name: String = env.get_string(name).map(String::from).unwrap_or_default();
        Paths::get().vms_internal().join(name)
    }

    /// The vitals JSON, or "" while no VM has run.
    #[no_mangle]
    pub extern "system" fn Java_com_vphone_android_CreateVmNative_vitals(
        env: JNIEnv,
        _this: JObject,
    ) -> jstring {
        let text = fs::read_to_string(super::super::vitals::stats_path(Paths::get())).unwrap_or_default();
        env.new_string(text).map_or(std::ptr::null_mut(), |s| s.into_raw())
    }

    /// The VM's screen setting: "on", "off", or the reason it was turned off after a failure.
    #[no_mangle]
    pub extern "system" fn Java_com_vphone_android_CreateVmNative_screenSetting(
        mut env: JNIEnv,
        _this: JObject,
        name: JString,
    ) -> jstring {
        let vm = vm_dir(&mut env, &name);
        let text = match setting(&vm) {
            Setting::On => "on".to_string(),
            Setting::Off => "off".to_string(),
            Setting::Headless(reason) => format!("headless:{reason}"),
        };
        env.new_string(text).map_or(std::ptr::null_mut(), |s| s.into_raw())
    }

    #[no_mangle]
    pub extern "system" fn Java_com_vphone_android_CreateVmNative_setScreenOn(
        mut env: JNIEnv,
        _this: JObject,
        name: JString,
        on: jboolean,
    ) -> jboolean {
        let vm = vm_dir(&mut env, &name);
        if set_screen_on(&vm, on == JNI_TRUE).is_ok() {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures/vm-display")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_screen_is_on_by_default_and_the_switch_wins_over_a_failure() {
        let d = dir("switch");
        assert_eq!(setting(&d), Setting::On);
        set_screen_on(&d, false).unwrap();
        assert_eq!(setting(&d), Setting::Off);
        turn_off_after_failure(&d, "the GPU failed");
        assert_eq!(setting(&d), Setting::Off, "the person's choice first");
        set_screen_on(&d, true).unwrap();
        assert_eq!(setting(&d), Setting::On, "turning it on forgets the failure");
    }

    #[test]
    fn a_failure_keeps_its_reason() {
        let d = dir("reason");
        turn_off_after_failure(&d, "Vulkan did not load\n");
        assert_eq!(setting(&d), Setting::Headless("Vulkan did not load".into()));
        assert_eq!(setting(&d).name(), "headless");
    }
}
