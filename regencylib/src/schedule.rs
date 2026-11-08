//! Order schedules (D3, D16): fixed interval or cron expression.
//!
//! Both give **slots**: the slot due at `now` is the only one fired (missed
//! slots are never replayed). A slot has a number used in the idempotency key
//! (`rec:{order_id}:{rev}:{slot}`), a start time (the job's `created_at`, so
//! double leaders agree, D8) and an end (the next slot: the task TTL, D4).

use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Schedule of a recurring order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// `every <duration>`, in seconds (slots aligned to the Unix epoch).
    Every { seconds: u64 },
    /// Cron expression (5 fields, or 6/7 with seconds/years) and IANA
    /// timezone (default UTC).
    Cron {
        expression: String,
        #[serde(default = "utc")]
        timezone: String,
    },
}

fn utc() -> String {
    "UTC".to_string()
}

/// One firing slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Interval: `floor(now / duration)`; cron: fire time in Unix seconds.
    pub number: i64,
    pub start: DateTime<Utc>,
    /// Start of the next slot.
    pub end: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScheduleError {
    #[error("interval must be at least 1 second")]
    ZeroInterval,
    #[error("invalid cron expression `{0}`: {1}")]
    Cron(String, String),
    #[error("unknown timezone `{0}`")]
    Timezone(String),
    #[error("cron expression `{0}` never fires")]
    Never(String),
}

impl Schedule {
    /// Parse `every 5m` / `every 1h30m` / `every 90s` or a cron expression
    /// (`cron:` prefix optional), e.g. for CLIs.
    pub fn parse(text: &str, timezone: Option<&str>) -> Result<Self, ScheduleError> {
        let text = text.trim();
        let schedule = if let Some(duration) = text.strip_prefix("every ") {
            Schedule::Every {
                seconds: parse_duration(duration.trim()).ok_or(ScheduleError::ZeroInterval)?,
            }
        } else {
            Schedule::Cron {
                expression: text
                    .strip_prefix("cron:")
                    .unwrap_or(text)
                    .trim()
                    .to_string(),
                timezone: timezone.unwrap_or("UTC").to_string(),
            }
        };
        schedule.validate()?;
        Ok(schedule)
    }

    pub fn validate(&self) -> Result<(), ScheduleError> {
        self.slot_at(Utc::now()).map(|_| ())
    }

    /// The slot containing `now`.
    pub fn slot_at(&self, now: DateTime<Utc>) -> Result<Slot, ScheduleError> {
        match self {
            Schedule::Every { seconds } => {
                if *seconds == 0 {
                    return Err(ScheduleError::ZeroInterval);
                }
                let duration = i64::try_from(*seconds).map_err(|_| ScheduleError::ZeroInterval)?;
                let number = now.timestamp().div_euclid(duration);
                let start = Utc
                    .timestamp_opt(number * duration, 0)
                    .single()
                    .unwrap_or(now);
                let end = Utc
                    .timestamp_opt((number + 1) * duration, 0)
                    .single()
                    .unwrap_or(now);
                Ok(Slot { number, start, end })
            }
            Schedule::Cron {
                expression,
                timezone,
            } => {
                let cron = parse_cron(expression)?;
                let tz = chrono_tz::Tz::from_str(timezone)
                    .map_err(|_| ScheduleError::Timezone(timezone.clone()))?;
                let local_now = now.with_timezone(&tz);
                // latest fire time <= now, and the next one after now
                let start = cron
                    .after(&(local_now + chrono::Duration::seconds(1)))
                    .next_back()
                    .map(|t| t.with_timezone(&Utc));
                let end = cron
                    .after(&local_now)
                    .next()
                    .map(|t| t.with_timezone(&Utc))
                    .ok_or_else(|| ScheduleError::Never(expression.clone()))?;
                // before the first fire time ever: the "slot" is the next one,
                // not yet due (callers fire only when start <= now)
                let start = start.unwrap_or(end);
                Ok(Slot {
                    number: start.timestamp(),
                    start,
                    end,
                })
            }
        }
    }

    /// Start of the slot after the one containing `now` (when to wake up).
    pub fn next_fire(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, ScheduleError> {
        let slot = self.slot_at(now)?;
        Ok(if slot.start > now {
            slot.start
        } else {
            slot.end
        })
    }
}

fn parse_cron(expression: &str) -> Result<cron::Schedule, ScheduleError> {
    let fields = expression.split_whitespace().count();
    let full = if fields == 5 {
        format!("0 {expression}")
    } else {
        expression.to_string()
    };
    cron::Schedule::from_str(&full)
        .map_err(|e| ScheduleError::Cron(expression.to_string(), e.to_string()))
}

/// `90`, `90s`, `5m`, `1h30m`, `2d` → seconds (None if 0 or invalid).
pub fn parse_duration(text: &str) -> Option<u64> {
    if let Ok(seconds) = text.parse::<u64>() {
        return (seconds > 0).then_some(seconds);
    }
    let mut total: u64 = 0;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let value: u64 = number.parse().ok()?;
        number.clear();
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        total = total.checked_add(value.checked_mul(unit)?)?;
    }
    if !number.is_empty() {
        return None;
    }
    (total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn interval_slots_align_to_wall_clock() {
        let schedule = Schedule::parse("every 5m", None).unwrap();
        let slot = schedule.slot_at(t("2026-11-04T10:07:30Z")).unwrap();
        assert_eq!(slot.start, t("2026-11-04T10:05:00Z"));
        assert_eq!(slot.end, t("2026-11-04T10:10:00Z"));
        assert_eq!(slot.number, t("2026-11-04T10:05:00Z").timestamp() / 300);
        assert_eq!(
            schedule.next_fire(t("2026-11-04T10:07:30Z")).unwrap(),
            t("2026-11-04T10:10:00Z")
        );
    }

    #[test]
    fn cron_slot_is_latest_fire_time() {
        let schedule = Schedule::parse("30 2 * * *", Some("Europe/Paris")).unwrap();
        // 02:30 Paris = 01:30 UTC in November
        let slot = schedule.slot_at(t("2026-11-04T12:00:00Z")).unwrap();
        assert_eq!(slot.start, t("2026-11-04T01:30:00Z"));
        assert_eq!(slot.end, t("2026-11-05T01:30:00Z"));
        assert_eq!(slot.number, slot.start.timestamp());
        // exactly at the fire time, that slot is due
        let at_fire = schedule.slot_at(t("2026-11-05T01:30:00Z")).unwrap();
        assert_eq!(at_fire.start, t("2026-11-05T01:30:00Z"));
    }

    #[test]
    fn invalid_schedules() {
        assert_eq!(
            Schedule::parse("every 0s", None),
            Err(ScheduleError::ZeroInterval)
        );
        assert!(matches!(
            Schedule::parse("61 * * * *", None),
            Err(ScheduleError::Cron(..))
        ));
        assert!(matches!(
            Schedule::parse("* * * * *", Some("Mars/Base")),
            Err(ScheduleError::Timezone(_))
        ));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("5x"), None);
    }
}
