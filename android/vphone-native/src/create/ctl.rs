//! What the phone tells a running creation: cancel, network, thermal. Set from the Service's
//! callbacks in the `:create` process; read by the step in progress.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);
static NET_CONNECTED: AtomicBool = AtomicBool::new(true);
static NET_UNMETERED: AtomicBool = AtomicBool::new(true);
static CELLULAR_OK: AtomicBool = AtomicBool::new(false);
static THERMAL: AtomicI32 = AtomicI32::new(0);

pub fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}
pub fn clear_cancel() {
    CANCEL.store(false, Ordering::SeqCst);
}
pub fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

pub fn set_network(connected: bool, unmetered: bool) {
    NET_CONNECTED.store(connected, Ordering::SeqCst);
    NET_UNMETERED.store(unmetered, Ordering::SeqCst);
}
pub fn set_cellular_ok(ok: bool) {
    CELLULAR_OK.store(ok, Ordering::SeqCst);
}

/// Why a download may not proceed now; `None` when it may.
pub fn network_block() -> Option<&'static str> {
    if !NET_CONNECTED.load(Ordering::SeqCst) {
        Some("Waiting for a network connection")
    } else if !NET_UNMETERED.load(Ordering::SeqCst) && !CELLULAR_OK.load(Ordering::SeqCst) {
        Some("Waiting for Wi-Fi (cellular downloads need your confirmation)")
    } else {
        None
    }
}

/// Android's `PowerManager.THERMAL_STATUS_*` (0 none … 6 shutdown).
pub fn set_thermal(status: i32) {
    THERMAL.store(status, Ordering::SeqCst);
}
pub fn thermal() -> i32 {
    THERMAL.load(Ordering::SeqCst)
}

pub const THERMAL_MODERATE: i32 = 2;
pub const THERMAL_SEVERE: i32 = 3;
pub const THERMAL_CRITICAL: i32 = 4;

pub fn thermal_text(status: i32) -> &'static str {
    match status {
        3 => "The phone is hot; this will be slow",
        4 => "The phone is very hot; paused until it cools down",
        5 | 6 => "The phone is overheating; stop using it and let it cool",
        _ => "",
    }
}
