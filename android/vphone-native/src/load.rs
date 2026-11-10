//! How busy the guest's CPUs are on average under the settings a start runs with, kept from one
//! start to the next so that the log shows a change's effect: the `guest top` lines' own number,
//! averaged, beside the average of the last start that ran with other settings.
//!
//! A start's settings are the ones in force in it, not the ones just saved: the services a start
//! before it applied, the preferences found in the guest, the kernel's arguments. The first
//! [`SETTLE`] after the unlock are left out, as iOS has work of its own then.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const FILE: &str = "guest-load.json";
/// How long after the unlock the samples start to count.
pub const SETTLE: Duration = Duration::from_secs(180);
/// A start with fewer samples than this is not kept as the one to compare with.
const ENOUGH: u32 = 4;

/// One start's average.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    /// The settings in force, in words.
    pub tuning: String,
    /// Guest CPUs busy, averaged over `samples` of the `guest top` lines.
    pub busy: f64,
    pub samples: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    last: Option<Run>,
    /// The last start that ran with other settings than `last`.
    before: Option<Run>,
}

#[derive(Debug)]
pub struct Meter {
    tuning: String,
    sum: f64,
    samples: u32,
    before: Option<Run>,
}

impl Meter {
    /// For a start that runs with `tuning`: what to compare with is the last start if it ran
    /// with other settings for long enough, else what that start was compared with.
    pub fn start(vm: &Path, tuning: String) -> Meter {
        let record: Record = std::fs::read(vm.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let other = |run: &Run| run.tuning != tuning && run.samples >= ENOUGH;
        let before = record.last.filter(other).or(record.before.filter(other));
        Meter { tuning, sum: 0.0, samples: 0, before }
    }

    /// One `guest top` line's number, and the record with it.
    pub fn add(&mut self, vm: &Path, busy: f64) {
        self.sum += busy;
        self.samples += 1;
        let record = Record { last: Some(self.run()), before: self.before.clone() };
        if let Ok(json) = serde_json::to_vec_pretty(&record) {
            let _ = std::fs::write(vm.join(FILE), json);
        }
    }

    fn run(&self) -> Run {
        Run { tuning: self.tuning.clone(), busy: self.sum / f64::from(self.samples.max(1)), samples: self.samples }
    }

    /// `guest load: this start runs with ...; the last start with other settings (...) averaged
    /// 5.62 guest CPUs busy over 40 lines`
    pub fn opening(&self) -> String {
        let before = match &self.before {
            Some(run) => format!("the last start with other settings ({}) averaged {:.2} guest CPUs busy over {} lines", run.tuning, run.busy, run.samples),
            None => "no earlier start with other settings to compare with".to_string(),
        };
        format!("guest load: this start runs with {}; {before}; its own average counts from {} min after the unlock", self.tuning, SETTLE.as_secs() / 60)
    }

    /// For the end of a `guest top` line: `; mean 4.31 over 12 lines; 5.62 over 40 before (...)`.
    pub fn words(&self) -> String {
        if self.samples == 0 {
            return String::new();
        }
        let mut out = format!("; mean {:.2} over {} lines", self.run().busy, self.samples);
        if let Some(run) = &self.before {
            out += &format!("; {:.2} over {} before ({})", run.busy, run.samples, run.tuning);
        }
        out
    }
}

/// The settings a start runs with, in words: the services a start before it applied, the two
/// groups of preferences as they were found in the guest, and the kernel's arguments.
pub fn tuning(services: &str, visuals: bool, launch_limits: bool, verbose_kernel: bool) -> String {
    format!(
        "{services}, {} visuals, {} launch limits, {} kernel log",
        if visuals { "lighter" } else { "stock" },
        if launch_limits { "longer" } else { "stock" },
        if verbose_kernel { "verbose" } else { "quiet" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_is_compared_with_the_last_one_that_ran_with_other_settings() {
        let dir = std::env::temp_dir().join(format!("vphone-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stock = tuning("services as they are", false, false, true);
        assert_eq!(stock, "services as they are, stock visuals, stock launch limits, verbose kernel log");
        let lighter = tuning("services trimmed and 4 more", true, true, false);

        let mut first = Meter::start(&dir, stock.clone());
        assert_eq!(first.words(), "");
        assert!(first.opening().contains("no earlier start with other settings"), "{}", first.opening());
        for busy in [5.5, 5.7, 5.6, 5.6] {
            first.add(&dir, busy);
        }
        assert_eq!(first.words(), "; mean 5.60 over 4 lines");

        let mut second = Meter::start(&dir, lighter.clone());
        assert!(second.opening().contains(&format!("the last start with other settings ({stock}) averaged 5.60 guest CPUs busy over 4 lines")));
        second.add(&dir, 4.0);
        second.add(&dir, 4.5);
        assert_eq!(second.words(), format!("; mean 4.25 over 2 lines; 5.60 over 4 before ({stock})"));

        // The same settings again: still compared with the stock start, not with itself. And
        // a start too short to count (two lines) is not what the stock settings are compared with.
        let third = Meter::start(&dir, lighter.clone());
        assert_eq!(third.before.as_ref().map(|r| r.tuning.as_str()), Some(stock.as_str()));
        let back = Meter::start(&dir, stock);
        assert_eq!(back.before, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
