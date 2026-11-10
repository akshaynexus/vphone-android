//! Reads the GPU driver straight from a cloudOS IPSW, as the app's `gpu-driver` step does.
//!
//!   cargo run --release --example gpu_driver -- <cloudOS.ipsw> <out dir> [threads]
//!
//! Prints the driver's SHA-1 and how long each part took.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use vphone_create_steps::progress::Progress;
use vphone_create_steps::{aea, steps};

struct Print(AtomicU64);

impl Progress for Print {
    fn log(&self, line: &str) {
        println!("{line}");
    }
    fn progress(&self, done: u64, total: u64, message: &str) {
        if total > 0 {
            let tenth = done * 10 / total;
            if self.0.swap(tenth, Ordering::Relaxed) != tenth {
                println!("  {message}: {}%", tenth * 10);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: gpu_driver <cloudOS.ipsw> <out dir> [threads]");
        std::process::exit(2);
    }
    let threads = args.get(3).and_then(|t| t.parse().ok()).unwrap_or_else(aea::default_threads);
    let progress = Print(AtomicU64::new(u64::MAX));
    match steps::gpu_driver_from_image(Path::new(&args[1]), Path::new(&args[2]), threads, &progress) {
        Ok(Some((driver, t))) => {
            println!(
                "driver {} sha1 {} known={} | threads {} decrypt {:.2}s extract {:.2}s image {} MB written {} MB",
                driver.bundle.display(),
                driver.sha1,
                driver.sha1 == steps::KNOWN_GPU_SHA1,
                t.threads,
                t.decrypt.as_secs_f64(),
                t.extract.as_secs_f64(),
                t.image_bytes >> 20,
                t.written_bytes >> 20
            );
        }
        Ok(None) => {
            println!("the image carries no key: only the temporary-VM path can read this cloudOS");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}
