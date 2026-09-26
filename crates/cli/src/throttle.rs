//! Repeated-condition accounting for the receiver's stderr. A condition that recurs per request or
//! per batch — a full export queue, a failing destination, a body this build cannot decode — is
//! noted when it first occurs and then at most once per [`INTERVAL`], each note with the running
//! count, so a service log grows with how long a condition lasts, never with how much traffic
//! meets it. Occurrences a note has not counted yet are reported once they have waited an interval
//! ([`Throttle::overdue`]) or when the receiver stops ([`Throttle::unnoted`]), so a condition that
//! stopped still says how often it occurred, as syslog's "last message repeated" does.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The least time between two notes of one condition.
pub const INTERVAL: Duration = Duration::from_secs(10 * 60);

/// One recurring condition: how often it has occurred, and how many of those the last note, made
/// when, counted. Taken by shared reference, since request handlers on several threads can meet
/// the same condition at once.
#[derive(Debug, Default)]
pub struct Throttle(Mutex<Occurrences>);

#[derive(Debug, Default)]
struct Occurrences {
    count: u64,
    noted: u64,
    noted_at: Option<Instant>,
}

/// When a throttle's held-back occurrences are reported: once they have waited an interval, or
/// all of them as the receiver stops.
#[derive(Debug, Clone, Copy)]
pub enum Tally {
    Overdue(Instant),
    Stopping,
}

impl Throttle {
    /// The running count to report for the occurrences no note has counted, per `when`.
    pub fn tally(&self, when: Tally) -> Option<u64> {
        match when {
            Tally::Overdue(now) => self.overdue(now),
            Tally::Stopping => self.unnoted(),
        }
    }

    /// Count one occurrence at `now`, and return the running count when it is to be noted: the
    /// first occurrence, then the first once [`INTERVAL`] has passed since the last note.
    pub fn occur(&self, now: Instant) -> Option<u64> {
        let mut o = self.lock();
        o.count += 1;
        if o.noted_at
            .is_some_and(|at| now.duration_since(at) < INTERVAL)
        {
            return None;
        }
        Some(o.note(now))
    }

    /// The running count when occurrences no note has counted have waited [`INTERVAL`] since the
    /// last one — the tally of a condition that has since stopped occurring.
    fn overdue(&self, now: Instant) -> Option<u64> {
        let mut o = self.lock();
        if o.count == o.noted
            || o.noted_at
                .is_some_and(|at| now.duration_since(at) < INTERVAL)
        {
            return None;
        }
        Some(o.note(now))
    }

    /// The running count when any occurrence no note has counted remains, however recent — for a
    /// receiver that is stopping.
    fn unnoted(&self) -> Option<u64> {
        let mut o = self.lock();
        (o.count != o.noted).then(|| {
            o.noted = o.count;
            o.count
        })
    }

    #[cfg(test)]
    pub fn count(&self) -> u64 {
        self.lock().count
    }

    fn lock(&self) -> MutexGuard<'_, Occurrences> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Occurrences {
    fn note(&mut self, now: Instant) -> u64 {
        self.noted = self.count;
        self.noted_at = Some(now);
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, INTERVAL, Instant, Throttle};

    #[test]
    fn notes_the_first_occurrence_then_at_most_once_an_interval() {
        let t = Throttle::default();
        let start = Instant::now();
        assert_eq!(t.occur(start), Some(1), "the first occurrence is noted");
        for _ in 0..10_000 {
            assert_eq!(t.occur(start + INTERVAL / 2), None, "however many meet it");
        }
        assert_eq!(t.occur(start + INTERVAL), Some(10_002));
        assert_eq!(
            t.occur(start + INTERVAL + Duration::from_secs(1)),
            None,
            "the interval runs from the last note"
        );
    }

    #[test]
    fn a_burst_that_stopped_reports_its_count_once_it_has_waited_an_interval() {
        let t = Throttle::default();
        let start = Instant::now();
        t.occur(start);
        for _ in 0..4 {
            t.occur(start + Duration::from_secs(60));
        }
        assert_eq!(t.overdue(start + INTERVAL / 2), None, "not yet an interval");
        assert_eq!(t.overdue(start + INTERVAL), Some(5));
        assert_eq!(
            t.overdue(start + 3 * INTERVAL),
            None,
            "nothing new to report"
        );
        t.occur(start + 3 * INTERVAL + Duration::from_secs(1));
        assert_eq!(t.unnoted(), None, "that occurrence was noted as it came");
        t.occur(start + 3 * INTERVAL + Duration::from_secs(2));
        assert_eq!(t.unnoted(), Some(7), "a stopping receiver reports the rest");
        assert_eq!(t.unnoted(), None);
    }
}
