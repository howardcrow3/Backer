//! Schedule maths in the user's local time zone, DST-safe.
use jiff::{civil::Date, tz::TimeZone, Span, Timestamp};
use nasbak_core::{Job, Schedule};

/// Slot times on one calendar day. Nonexistent local times (spring forward) shift later; repeated ones (fall back) use the first.
fn slot_on(tz: &TimeZone, d: Date, h: u8, m: u8) -> Option<Timestamp> {
    let dt = d.at(h as i8, m as i8, 0, 0);
    tz.to_ambiguous_zoned(dt).compatible().ok().map(|z| z.timestamp())
}

fn day_matches(s: &Schedule, d: Date) -> Option<(u8, u8)> {
    match s {
        Schedule::Daily { hour, minute } => Some((*hour, *minute)),
        Schedule::Weekly { days, hour, minute } => {
            let wd = d.weekday().to_monday_one_offset() as u8;
            days.contains(&wd).then_some((*hour, *minute))
        }
        _ => None,
    }
}

/// The latest scheduled slot at or before `now`.
pub fn previous_slot(s: &Schedule, now: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
    let mut d = now.to_zoned(tz.clone()).date();
    for _ in 0..9 {
        if let Some((h, m)) = day_matches(s, d) {
            if let Some(t) = slot_on(tz, d, h, m) {
                if t <= now {
                    return Some(t);
                }
            }
        }
        d = d.yesterday().ok()?;
    }
    None
}

/// The first scheduled slot strictly after `now`. For `EveryHours`, relative to the last run start.
pub fn next_run(s: &Schedule, last_start: Option<Timestamp>, now: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
    match s {
        Schedule::Manual => None,
        Schedule::EveryHours { hours } => {
            let base = last_start?;
            let next = base.checked_add(Span::new().hours(*hours as i64)).ok()?;
            Some(if next < now { now } else { next })
        }
        _ => {
            let mut d = now.to_zoned(tz.clone()).date();
            for _ in 0..9 {
                if let Some((h, m)) = day_matches(s, d) {
                    if let Some(t) = slot_on(tz, d, h, m) {
                        if t > now {
                            return Some(t);
                        }
                    }
                }
                d = d.tomorrow().ok()?;
            }
            None
        }
    }
}

/// Should this job start now? Catch-up runs a missed slot once, no matter how many were missed.
pub fn is_due(job: &Job, last_start: Option<Timestamp>, now: Timestamp, tz: &TimeZone) -> bool {
    if job.paused {
        return false;
    }
    match &job.schedule {
        Schedule::Manual => false,
        Schedule::EveryHours { hours } => match last_start {
            None => true,
            Some(l) => l.checked_add(Span::new().hours(*hours as i64)).map(|n| n <= now).unwrap_or(false),
        },
        s => {
            let Some(prev) = previous_slot(s, now, tz) else { return false };
            if last_start.map(|l| l >= prev).unwrap_or(false) {
                return false;
            }
            // Never run before the job existed: with no history, wait for the next slot.
            let Some(_) = last_start else {
                return now.duration_since(prev).as_secs() < 60;
            };
            job.catch_up || now.duration_since(prev).as_secs() < 60
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> TimeZone {
        TimeZone::get("America/Los_Angeles").unwrap()
    }
    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }
    fn job(s: Schedule) -> Job {
        Job { id: 1, name: "j".into(), folders: vec![], dest_root: "/".into(), schedule: s, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None }
    }

    #[test]
    fn daily_next_run_local_time() {
        // 2:00 AM PDT = 09:00Z
        let n = next_run(&Schedule::Daily { hour: 2, minute: 0 }, None, ts("2026-10-13T12:00:00Z"), &tz()).unwrap();
        assert_eq!(n, ts("2026-10-14T09:00:00Z"));
    }

    #[test]
    fn dst_spring_forward_nonexistent_time_shifts_later() {
        // 2026-03-08 02:30 does not exist in Los Angeles.
        let n = next_run(&Schedule::Daily { hour: 2, minute: 30 }, None, ts("2026-03-08T00:00:00Z"), &tz()).unwrap();
        assert_eq!(n, ts("2026-03-08T10:30:00Z")); // 03:30 PDT
    }

    #[test]
    fn dst_fall_back_runs_once_at_first_occurrence() {
        // 2026-11-01 01:30 happens twice; use the first (PDT, 08:30Z).
        let s = Schedule::Daily { hour: 1, minute: 30 };
        let n = next_run(&s, None, ts("2026-11-01T00:00:00Z"), &tz()).unwrap();
        assert_eq!(n, ts("2026-11-01T08:30:00Z"));
        // After that run, the second 01:30 (PST, 09:30Z) must not trigger another.
        assert!(!is_due(&job(s), Some(n), ts("2026-11-01T09:31:00Z"), &tz()));
    }

    #[test]
    fn catch_up_after_sleep_runs_once() {
        let j = job(Schedule::Daily { hour: 2, minute: 0 });
        let last = ts("2026-10-10T09:00:00Z");
        // Mac wakes at 10 AM local two days later.
        assert!(is_due(&j, Some(last), ts("2026-10-12T17:00:00Z"), &tz()));
        // Already ran at that slot.
        assert!(!is_due(&j, Some(ts("2026-10-12T09:00:05Z")), ts("2026-10-12T17:00:00Z"), &tz()));
    }

    #[test]
    fn no_catch_up_when_disabled() {
        let mut j = job(Schedule::Daily { hour: 2, minute: 0 });
        j.catch_up = false;
        assert!(!is_due(&j, Some(ts("2026-10-10T09:00:00Z")), ts("2026-10-12T17:00:00Z"), &tz()));
        assert!(is_due(&j, Some(ts("2026-10-10T09:00:00Z")), ts("2026-10-12T09:00:20Z"), &tz()));
    }

    #[test]
    fn weekly_and_hourly_and_paused() {
        // Mondays only. 2026-10-13 is a Tuesday; next Monday is 10-19.
        let w = Schedule::Weekly { days: vec![1], hour: 3, minute: 0 };
        assert_eq!(next_run(&w, None, ts("2026-10-13T12:00:00Z"), &tz()).unwrap(), ts("2026-10-19T10:00:00Z"));
        let h = job(Schedule::EveryHours { hours: 2 });
        assert!(!is_due(&h, Some(ts("2026-10-13T10:00:00Z")), ts("2026-10-13T11:59:00Z"), &tz()));
        assert!(is_due(&h, Some(ts("2026-10-13T10:00:00Z")), ts("2026-10-13T12:00:00Z"), &tz()));
        let mut p = job(Schedule::EveryHours { hours: 1 });
        p.paused = true;
        assert!(!is_due(&p, None, ts("2026-10-13T12:00:00Z"), &tz()));
    }
}
