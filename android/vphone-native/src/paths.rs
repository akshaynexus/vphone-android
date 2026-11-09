//! The app's directories, given once by Kotlin (which alone can ask Android for them).

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct Paths {
    /// `Context.getFilesDir()`: internal, short enough for Unix socket paths.
    pub files: PathBuf,
    /// `Context.getCacheDir()`.
    pub cache: PathBuf,
    /// `Context.getExternalFilesDir(null)`: `vms/` lives here.
    pub external: PathBuf,
    /// `ApplicationInfo.nativeLibraryDir`.
    pub native_lib: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();

impl Paths {
    pub fn set(paths: Paths) {
        let _ = PATHS.set(paths);
    }
    pub fn get() -> &'static Paths {
        PATHS.get().expect("Native.setup was not called")
    }
    pub fn vms(&self) -> PathBuf {
        self.external.join("vms")
    }
    /// Where a VM made on the phone lives: internal storage, a real ext4/f2fs with sparse files.
    pub fn vms_internal(&self) -> PathBuf {
        self.files.join("vms")
    }
    /// A VM's folder: internal storage first, then the importable external location.
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        let internal = self.vms_internal().join(name);
        if internal.is_dir() {
            internal
        } else {
            self.vms().join(name)
        }
    }
    /// Firmware files kept between creations.
    pub fn fw_cache(&self) -> PathBuf {
        self.files.join("fw-cache")
    }
    /// Short socket directory for the USB broker, usbmuxd and the VM's chardev.
    pub fn create_sockets(&self) -> PathBuf {
        self.files.join("cv")
    }
    pub fn vsock_socket(&self) -> PathBuf {
        self.files
            .join("vsock")
            .join(format!("{}.sock", crate::launcher::GUEST_CID))
    }
    pub fn qmp_socket(&self) -> PathBuf {
        self.files.join("qmp.sock")
    }
    pub fn last_run(&self) -> PathBuf {
        self.files.join("last-run.json")
    }
    pub fn lib(&self, name: &str) -> PathBuf {
        self.native_lib.join(name)
    }
}
