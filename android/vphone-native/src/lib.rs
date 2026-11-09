//! The native half of the vphone Android app: everything that is not an Android framework call.
//! Kotlin holds the Activity, the Service and SAF, and calls into [`jni_api`].

pub mod bundle;
pub mod config;
pub mod create;
pub mod import;
pub mod launcher;
pub mod meminfo;
pub mod paths;
pub mod platform;
pub mod preflight;
pub mod qmp;
pub mod runtime;
pub mod vphoned;

#[cfg(target_os = "android")]
mod adpf;
#[cfg(target_os = "android")]
mod jni_api;
