//! How often the guest is told where the finger is.
//!
//! Android reads a touchscreen two to four times a display refresh and hands the app every
//! sample. Until 0.6.1 each one went to the guest as a report of its USB touchscreen, and every
//! report is work for the guest's HID driver, backboardd and the app under the finger, on vCPUs
//! that were already full (the 0.6.0 log: backboardd at 0.5 to 1.45 guest CPUs). An iPhone's own
//! screen reports once a refresh. So a move goes on at most once per [`Pacer::interval_ns`]: one
//! that comes sooner is held, replaced by any newer one, and sent when the interval is over, so
//! the guest always ends at the finger's last place. A press and a release go at once, exactly
//! where they happened; a release takes the place of a held move, since it carries the newer
//! position.

/// What the finger did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Down,
    Up,
    Move,
}

/// A position in guest pixels and when Android read it (`CLOCK_MONOTONIC`, ns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub x: u16,
    pub y: u16,
    pub event_ns: i64,
}

/// What came in and what went on since the last [`Pacer::take_counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub presses: u64,
    pub moves: u64,
    pub moves_sent: u64,
}

impl Counts {
    /// `214 touch events: 6 presses and releases, 208 moves of which 131 went to the guest`.
    pub fn words(&self) -> String {
        format!(
            "{} touch events: {} presses and releases, {} moves of which {} went to the guest (at most one a refresh)",
            self.presses + self.moves,
            self.presses,
            self.moves,
            self.moves_sent
        )
    }
}

#[derive(Debug)]
pub struct Pacer {
    pub interval_ns: i64,
    last_sent_ns: Option<i64>,
    held: Option<Sample>,
    counts: Counts,
}

/// Seven eighths of a refresh: Android's batches come a refresh apart give or take a little, and
/// one that is a little early should go now rather than wait for a timer.
pub fn interval_ns(refresh_hz: u32) -> i64 {
    1_000_000_000 / i64::from(refresh_hz.clamp(30, 240)) * 7 / 8
}

impl Pacer {
    pub const fn new(interval_ns: i64) -> Pacer {
        Pacer { interval_ns, last_sent_ns: None, held: None, counts: Counts { presses: 0, moves: 0, moves_sent: 0 } }
    }

    /// An event at `now_ns`: true when it goes to the guest now, false when it is held (a move
    /// inside the interval), to be asked for with [`Pacer::due`] from [`Pacer::due_at`] on.
    pub fn offer(&mut self, kind: Kind, sample: Sample, now_ns: i64) -> bool {
        match kind {
            Kind::Down | Kind::Up => {
                self.counts.presses += 1;
                // The release is where the finger is now; a press starts a new touch.
                self.held = None;
                self.last_sent_ns = Some(now_ns);
                true
            }
            Kind::Move => {
                self.counts.moves += 1;
                if self.last_sent_ns.is_some_and(|last| now_ns - last < self.interval_ns) {
                    self.held = Some(sample);
                    false
                } else {
                    self.held = None;
                    self.last_sent_ns = Some(now_ns);
                    self.counts.moves_sent += 1;
                    true
                }
            }
        }
    }

    /// When the held move is to go; `None` with nothing held.
    pub fn due_at(&self) -> Option<i64> {
        self.held.map(|_| self.last_sent_ns.unwrap_or(0) + self.interval_ns)
    }

    /// The held move, once its time has come: the caller sends it.
    pub fn due(&mut self, now_ns: i64) -> Option<Sample> {
        if self.due_at().is_some_and(|at| now_ns >= at) {
            self.last_sent_ns = Some(now_ns);
            self.counts.moves_sent += 1;
            return self.held.take();
        }
        None
    }

    pub fn take_counts(&mut self) -> Counts {
        std::mem::take(&mut self.counts)
    }
}

/// The number an event carries to QEMU above its button bits (`VPHONE_TOUCH_TAG_SHIFT`,
/// qemu/include/qemu/vphone-touch.h): 1 to 2^24-1, round and round, never 0.
pub const TAG_SHIFT: u32 = 8;
pub fn next_tag(tag: u32) -> u32 {
    if tag >= (1 << 24) - 1 {
        1
    } else {
        tag + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn at(x: u16, t_ms: i64) -> Sample {
        Sample { x, y: 0, event_ns: t_ms * MS }
    }

    #[test]
    fn a_fast_touchscreen_reaches_the_guest_once_a_refresh() {
        // 120 Hz: 7.29 ms. A 480 Hz panel's samples, 2 ms apart for 100 ms.
        let mut p = Pacer::new(interval_ns(120));
        assert_eq!(p.interval_ns, 7_291_666);
        assert!(p.offer(Kind::Down, at(0, 0), 0));
        let mut sent = Vec::new();
        for i in 1..=50i64 {
            let now = i * 2 * MS;
            // The timer's turn first, as the flushing thread would have had it.
            if let Some(s) = p.due(now) {
                sent.push(s.x);
            }
            if p.offer(Kind::Move, at(i as u16, i * 2), now) {
                sent.push(i as u16);
            }
        }
        // 100 ms of moves at one per 8 ms (the samples are 2 ms apart): 12 or 13, in order.
        assert!((12..=13).contains(&sent.len()), "{sent:?}");
        assert!(sent.windows(2).all(|w| w[0] < w[1]), "{sent:?}");
        let c = p.take_counts();
        assert_eq!((c.presses, c.moves, c.moves_sent), (1, 50, sent.len() as u64));
        assert_eq!(p.take_counts(), Counts::default());
    }

    #[test]
    fn the_finger_s_last_place_always_arrives() {
        let mut p = Pacer::new(8 * MS);
        assert!(p.offer(Kind::Down, at(1, 0), 0));
        assert!(!p.offer(Kind::Move, at(2, 1), MS));
        assert!(!p.offer(Kind::Move, at(3, 2), 2 * MS), "a newer move replaces the held one");
        assert_eq!(p.due_at(), Some(8 * MS));
        assert_eq!(p.due(7 * MS), None);
        // The finger stops: nothing more comes from Android, and the timer sends the last place.
        assert_eq!(p.due(8 * MS), Some(at(3, 2)));
        assert_eq!((p.due_at(), p.due(100 * MS)), (None, None));
        // A slow move goes at once.
        assert!(p.offer(Kind::Move, at(4, 30), 30 * MS));
    }

    #[test]
    fn presses_and_releases_are_never_held_and_a_release_drops_the_held_move() {
        let mut p = Pacer::new(8 * MS);
        assert!(p.offer(Kind::Down, at(1, 0), 0));
        assert!(!p.offer(Kind::Move, at(2, 1), MS));
        // The release carries the finger's place; the held move must not follow it.
        assert!(p.offer(Kind::Up, at(3, 2), 2 * MS));
        assert_eq!((p.due_at(), p.due(50 * MS)), (None, None));
        // A tap right after: the press goes at once though the interval is not over.
        assert!(p.offer(Kind::Down, at(4, 3), 3 * MS));
        assert!(p.offer(Kind::Up, at(4, 4), 4 * MS));
        let c = p.take_counts();
        assert_eq!((c.presses, c.moves, c.moves_sent), (4, 1, 0));
        assert_eq!(
            c.words(),
            "5 touch events: 4 presses and releases, 1 moves of which 0 went to the guest (at most one a refresh)"
        );
    }

    #[test]
    fn the_interval_follows_the_refresh_rate_and_tags_skip_zero() {
        assert_eq!(interval_ns(60), 14_583_332);
        assert!(interval_ns(120) < interval_ns(60));
        // A refresh rate nobody has is held to a sane one.
        assert_eq!(interval_ns(0), interval_ns(30));
        assert_eq!(next_tag(0), 1);
        assert_eq!(next_tag((1 << 24) - 1), 1);
        assert!((next_tag(5) << TAG_SHIFT) >> TAG_SHIFT == 6);
    }
}
