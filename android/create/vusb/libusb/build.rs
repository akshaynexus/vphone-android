fn main() {
    // The DT_SONAME consumers link against: libusb-1.0.so on Android (DT_NEEDED is resolved inside
    // nativeLibraryDir by exactly that name), libusb-1.0.so.0 on a host.
    let soname = std::env::var("VUSB_LIBUSB_SONAME").unwrap_or_else(|_| "libusb-1.0.so.0".into());
    println!("cargo:rerun-if-env-changed=VUSB_LIBUSB_SONAME");
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,{soname}");
}
