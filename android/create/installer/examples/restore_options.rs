//! Makes sure a VM's restore ramdisk turns the fast mtree path on, as the app does before a
//! restore, and says what it found.
//!
//!   cargo run --release --example restore_options -- <vm directory>

use std::path::Path;
use vphone_create_steps::progress::Progress;
use vphone_create_steps::restore_options;

struct Print;

impl Progress for Print {
    fn log(&self, line: &str) {
        println!("{line}");
    }
    fn progress(&self, _: u64, _: u64, _: &str) {}
}

fn main() {
    let Some(vm) = std::env::args().nth(1) else {
        eprintln!("usage: restore_options <vm directory>");
        std::process::exit(2);
    };
    match restore_options::ensure_fast_mtree(Path::new(&vm), &Print) {
        Ok(state) => println!("{}", state.describe()),
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}
