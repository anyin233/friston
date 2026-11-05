//! Coalesces bursts of change signals per source.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// Fires a source once it has been quiet for `quiet`, or after `max_wait` of
/// continuous changes, so a note typed non-stop during a meeting still gets
/// indexed periodically.
#[derive(Debug)]
pub struct Debouncer<K> {
    quiet: Duration,
    max_wait: Duration,
    pending: HashMap<K, (Instant, Instant)>,
}

impl<K: Eq + Hash + Clone> Debouncer<K> {
    /// Creates a debouncer.
    ///
    /// Params: `quiet` – idle time before firing; `max_wait` – upper bound on delay.
    /// Returns: an empty debouncer.
    pub fn new(quiet: Duration, max_wait: Duration) -> Self {
        Self {
            quiet,
            max_wait,
            pending: HashMap::new(),
        }
    }

    /// Records a change to `key` at `now`.
    pub fn touch(&mut self, key: K, now: Instant) {
        self.pending
            .entry(key)
            .and_modify(|(_, last)| *last = now)
            .or_insert((now, now));
    }

    /// Removes and returns every key that is due at `now`.
    ///
    /// Params: `now` – current time.
    /// Returns: keys to re-index.
    pub fn drain_due(&mut self, now: Instant) -> Vec<K> {
        let due: Vec<K> = self
            .pending
            .iter()
            .filter(|(_, (first, last))| {
                now.duration_since(*last) >= self.quiet
                    || now.duration_since(*first) >= self.max_wait
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &due {
            self.pending.remove(k);
        }
        due
    }

    /// Earliest instant at which some key becomes due (for timer scheduling).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|(first, last)| (*last + self.quiet).min(*first + self.max_wait))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_after_quiet_period_and_after_max_wait() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = Debouncer::new(ms(1500), ms(10_000));
        d.touch("note", t0);
        d.touch("note", t0 + ms(1000));
        assert!(
            d.drain_due(t0 + ms(2000)).is_empty(),
            "still within quiet window"
        );
        assert_eq!(d.drain_due(t0 + ms(2600)), vec!["note"]);

        // Continuous typing: touched every second, still fires at max_wait.
        for s in 0..=10 {
            d.touch("live", t0 + Duration::from_secs(s));
        }
        assert_eq!(d.drain_due(t0 + ms(10_000)), vec!["live"]);
        assert!(d.next_deadline().is_none());
    }
}
