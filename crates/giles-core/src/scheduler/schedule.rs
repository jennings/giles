//! When jobs fire: pure functions over a job's schedule, its time zone, and the clock.

use croner::Cron;
use giles_types::Timestamp;
use jiff::tz::TimeZone;

use crate::config::ActiveHours;

/// When a job fires.
#[derive(Debug, Clone)]
pub enum When {
    /// At every match of a five-field cron pattern, in the job's time zone.
    Cron(Box<Cron>),
    /// Once, at this instant.
    At(Timestamp),
}

impl When {
    /// The first time it fires strictly after `after`, with cron patterns read in `tz`. `None`:
    /// never again (a one-shot time that has passed, or a pattern with no future match).
    pub fn next_after(&self, tz: &TimeZone, after: Timestamp) -> Option<Timestamp> {
        match self {
            Self::Cron(cron) => cron
                .find_next_occurrence(&after.to_zoned(tz.clone()), false)
                .ok()
                .map(|next| next.timestamp()),
            Self::At(at) => (*at > after).then_some(*at),
        }
    }

    /// When it fires first after the scheduler starts at `now` (PLAN section 6.11). `since` is
    /// when the job last ran, else when it was created (runtime jobs), else `None`. If it was
    /// due at least once between `since` and `now` (the core was down), every missed time is
    /// coalesced into one run at `now`; otherwise it's the next time after `now`.
    pub fn first_fire(
        &self,
        tz: &TimeZone,
        since: Option<Timestamp>,
        now: Timestamp,
    ) -> Option<Timestamp> {
        if let Some(since) = since
            && self
                .next_after(tz, since)
                .is_some_and(|missed| missed <= now)
        {
            return Some(now);
        }
        self.next_after(tz, now)
    }
}

/// Whether a job with `hours` may run at `at` in `tz`: always without active hours.
pub fn in_active_hours(hours: Option<&ActiveHours>, tz: &TimeZone, at: Timestamp) -> bool {
    hours.is_none_or(|hours| hours.contains(at.to_zoned(tz.clone()).time()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_cron;

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn la() -> TimeZone {
        TimeZone::get("America/Los_Angeles").unwrap()
    }

    fn cron(pattern: &str) -> When {
        When::Cron(Box::new(parse_cron(pattern).unwrap()))
    }

    #[test]
    fn cron_patterns_are_read_in_the_job_time_zone() {
        let eight = cron("0 8 * * *");
        // 08:00 in Los Angeles (PDT, UTC-7) is 15:00 UTC.
        assert_eq!(
            eight.next_after(&la(), ts("2026-10-22T16:00:00Z")),
            Some(ts("2026-10-23T15:00:00Z"))
        );
        assert_eq!(
            eight.next_after(&TimeZone::UTC, ts("2026-10-22T16:00:00Z")),
            Some(ts("2026-10-23T08:00:00Z"))
        );
        // After the switch to PST (UTC-8), the same wall time is an hour later in UTC.
        assert_eq!(
            eight.next_after(&la(), ts("2026-11-01T16:00:00Z")),
            Some(ts("2026-11-02T16:00:00Z"))
        );
    }

    #[test]
    fn next_is_strictly_after_even_within_the_firing_second() {
        let every_minute = cron("* * * * *");
        let fired = ts("2026-10-22T17:00:00Z");
        assert_eq!(
            every_minute.next_after(&TimeZone::UTC, fired),
            Some(ts("2026-10-22T17:01:00Z"))
        );
        // A wake-up a few milliseconds late doesn't fire the same minute again.
        assert_eq!(
            every_minute.next_after(&TimeZone::UTC, ts("2026-10-22T17:00:00.003Z")),
            Some(ts("2026-10-22T17:01:00Z"))
        );
    }

    #[test]
    fn one_shot_times_fire_once() {
        let at = When::At(ts("2026-10-22T17:00:00Z"));
        assert_eq!(
            at.next_after(&TimeZone::UTC, ts("2026-10-22T16:59:59Z")),
            Some(ts("2026-10-22T17:00:00Z"))
        );
        assert_eq!(
            at.next_after(&TimeZone::UTC, ts("2026-10-22T17:00:00Z")),
            None
        );
    }

    #[test]
    fn missed_runs_are_coalesced_into_one_at_startup() {
        let hourly = cron("0 * * * *");
        let now = ts("2026-10-22T17:30:00Z");
        // Last ran at 12:00; 13:00 to 17:00 were missed: one run now.
        assert_eq!(
            hourly.first_fire(&TimeZone::UTC, Some(ts("2026-10-22T12:00:00Z")), now),
            Some(now)
        );
        // Last ran at 17:00: nothing missed, next at 18:00.
        assert_eq!(
            hourly.first_fire(&TimeZone::UTC, Some(ts("2026-10-22T17:00:00Z")), now),
            Some(ts("2026-10-22T18:00:00Z"))
        );
        // Never ran and no creation time (a config job): the next match.
        assert_eq!(
            hourly.first_fire(&TimeZone::UTC, None, now),
            Some(ts("2026-10-22T18:00:00Z"))
        );

        // A one-shot created before its time, which passed while the core was down, runs now;
        // once it has run, never again.
        let at = When::At(ts("2026-10-22T17:00:00Z"));
        assert_eq!(
            at.first_fire(&TimeZone::UTC, Some(ts("2026-10-22T16:00:00Z")), now),
            Some(now)
        );
        assert_eq!(
            at.first_fire(&TimeZone::UTC, Some(ts("2026-10-22T17:00:01Z")), now),
            None
        );
    }

    #[test]
    fn active_hours_are_read_in_the_job_time_zone() {
        let day = ActiveHours::parse("08:00-22:00").unwrap();
        // 14:59 UTC is 07:59 in Los Angeles, before the window; 15:00 UTC is 08:00, inside.
        assert!(!in_active_hours(
            Some(&day),
            &la(),
            ts("2026-10-22T14:59:00Z")
        ));
        assert!(in_active_hours(
            Some(&day),
            &la(),
            ts("2026-10-22T15:00:00Z")
        ));
        // 05:00 UTC is 22:00 in Los Angeles: the end is excluded.
        assert!(!in_active_hours(
            Some(&day),
            &la(),
            ts("2026-10-23T05:00:00Z")
        ));
        assert!(in_active_hours(None, &la(), ts("2026-10-23T05:00:00Z")));
        // A window past midnight, in UTC.
        let night = ActiveHours::parse("22:00-06:00").unwrap();
        assert!(in_active_hours(
            Some(&night),
            &TimeZone::UTC,
            ts("2026-10-23T05:00:00Z")
        ));
        assert!(!in_active_hours(
            Some(&night),
            &TimeZone::UTC,
            ts("2026-10-23T06:00:00Z")
        ));
    }
}
