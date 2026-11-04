//! Schedule computation — single source of truth for "when does the
//! manager run next?" Used by the runner after each completion, by
//! `workspace_set_schedule` when arming a fresh anchor, and by
//! `workspace_preview_schedule` for the UI's live next-N-runs preview.
//!
//! The function takes a `ScheduleKind` and a wall-clock `now` (Unix ms),
//! and returns the next fire time as Unix ms. For `Interval` mode this
//! is just `now + interval_ms`; for `Cron` it parses a 5-field
//! expression, computes the next fire in the user's IANA timezone, and
//! converts back to Unix ms.
//!
//! Errors:
//! - `Interval` with `interval_minutes == 0`: rejected — would degenerate
//!   into an infinite tight loop.
//! - `Cron` with an empty or malformed expression / unknown timezone:
//!   rejected with a message safe to surface to the UI.
//! - `Cron` whose iterator yields no upcoming time (e.g. a pattern that
//!   matches a year-end edge case far in the past): rejected.

use std::str::FromStr;

use chrono::TimeZone;
use cron::Schedule;

use crate::config::workspace_config::ScheduleKind;

/// Compute the next fire time for the given schedule, strictly after
/// `now_unix_ms`. Returns the result in Unix ms (UTC), ready to persist
/// into `WorkspaceSchedule.next_run_at_unix_ms`.
pub fn compute_next_run_at(kind: &ScheduleKind, now_unix_ms: i64) -> Result<i64, String> {
    match kind {
        ScheduleKind::Interval { interval_minutes } => {
            if *interval_minutes == 0 {
                return Err("Interval schedule requires at least 1 minute.".to_string());
            }
            let interval_ms = (*interval_minutes as i64).saturating_mul(60_000);
            Ok(now_unix_ms.saturating_add(interval_ms))
        }
        ScheduleKind::EnhancedInterval { interval_seconds } => {
            if *interval_seconds < 15 {
                return Err("Interval schedule requires at least 15 seconds.".to_string());
            }
            Ok(now_unix_ms.saturating_add(i64::from(*interval_seconds) * 1_000))
        }
        ScheduleKind::Cron {
            expression,
            timezone,
        }
        | ScheduleKind::EnhancedCron {
            expression,
            timezone,
        } => compute_next_cron_run_at(expression, timezone, now_unix_ms),
    }
}

/// Compute the next `n` cron fire times for the live preview in the UI.
/// Returns an empty list if `kind` is not Cron — interval mode has no
/// list to display beyond the single computed next-run.
pub fn upcoming_cron_runs(
    kind: &ScheduleKind,
    now_unix_ms: i64,
    n: usize,
) -> Result<Vec<i64>, String> {
    let (ScheduleKind::Cron {
        expression,
        timezone,
    }
    | ScheduleKind::EnhancedCron {
        expression,
        timezone,
    }) = kind
    else {
        return Ok(Vec::new());
    };
    let schedule = parse_cron(expression)?;
    let tz = parse_timezone(timezone)?;
    let now = tz
        .timestamp_millis_opt(now_unix_ms)
        .single()
        .ok_or_else(|| format!("Invalid `now` timestamp: {}", now_unix_ms))?;
    Ok(schedule
        .after(&now)
        .take(n)
        .map(|dt| dt.timestamp_millis())
        .collect())
}

fn compute_next_cron_run_at(
    expression: &str,
    timezone: &str,
    now_unix_ms: i64,
) -> Result<i64, String> {
    let schedule = parse_cron(expression)?;
    let tz = parse_timezone(timezone)?;
    let now = tz
        .timestamp_millis_opt(now_unix_ms)
        .single()
        .ok_or_else(|| format!("Invalid `now` timestamp: {}", now_unix_ms))?;
    schedule
        .after(&now)
        .next()
        .map(|dt| dt.timestamp_millis())
        .ok_or_else(|| {
            "Cron expression has no upcoming fire times — check that the pattern is valid."
                .to_string()
        })
}

fn parse_cron(expression: &str) -> Result<Schedule, String> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err("Cron expression is empty.".to_string());
    }
    // Users write 5-field Vixie cron (`min hour dom mon dow`). The
    // `cron` crate requires a leading seconds field; pad with `"0 "` so
    // schedules fire on the minute boundary.
    let padded = format!("0 {}", trimmed);
    Schedule::from_str(&padded).map_err(|e| format!("Invalid cron expression: {}", e))
}

fn parse_timezone(name: &str) -> Result<chrono_tz::Tz, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Cron schedule requires a timezone.".to_string());
    }
    chrono_tz::Tz::from_str(trimmed).map_err(|_| format!("Unknown IANA timezone: `{}`", trimmed))
}

/// Timing state for opt-in schedules. Intervals use elapsed time; cron uses
/// wall time and retains the last occurrence to avoid replay after a rollback.
#[derive(Debug, Clone)]
pub struct EnhancedTiming {
    kind: ScheduleKind,
    interval_deadline: Option<std::time::Instant>,
    cron_target: Option<i64>,
    last_cron_target: Option<i64>,
    allowed_blocks: Option<super::weekly_blocks::WeeklyAllowedBlocks>,
}

impl EnhancedTiming {
    pub fn new(kind: ScheduleKind, now_ms: i64, now: std::time::Instant) -> Result<Self, String> {
        if !matches!(
            kind,
            ScheduleKind::EnhancedInterval { .. } | ScheduleKind::EnhancedCron { .. }
        ) {
            return Err("Expected an enhanced schedule.".into());
        }
        let mut timing = Self {
            kind,
            interval_deadline: None,
            cron_target: None,
            last_cron_target: None,
            allowed_blocks: None,
        };
        timing.wake(now_ms, now)?;
        Ok(timing)
    }

    pub fn is_cron(&self) -> bool {
        matches!(self.kind, ScheduleKind::EnhancedCron { .. })
    }

    pub fn set_allowed_blocks(
        &mut self,
        blocks: Option<super::weekly_blocks::WeeklyAllowedBlocks>,
    ) -> Result<(), String> {
        if let Some(blocks) = &blocks {
            blocks.validate(&self.kind)?;
        }
        self.allowed_blocks = blocks;
        Ok(())
    }

    pub fn allowed_blocks(&self) -> Option<super::weekly_blocks::WeeklyAllowedBlocks> {
        self.allowed_blocks.clone()
    }

    pub fn waiting_for_block(&self, now_ms: i64, now: std::time::Instant) -> bool {
        self.cadence_due(now_ms, now) && !self.block_is_open(now_ms)
    }

    fn block_is_open(&self, now_ms: i64) -> bool {
        self.allowed_blocks
            .as_ref()
            .is_none_or(|blocks| blocks.is_allowed(now_ms))
    }

    pub fn is_due(&self, now_ms: i64, now: std::time::Instant) -> bool {
        self.cadence_due(now_ms, now) && self.block_is_open(now_ms)
    }

    fn cadence_due(&self, now_ms: i64, now: std::time::Instant) -> bool {
        self.interval_deadline
            .is_some_and(|deadline| now >= deadline)
            || self.cron_target.is_some_and(|target| now_ms >= target)
    }

    /// Whole seconds until the same deadline used by `is_due`.
    pub fn seconds_until_next_run(&self, now_ms: i64, now: std::time::Instant) -> u64 {
        let target = if let Some(deadline) = self.interval_deadline {
            now_ms.saturating_add(
                i64::try_from(deadline.saturating_duration_since(now).as_millis())
                    .unwrap_or(i64::MAX),
            )
        } else {
            self.cron_target.unwrap_or(now_ms).max(now_ms)
        };
        let eligible = match &self.allowed_blocks {
            Some(blocks) => match blocks.next_allowed_at(target) {
                Ok(eligible) => eligible,
                Err(_) => return u64::MAX,
            },
            None => target,
        };
        eligible.saturating_sub(now_ms).max(0) as u64 / 1000
    }

    pub fn complete(&mut self, now_ms: i64, now: std::time::Instant) -> Result<(), String> {
        if let Some(target) = self.cron_target {
            self.last_cron_target = Some(target);
        }
        self.wake(now_ms, now)
    }

    pub fn clock_changed(&mut self, now_ms: i64, now: std::time::Instant) -> Result<(), String> {
        if matches!(self.kind, ScheduleKind::EnhancedCron { .. }) {
            self.wake(now_ms, now)?;
        }
        Ok(())
    }

    pub fn wake(&mut self, now_ms: i64, now: std::time::Instant) -> Result<(), String> {
        match &self.kind {
            ScheduleKind::EnhancedInterval { interval_seconds } => {
                compute_next_run_at(&self.kind, now_ms)?;
                self.interval_deadline =
                    now.checked_add(std::time::Duration::from_secs(u64::from(*interval_seconds)));
                if self.interval_deadline.is_none() {
                    return Err("Interval deadline is out of range.".into());
                }
                self.cron_target = None;
            }
            ScheduleKind::EnhancedCron { .. } => {
                self.cron_target = Some(compute_next_run_at(
                    &self.kind,
                    now_ms.max(self.last_cron_target.unwrap_or(i64::MIN)),
                )?);
                self.interval_deadline = None;
            }
            _ => return Err("Expected an enhanced schedule.".into()),
        }
        Ok(())
    }
}

/// Best-effort detection of the host's IANA timezone, exposed to the FE
/// as the default value for new cron schedules. Falls back to `"UTC"`
/// if the platform can't resolve the local zone.
pub fn host_timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_blocks_countdown_uses_exact_deadline_before_rounding_seconds() {
        use super::super::weekly_blocks::WeeklyAllowedBlocks;
        use std::time::{Duration, Instant};
        let anchor = chrono::DateTime::parse_from_rfc3339("2026-11-02T09:14:00.500Z")
            .unwrap()
            .timestamp_millis();
        let next_open = chrono::DateTime::parse_from_rfc3339("2026-11-02T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        let start = Instant::now();
        for kind in [
            ScheduleKind::EnhancedInterval {
                interval_seconds: 60,
            },
            ScheduleKind::EnhancedCron {
                expression: "15 9 * * *".into(),
                timezone: "UTC".into(),
            },
        ] {
            let mut timing = EnhancedTiming::new(kind, anchor, start).unwrap();
            timing
                .set_allowed_blocks(Some(WeeklyAllowedBlocks {
                    timezone: "UTC".into(),
                    slots: vec![36, 40],
                }))
                .unwrap();
            let now_ms = anchor + 250;
            assert_eq!(
                timing.seconds_until_next_run(now_ms, start + Duration::from_millis(250)),
                (next_open - now_ms) as u64 / 1000
            );
        }
    }

    #[test]
    fn weekly_blocks_hold_one_due_check_and_reset_on_wake_for_both_cadences() {
        use super::super::weekly_blocks::WeeklyAllowedBlocks;
        use std::time::{Duration, Instant};
        let wall = chrono::DateTime::parse_from_rfc3339("2026-11-02T08:58:00Z")
            .unwrap()
            .timestamp_millis();
        let start = Instant::now();
        for kind in [
            ScheduleKind::EnhancedInterval {
                interval_seconds: 15,
            },
            ScheduleKind::EnhancedCron {
                expression: "* * * * *".into(),
                timezone: "UTC".into(),
            },
        ] {
            let mut timing = EnhancedTiming::new(kind, wall, start).unwrap();
            timing
                .set_allowed_blocks(Some(WeeklyAllowedBlocks {
                    timezone: "UTC".into(),
                    slots: vec![36],
                }))
                .unwrap();
            assert!(!timing.is_due(wall + 59_999, start + Duration::from_secs(59)));
            assert!(timing.waiting_for_block(wall + 120_000 - 1, start + Duration::from_secs(60)));
            assert_eq!(
                timing.seconds_until_next_run(wall + 90_000, start + Duration::from_secs(90)),
                30
            );
            assert!(timing.is_due(wall + 120_000, start + Duration::from_secs(120)));
            assert!(!timing.is_due(wall + 1_020_000, start + Duration::from_secs(1020)));
            timing
                .wake(wall + 120_000, start + Duration::from_secs(120))
                .unwrap();
            assert!(!timing.is_due(wall + 120_000, start + Duration::from_secs(120)));
            timing
                .complete(wall + 135_000, start + Duration::from_secs(135))
                .unwrap();
            assert!(!timing.is_due(wall + 135_000, start + Duration::from_secs(135)));
        }
    }

    #[test]
    fn enhanced_interval_uses_elapsed_time_and_restarts_after_wake() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let kind = ScheduleKind::EnhancedInterval {
            interval_seconds: 15,
        };
        let mut timing = EnhancedTiming::new(kind, 1_000, start).unwrap();
        assert!(!timing.is_due(10_000_000, start + Duration::from_secs(14)));
        assert!(timing.is_due(0, start + Duration::from_secs(15)));
        timing.complete(0, start + Duration::from_secs(20)).unwrap();
        assert!(!timing.is_due(0, start + Duration::from_secs(34)));
        timing.wake(0, start + Duration::from_secs(34)).unwrap();
        assert!(!timing.is_due(0, start + Duration::from_secs(48)));
        assert!(timing.is_due(0, start + Duration::from_secs(49)));
    }

    #[test]
    fn enhanced_cron_tracks_wall_clock_without_repeating_after_rollback() {
        use std::time::Instant;
        let start = Instant::now();
        let kind = ScheduleKind::EnhancedCron {
            expression: "* * * * *".into(),
            timezone: "UTC".into(),
        };
        let mut timing = EnhancedTiming::new(kind, 0, start).unwrap();
        assert!(!timing.is_due(59_999, start));
        assert!(timing.is_due(60_000, start));
        timing.complete(60_000, start).unwrap();
        assert!(!timing.is_due(0, start));
        assert!(!timing.is_due(60_000, start));
        assert!(timing.is_due(120_000, start));
    }

    #[test]
    fn enhanced_interval_enforces_minimum_and_anchors_to_completion() {
        let too_short = ScheduleKind::EnhancedInterval {
            interval_seconds: 14,
        };
        assert!(compute_next_run_at(&too_short, 1_000).is_err());
        let minimum = ScheduleKind::EnhancedInterval {
            interval_seconds: 15,
        };
        assert_eq!(compute_next_run_at(&minimum, 1_000).unwrap(), 16_000);
    }

    #[test]
    fn interval_returns_now_plus_minutes() {
        let now = 1_000_000_000_000_i64;
        let kind = ScheduleKind::Interval {
            interval_minutes: 5,
        };
        assert_eq!(compute_next_run_at(&kind, now).unwrap(), now + 5 * 60_000);
    }

    #[test]
    fn interval_zero_is_rejected() {
        let kind = ScheduleKind::Interval {
            interval_minutes: 0,
        };
        assert!(compute_next_run_at(&kind, 0).is_err());
    }

    #[test]
    fn cron_returns_future_unix_ms() {
        let kind = ScheduleKind::Cron {
            // Every minute, UTC.
            expression: "* * * * *".to_string(),
            timezone: "UTC".to_string(),
        };
        let now = chrono::Utc::now().timestamp_millis();
        let next = compute_next_run_at(&kind, now).unwrap();
        assert!(
            next > now,
            "next ({}) must be strictly after now ({})",
            next,
            now
        );
        // Should fire within the next 60 seconds.
        assert!((next - now) <= 60_000);
    }

    #[test]
    fn cron_invalid_expression_is_rejected() {
        let kind = ScheduleKind::Cron {
            expression: "not a cron".to_string(),
            timezone: "UTC".to_string(),
        };
        let err = compute_next_run_at(&kind, 0).unwrap_err();
        assert!(err.to_lowercase().contains("invalid cron"));
    }

    #[test]
    fn cron_unknown_timezone_is_rejected() {
        let kind = ScheduleKind::Cron {
            expression: "* * * * *".to_string(),
            timezone: "Imaginary/Place".to_string(),
        };
        let err = compute_next_run_at(&kind, 0).unwrap_err();
        assert!(err.to_lowercase().contains("timezone"));
    }

    #[test]
    fn cron_empty_timezone_is_rejected() {
        let kind = ScheduleKind::Cron {
            expression: "* * * * *".to_string(),
            timezone: String::new(),
        };
        assert!(compute_next_run_at(&kind, 0).is_err());
    }

    #[test]
    fn upcoming_cron_runs_returns_n_strictly_increasing_times() {
        let kind = ScheduleKind::Cron {
            expression: "0 * * * *".to_string(), // top of every hour
            timezone: "UTC".to_string(),
        };
        let now = chrono::Utc::now().timestamp_millis();
        let upcoming = upcoming_cron_runs(&kind, now, 3).unwrap();
        assert_eq!(upcoming.len(), 3);
        assert!(upcoming[0] > now);
        assert!(upcoming[1] > upcoming[0]);
        assert!(upcoming[2] > upcoming[1]);
    }

    #[test]
    fn upcoming_cron_runs_is_empty_for_interval_kind() {
        let kind = ScheduleKind::Interval {
            interval_minutes: 5,
        };
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            upcoming_cron_runs(&kind, now, 3).unwrap(),
            Vec::<i64>::new()
        );
    }

    /// Regression for the `intervalMinutes` deserialization bug: the FE
    /// sends camelCase keys and serde must honor them for fields *inside*
    /// the variant. Without `rename_all_fields = "camelCase"` on the
    /// enum, the field name stays snake_case (`interval_minutes`) and a
    /// camelCase payload silently defaults to 0, tripping the
    /// "≥1 minute" validator with a confusing message.
    #[test]
    fn camel_case_payload_round_trips_through_serde() {
        let json = r#"{"type":"interval","intervalMinutes":1440}"#;
        let kind: ScheduleKind = serde_json::from_str(json).unwrap();
        match kind {
            ScheduleKind::Interval { interval_minutes } => {
                assert_eq!(interval_minutes, 1440);
            }
            other => panic!("expected Interval, got {:?}", other),
        }
        // Round-trip back to JSON: must serialize as camelCase too so
        // the on-disk shape matches the wire shape.
        let reserialized = serde_json::to_string(&ScheduleKind::Interval {
            interval_minutes: 1440,
        })
        .unwrap();
        assert!(reserialized.contains("intervalMinutes"));
        assert!(!reserialized.contains("interval_minutes"));
    }

    #[test]
    fn camel_case_cron_payload_round_trips_through_serde() {
        let json = r#"{"type":"cron","expression":"0 9 * * 1-5","timezone":"UTC"}"#;
        let kind: ScheduleKind = serde_json::from_str(json).unwrap();
        match kind {
            ScheduleKind::Cron {
                expression,
                timezone,
            } => {
                assert_eq!(expression, "0 9 * * 1-5");
                assert_eq!(timezone, "UTC");
            }
            other => panic!("expected Cron, got {:?}", other),
        }
    }
}
