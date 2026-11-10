//! A clock that leaves out the time this process was not running.
//!
//! A paused creation stops its helpers (SIGSTOP) and its VM, for minutes or for hours. A deadline
//! measured on the system's clock keeps running through that, so the first check after the pause
//! finds every wait expired: the guest "did not acknowledge for 300 s" although it was given no
//! time at all. Deadlines in the helpers are measured on this clock instead.
//!
//! A thread ticks every `TICK`. Whatever the system clock advanced by between two ticks counts,
//! but never more than `GAP_LIMIT`: a process that was stopped comes back to one tick that is
//! minutes long, and only `GAP_LIMIT` of it is counted. A reader between ticks applies the same
//! limit to the time since the last one, so a deadline checked by the thread that wakes first
//! after a pause is already right. A ticker that the system keeps from running for longer than
//! the limit makes the clock run slow, which only lengthens a wait.
//!
//! A process that is not itself stopped while what it waits for is (the app's engine, waiting on
//! the paused VM) says so with [`hold`].

use std::ops::Add;
use std::sync::{Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(100);
/// The most one gap between ticks may count.
pub const GAP_LIMIT: Duration = Duration::from_secs(1);

struct Clock {
    /// Active time up to `last`.
    counted: Duration,
    last: Instant,
    held: bool,
}

impl Clock {
    fn pending(&self, now: Instant) -> Duration {
        if self.held {
            Duration::ZERO
        } else {
            now.saturating_duration_since(self.last).min(GAP_LIMIT)
        }
    }

    fn advance(&mut self, now: Instant) {
        self.counted += self.pending(now);
        self.last = now;
    }
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static TICKER: Once = Once::new();

fn with<T>(f: impl FnOnce(&mut Clock) -> T) -> T {
    TICKER.call_once(|| {
        *CLOCK.lock().unwrap() = Some(Clock { counted: Duration::ZERO, last: Instant::now(), held: false });
        // Without the thread the clock would still be monotonic, only slow: every read would
        // count at most `GAP_LIMIT`. Nothing here can do better, so the failure is not fatal.
        let _ = thread::Builder::new().name("active-clock".into()).spawn(|| loop {
            thread::sleep(TICK);
            if let Some(clock) = CLOCK.lock().unwrap().as_mut() {
                clock.advance(Instant::now());
            }
        });
    });
    f(CLOCK.lock().unwrap().as_mut().expect("set by the Once above"))
}

/// Stops (`true`) or resumes the clock: for a process that keeps running while the VM it waits on
/// is paused.
pub fn hold(on: bool) {
    with(|clock| {
        clock.advance(Instant::now());
        clock.held = on;
    });
}

/// A moment on the active clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActiveInstant(Duration);

impl ActiveInstant {
    pub fn now() -> ActiveInstant {
        with(|clock| ActiveInstant(clock.counted + clock.pending(Instant::now())))
    }

    /// Active time since this moment.
    pub fn elapsed(&self) -> Duration {
        ActiveInstant::now().0.saturating_sub(self.0)
    }

    /// Active time from now until this moment; zero once it has passed.
    pub fn remaining(&self) -> Duration {
        self.0.saturating_sub(ActiveInstant::now().0)
    }

    pub fn passed(&self) -> bool {
        self.remaining().is_zero()
    }
}

impl Add<Duration> for ActiveInstant {
    type Output = ActiveInstant;
    fn add(self, rhs: Duration) -> ActiveInstant {
        ActiveInstant(self.0.saturating_add(rhs))
    }
}

/// Sleeps for `duration` of active time: a sleep the process was stopped in is finished afterwards.
pub fn sleep(duration: Duration) {
    let end = ActiveInstant::now() + duration;
    loop {
        let left = end.remaining();
        if left.is_zero() {
            return;
        }
        thread::sleep(left);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_gap_counts_as_the_limit_and_a_held_clock_does_not_move() {
        let start = Instant::now();
        let mut clock = Clock { counted: Duration::ZERO, last: start, held: false };
        // Ticks a tenth of a second apart all count.
        for i in 1..=20 {
            clock.advance(start + TICK * i);
        }
        assert_eq!(clock.counted, Duration::from_secs(2));
        // Six minutes stopped: the tick that follows counts one limit.
        let resumed = start + Duration::from_secs(2 + 360);
        assert_eq!(clock.pending(resumed), GAP_LIMIT, "the first reader after the pause sees the limit");
        clock.advance(resumed);
        assert_eq!(clock.counted, Duration::from_secs(2) + GAP_LIMIT);
        // Held: nothing counts, however it is read.
        clock.held = true;
        assert_eq!(clock.pending(resumed + Duration::from_millis(300)), Duration::ZERO);
        clock.advance(resumed + Duration::from_millis(300));
        assert_eq!(clock.counted, Duration::from_secs(3));
        clock.held = false;
        clock.advance(resumed + Duration::from_millis(400));
        assert_eq!(clock.counted, Duration::from_millis(3100));
    }

    #[test]
    fn it_follows_the_system_clock_while_the_process_runs() {
        let (active, system) = (ActiveInstant::now(), Instant::now());
        thread::sleep(Duration::from_millis(450));
        let (a, s) = (active.elapsed(), system.elapsed());
        assert!(a <= s + Duration::from_millis(5), "{a:?} against {s:?}");
        assert!(a + Duration::from_millis(150) >= s, "{a:?} against {s:?}");
        assert!((active + Duration::from_secs(60)).remaining() > Duration::from_secs(58));
        assert!(active.passed());
    }
}
