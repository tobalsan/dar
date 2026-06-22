//! Next-fire computation from a cron expression + IANA timezone + optional
//! `startAt` anchor. Mirrors aihub `schedule.ts`:
//! `computeNextRunAtMs(schedule, now)` anchors the search at
//! `max(now, startAt)` and returns the next cron instant strictly after it.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;

use crate::store::Schedule;

/// Compute the next fire instant (UTC epoch millis) for `schedule`, searching
/// from `max(now_ms, startAt)`. The cron expression is evaluated in the job's
/// IANA timezone so wall-clock schedules respect DST.
pub fn compute_next_run_at_ms(schedule: &Schedule, now_ms: i64) -> Result<i64> {
    let anchor_ms = match &schedule.start_at {
        Some(start_at) => {
            let start_ms = DateTime::parse_from_rfc3339(start_at)
                .with_context(|| format!("parsing startAt {start_at:?}"))?
                .timestamp_millis();
            now_ms.max(start_ms)
        }
        None => now_ms,
    };

    let tz: Tz = schedule
        .tz
        .parse()
        .with_context(|| format!("parsing timezone {:?}", schedule.tz))?;

    let cron = Cron::from_str(&schedule.cron)
        .with_context(|| format!("parsing cron expression {:?}", schedule.cron))?;

    // Evaluate in the schedule's timezone so wall-clock fields (e.g. "0 8 * * *")
    // land on the local hour, then convert the result back to a UTC instant.
    let anchor_utc = Utc
        .timestamp_millis_opt(anchor_ms)
        .single()
        .with_context(|| format!("anchor millis {anchor_ms} out of range"))?;
    let anchor_local = anchor_utc.with_timezone(&tz);

    let next = cron
        .find_next_occurrence(&anchor_local, false)
        .with_context(|| format!("computing next occurrence for {:?}", schedule.cron))?;

    Ok(next.with_timezone(&Utc).timestamp_millis())
}

/// Human-readable schedule string used in output frontmatter / body.
/// Matches aihub `formatScheduleForOutput`: `"<cron> <tz>"`, with
/// ` @ <startAt>` appended when an anchor is set.
pub fn format_schedule(schedule: &Schedule) -> String {
    let base = format!("{} {}", schedule.cron, schedule.tz);
    match &schedule.start_at {
        Some(start_at) => format!("{base} @ {start_at}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(ms: i64) -> String {
        Utc.timestamp_millis_opt(ms)
            .single()
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn sched(cron: &str, tz: &str) -> Schedule {
        Schedule {
            cron: cron.to_string(),
            tz: tz.to_string(),
            start_at: None,
        }
    }

    fn now(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn computes_next_cron_run_in_utc() {
        let next = compute_next_run_at_ms(&sched("0 9 * * *", "UTC"), now("2024-06-15T08:00:00Z"))
            .unwrap();
        assert_eq!(iso(next), "2024-06-15T09:00:00.000Z");
    }

    #[test]
    fn schedules_next_day_when_todays_cron_time_has_passed() {
        let next = compute_next_run_at_ms(&sched("0 9 * * *", "UTC"), now("2024-06-15T10:00:00Z"))
            .unwrap();
        assert_eq!(iso(next), "2024-06-16T09:00:00.000Z");
    }

    #[test]
    fn uses_timezone() {
        // 09:00 America/New_York in January (EST, UTC-5) == 14:00 UTC.
        let next = compute_next_run_at_ms(
            &sched("0 9 * * *", "America/New_York"),
            now("2024-01-15T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(iso(next), "2024-01-15T14:00:00.000Z");
    }

    #[test]
    fn dst_spring_forward_summer_offset() {
        // In July America/New_York is EDT (UTC-4), so 09:00 local == 13:00 UTC.
        let next = compute_next_run_at_ms(
            &sched("0 9 * * *", "America/New_York"),
            now("2024-07-15T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(iso(next), "2024-07-15T13:00:00.000Z");
    }

    #[test]
    fn start_at_anchors_search_forward() {
        // now is before startAt: the anchor jumps forward to startAt, so the
        // next 09:00 UTC fire is computed relative to the anchor, not now.
        let schedule = Schedule {
            cron: "0 9 * * *".to_string(),
            tz: "UTC".to_string(),
            start_at: Some("2026-05-19T07:00:00.000Z".to_string()),
        };
        let next = compute_next_run_at_ms(&schedule, now("2020-01-01T00:00:00Z")).unwrap();
        assert_eq!(iso(next), "2026-05-19T09:00:00.000Z");
    }

    #[test]
    fn start_at_in_past_does_not_rewind() {
        // now is after startAt: max(now, startAt) == now, so behaves as if no anchor.
        let schedule = Schedule {
            cron: "0 9 * * *".to_string(),
            tz: "UTC".to_string(),
            start_at: Some("2020-01-01T00:00:00.000Z".to_string()),
        };
        let next = compute_next_run_at_ms(&schedule, now("2024-06-15T08:00:00Z")).unwrap();
        assert_eq!(iso(next), "2024-06-15T09:00:00.000Z");
    }

    #[test]
    fn format_schedule_with_and_without_start_at() {
        assert_eq!(
            format_schedule(&sched("0 8 * * *", "Europe/Paris")),
            "0 8 * * * Europe/Paris"
        );
        let anchored = Schedule {
            cron: "0 8 * * *".to_string(),
            tz: "Europe/Paris".to_string(),
            start_at: Some("2026-05-19T07:00:00.000Z".to_string()),
        };
        assert_eq!(
            format_schedule(&anchored),
            "0 8 * * * Europe/Paris @ 2026-05-19T07:00:00.000Z"
        );
    }
}
