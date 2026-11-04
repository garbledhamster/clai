//! Monday-first local-clock quarter-hour eligibility for enhanced schedules.
use crate::config::workspace_config::ScheduleKind;
use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Selected Monday-first quarter-hours, 0..672. Empty means unrestricted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "bindings.ts")]
pub struct WeeklyAllowedBlocks {
    pub timezone: String,
    pub slots: Vec<u16>,
}

impl WeeklyAllowedBlocks {
    fn zone(&self) -> Result<chrono_tz::Tz, String> {
        self.timezone
            .parse()
            .map_err(|_| format!("Unknown IANA timezone: `{}`", self.timezone))
    }

    pub fn validate(&self, kind: &ScheduleKind) -> Result<(), String> {
        self.zone()?;
        if self.slots.len() > 672 || self.slots.iter().any(|slot| *slot >= 672) {
            return Err("Allowed blocks must be quarter-hours within one week.".into());
        }
        match kind {
            ScheduleKind::EnhancedInterval { .. } => Ok(()),
            ScheduleKind::EnhancedCron { timezone, .. } if *timezone == self.timezone => Ok(()),
            ScheduleKind::EnhancedCron { .. } => {
                Err("Cron and allowed blocks must share one timezone.".into())
            }
            _ => Err("Allowed blocks require enhanced scheduling.".into()),
        }
    }

    fn allowed_in_zone(&self, zone: chrono_tz::Tz, now_ms: i64) -> bool {
        let Some(local) = zone.timestamp_millis_opt(now_ms).single() else {
            return false;
        };
        let slot =
            local.weekday().num_days_from_monday() * 96 + local.hour() * 4 + local.minute() / 15;
        self.slots.is_empty() || self.slots.contains(&(slot as u16))
    }

    /// Invalid persisted data fails closed, including empty selections with an invalid zone.
    pub fn is_allowed(&self, now_ms: i64) -> bool {
        self.zone()
            .is_ok_and(|zone| self.allowed_in_zone(zone, now_ms))
    }

    /// Earliest eligible instant at or after the cadence target. Walk actual UTC
    /// minutes so repeated hours remain eligible and missing local times never exist.
    pub fn next_allowed_at(&self, target_ms: i64) -> Result<i64, String> {
        let zone = self.zone()?;
        if self.allowed_in_zone(zone, target_ms) {
            return Ok(target_ms);
        }
        let mut candidate = target_ms
            .div_euclid(60_000)
            .checked_add(1)
            .and_then(|minute| minute.checked_mul(60_000))
            .ok_or("Allowed-block target is out of range.")?;
        // A sole weekly slot can be nonexistent this week. Search two full
        // weekly cycles plus a day for offset changes, still without inventing
        // any local time or maintaining another scheduling deadline.
        for _ in 0..15 * 24 * 60 {
            if self.allowed_in_zone(zone, candidate) {
                return Ok(candidate);
            }
            candidate = candidate
                .checked_add(60_000)
                .ok_or("Allowed-block target is out of range.")?;
        }
        Err("No allowed block found in the next fifteen days.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    fn ms(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }
    fn blocks(zone: &str, slots: Vec<u16>) -> WeeklyAllowedBlocks {
        WeeklyAllowedBlocks {
            timezone: zone.into(),
            slots,
        }
    }
    #[test]
    fn weekly_blocks_find_next_week_after_skipped_spring_occurrence() {
        let spring = blocks("America/Chicago", vec![6 * 96 + 10]);
        assert_eq!(
            spring.next_allowed_at(ms("2026-03-02T00:00:00Z")).unwrap(),
            ms("2026-03-15T07:30:00Z")
        );
    }

    #[test]
    fn weekly_blocks_open_inclusively_close_exclusively_and_wrap_week() {
        let b = blocks("UTC", vec![36, 671]);
        assert!(!b.is_allowed(ms("2026-11-02T08:59:59Z")));
        assert!(b.is_allowed(ms("2026-11-02T09:00:00Z")));
        assert!(!b.is_allowed(ms("2026-11-02T09:15:00Z")));
        assert!(b.is_allowed(ms("2026-11-08T23:59:59Z")));
        assert_eq!(
            b.next_allowed_at(ms("2026-11-09T00:00:00Z")).unwrap(),
            ms("2026-11-09T09:00:00Z")
        );
    }
    #[test]
    fn weekly_blocks_validate_zone_slots_and_shared_cron_zone() {
        assert!(blocks("Imaginary/Place", vec![])
            .validate(&ScheduleKind::EnhancedInterval {
                interval_seconds: 15
            })
            .is_err());
        assert!(blocks("UTC", vec![672])
            .validate(&ScheduleKind::EnhancedInterval {
                interval_seconds: 15
            })
            .is_err());
        assert!(blocks("UTC", vec![36])
            .validate(&ScheduleKind::EnhancedCron {
                expression: "* * * * *".into(),
                timezone: "America/Chicago".into()
            })
            .is_err());
        let b = blocks("UTC", vec![]);
        assert!(b.is_allowed(0));
        assert_eq!(b.next_allowed_at(12345).unwrap(), 12345);
    }
    #[test]
    fn weekly_blocks_follow_both_fall_hours_and_skip_spring_gap() {
        let fall = blocks("America/Chicago", vec![6 * 96 + 6]); // Sunday 01:30
        assert!(fall.is_allowed(ms("2026-11-01T06:30:00Z")));
        assert!(fall.is_allowed(ms("2026-11-01T07:30:00Z")));
        assert_eq!(
            fall.next_allowed_at(ms("2026-11-01T06:45:00Z")).unwrap(),
            ms("2026-11-01T07:30:00Z")
        );
        let spring = blocks("America/Chicago", vec![6 * 96 + 10]); // Sunday 02:30
        assert_eq!(
            spring.next_allowed_at(ms("2026-03-08T07:59:00Z")).unwrap(),
            ms("2026-03-15T07:30:00Z")
        );
    }
}
