//! Cron expression parser and constrained natural-language schedule parser.
//!
//! This module provides:
//! - Deterministic cron expression validation and normalization
//! - Constrained natural-language phrase parsing (grammar-based, not LLM)
//! - Next-occurrence calculation
//!
//! Supported natural-language patterns:
//! - `every N seconds`
//! - `every N minutes/hours`
//! - `every day at HH:MM`
//! - `every <weekday> at HH:MM`

use anyhow::Result;
use chrono::{Datelike, Duration, Timelike};
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub struct CronExpression {
    pub interval_seconds: Option<u32>,
    pub minute: CronField,
    pub hour: CronField,
    pub day_of_month: CronField,
    pub month: CronField,
    pub day_of_week: CronField,
    pub original: String,
}

impl std::fmt::Display for CronExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(interval) = self.interval_seconds {
            return write!(f, "every {} seconds", interval);
        }
        write!(
            f,
            "{} {} {} {} {}",
            self.minute, self.hour, self.day_of_month, self.month, self.day_of_week
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CronField {
    Any,
    Exact(u32),
    Range(u32, u32),
    Step(u32, u32),
    List(Vec<CronField>),
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        match self {
            CronField::Any => true,
            CronField::Exact(v) => *v == value,
            CronField::Range(s, e) => value >= *s && value <= *e,
            CronField::Step(start, step) => value >= *start && (value - start) % step == 0,
            CronField::List(items) => items.iter().any(|i| i.matches(value)),
        }
    }
}

impl std::fmt::Display for CronField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CronField::Any => write!(f, "*"),
            CronField::Exact(v) => write!(f, "{}", v),
            CronField::Range(s, e) => write!(f, "{}-{}", s, e),
            CronField::Step(v, s) => write!(f, "{}/{}", v, s),
            CronField::List(items) => {
                let strs: Vec<String> = items.iter().map(|i| i.to_string()).collect();
                write!(f, "{}", strs.join(","))
            }
        }
    }
}

#[derive(Debug)]
pub enum ScheduleParseError {
    InvalidCron(String),
    AmbiguousPhrase(String),
    UnsupportedPhrase(String),
}

impl std::fmt::Display for ScheduleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleParseError::InvalidCron(msg) => write!(f, "Invalid cron expression: {}", msg),
            ScheduleParseError::AmbiguousPhrase(msg) => {
                write!(
                    f,
                    "Ambiguous schedule phrase: {}. Please use explicit cron syntax.",
                    msg
                )
            }
            ScheduleParseError::UnsupportedPhrase(msg) => {
                write!(f, "Unsupported schedule phrase: {}", msg)
            }
        }
    }
}

impl std::error::Error for ScheduleParseError {}

pub fn parse_schedule(input: &str) -> Result<CronExpression, ScheduleParseError> {
    let trimmed = input.trim();

    if trimmed.contains('/')
        || trimmed.contains(',')
        || trimmed.contains('-')
        || trimmed.split_whitespace().count() == 5
    {
        parse_cron_expression(trimmed)
    } else {
        parse_natural_language(trimmed)
    }
}

fn parse_cron_expression(input: &str) -> Result<CronExpression, ScheduleParseError> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(ScheduleParseError::InvalidCron(format!(
            "Expected 5 fields, got {}",
            parts.len()
        )));
    }

    let minute = parse_cron_field(parts[0], 0, 59)?;
    let hour = parse_cron_field(parts[1], 0, 23)?;
    let day_of_month = parse_cron_field(parts[2], 1, 31)?;
    let month = parse_cron_field(parts[3], 1, 12)?;
    let day_of_week = parse_cron_field(parts[4], 0, 6)?;

    Ok(CronExpression {
        interval_seconds: None,
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
        original: input.to_string(),
    })
}

fn parse_cron_field(
    input: &str,
    min_val: u32,
    max_val: u32,
) -> Result<CronField, ScheduleParseError> {
    if input == "*" {
        return Ok(CronField::Any);
    }

    if input.contains(',') {
        let parts: Vec<CronField> = input
            .split(',')
            .map(|p| parse_cron_field(p.trim(), min_val, max_val))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(CronField::List(parts));
    }

    if input.contains('/') {
        let parts: Vec<&str> = input.split('/').collect();
        if parts.len() != 2 {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Invalid step syntax: {}",
                input
            )));
        }
        let start = if parts[0] == "*" {
            min_val
        } else {
            parts[0].parse().map_err(|e| {
                ScheduleParseError::InvalidCron(format!("Invalid step start '{}': {}", parts[0], e))
            })?
        };
        let step: u32 = parts[1].parse().map_err(|e| {
            ScheduleParseError::InvalidCron(format!("Invalid step value '{}': {}", parts[1], e))
        })?;
        if step == 0 {
            return Err(ScheduleParseError::InvalidCron(
                "Step value cannot be zero".to_string(),
            ));
        }
        if start < min_val || start > max_val {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Step start {} out of range [{}, {}]",
                start, min_val, max_val
            )));
        }
        return Ok(CronField::Step(start, step));
    }

    if input.contains('-') {
        let parts: Vec<&str> = input.split('-').collect();
        if parts.len() != 2 {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Invalid range syntax: {}",
                input
            )));
        }
        let start: u32 = parts[0].parse().map_err(|e| {
            ScheduleParseError::InvalidCron(format!("Invalid range start '{}': {}", parts[0], e))
        })?;
        let end: u32 = parts[1].parse().map_err(|e| {
            ScheduleParseError::InvalidCron(format!("Invalid range end '{}': {}", parts[1], e))
        })?;
        if start < min_val || start > max_val || end < min_val || end > max_val {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Range {}-{} out of bounds [{}, {}]",
                start, end, min_val, max_val
            )));
        }
        if start > end {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Range start {} > end {}",
                start, end
            )));
        }
        return Ok(CronField::Range(start, end));
    }

    let val: u32 = input.parse().map_err(|e| {
        ScheduleParseError::InvalidCron(format!("Invalid field value '{}': {}", input, e))
    })?;
    if val < min_val || val > max_val {
        return Err(ScheduleParseError::InvalidCron(format!(
            "Value {} out of range [{}, {}]",
            val, min_val, max_val
        )));
    }
    Ok(CronField::Exact(val))
}

fn parse_natural_language(input: &str) -> Result<CronExpression, ScheduleParseError> {
    let lower = input.to_lowercase();

    let every_interval_re = Regex::new(
        r"^every\s+(\d+)\s+(second|seconds|minute|minutes|hour|hours)$",
    )
    .map_err(|_| ScheduleParseError::UnsupportedPhrase("Regex compilation failed".to_string()))?;

    if let Some(caps) = every_interval_re.captures(&lower) {
        let n: u32 = caps[1].parse().unwrap();
        let unit = &caps[2];
        if n == 0 {
            return Err(ScheduleParseError::InvalidCron(
                "Interval cannot be zero".to_string(),
            ));
        }
        if unit.starts_with("second") {
            return Ok(CronExpression {
                interval_seconds: Some(n),
                minute: CronField::Any,
                hour: CronField::Any,
                day_of_month: CronField::Any,
                month: CronField::Any,
                day_of_week: CronField::Any,
                original: input.to_string(),
            });
        }
        if unit.starts_with("minute") {
            if n > 59 {
                return Err(ScheduleParseError::InvalidCron(format!(
                    "Minute interval {} exceeds maximum of 59",
                    n
                )));
            }
            return Ok(CronExpression {
                interval_seconds: None,
                minute: CronField::Step(0, n),
                hour: CronField::Any,
                day_of_month: CronField::Any,
                month: CronField::Any,
                day_of_week: CronField::Any,
                original: input.to_string(),
            });
        }
        if unit.starts_with("hour") {
            if n > 23 {
                return Err(ScheduleParseError::InvalidCron(format!(
                    "Hour interval {} exceeds maximum of 23",
                    n
                )));
            }
            return Ok(CronExpression {
                interval_seconds: None,
                minute: CronField::Exact(0),
                hour: CronField::Step(0, n),
                day_of_month: CronField::Any,
                month: CronField::Any,
                day_of_week: CronField::Any,
                original: input.to_string(),
            });
        }
    }

    let weekday_re = Regex::new(
        r"^every\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\s+at\s+(\d{1,2}):(\d{2})$",
    )
    .map_err(|_| ScheduleParseError::UnsupportedPhrase("Regex compilation failed".to_string()))?;

    if let Some(caps) = weekday_re.captures(&lower) {
        let weekday_str = &caps[1];
        let hour: u32 = caps[2].parse().unwrap();
        let minute: u32 = caps[3].parse().unwrap();

        if hour > 23 || minute > 59 {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Invalid time {:02}:{:02}",
                hour, minute
            )));
        }

        let dow = match weekday_str {
            "sunday" => 0,
            "monday" => 1,
            "tuesday" => 2,
            "wednesday" => 3,
            "thursday" => 4,
            "friday" => 5,
            "saturday" => 6,
            _ => unreachable!(),
        };

        return Ok(CronExpression {
            interval_seconds: None,
            minute: CronField::Exact(minute),
            hour: CronField::Exact(hour),
            day_of_month: CronField::Any,
            month: CronField::Any,
            day_of_week: CronField::Exact(dow),
            original: input.to_string(),
        });
    }

    let daily_re = Regex::new(r"^every\s+day\s+at\s+(\d{1,2}):(\d{2})$").map_err(|_| {
        ScheduleParseError::UnsupportedPhrase("Regex compilation failed".to_string())
    })?;

    if let Some(caps) = daily_re.captures(&lower) {
        let hour: u32 = caps[1].parse().unwrap();
        let minute: u32 = caps[2].parse().unwrap();

        if hour > 23 || minute > 59 {
            return Err(ScheduleParseError::InvalidCron(format!(
                "Invalid time {:02}:{:02}",
                hour, minute
            )));
        }

        return Ok(CronExpression {
            interval_seconds: None,
            minute: CronField::Exact(minute),
            hour: CronField::Exact(hour),
            day_of_month: CronField::Any,
            month: CronField::Any,
            day_of_week: CronField::Any,
            original: input.to_string(),
        });
    }

    if lower.starts_with("every ") {
        return Err(ScheduleParseError::UnsupportedPhrase(format!(
            "'{}' is not a supported schedule phrase. Supported patterns: 'every N seconds/minutes/hours', 'every day at HH:MM', 'every <weekday> at HH:MM'",
            input
        )));
    }

    Err(ScheduleParseError::InvalidCron(format!(
        "Not a recognized cron expression (expected 5 space-separated fields)"
    )))
}

pub fn next_occurrence(
    cron: &CronExpression,
    after: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(interval_secs) = cron.interval_seconds {
        let candidate = after + Duration::seconds(interval_secs as i64);
        return candidate.with_nanosecond(0).or(Some(candidate));
    }

    let mut candidate = after + Duration::minutes(1);
    candidate = candidate
        .with_second(0)
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(candidate);

    let max_iterations = 366 * 24 * 60;
    for _ in 0..max_iterations {
        if matches_cron(candidate, cron) {
            return Some(candidate);
        }
        candidate = candidate + Duration::minutes(1);
    }
    None
}

fn matches_cron(dt: chrono::DateTime<chrono::Utc>, cron: &CronExpression) -> bool {
    if !cron.minute.matches(dt.minute() as u32) {
        return false;
    }
    if !cron.hour.matches(dt.hour() as u32) {
        return false;
    }
    if !cron.month.matches(dt.month() as u32) {
        return false;
    }
    if !cron.day_of_month.matches(dt.day() as u32) {
        return false;
    }
    if !cron
        .day_of_week
        .matches(dt.weekday().num_days_from_sunday())
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_seconds_supported() {
        let cron = parse_schedule("every 5 seconds").unwrap();
        assert_eq!(cron.interval_seconds, Some(5));
    }

    #[test]
    fn test_parse_cron_every_5_minutes() {
        let cron = parse_schedule("every 5 minutes").unwrap();
        assert_eq!(cron.minute, CronField::Step(0, 5));
        assert_eq!(cron.hour, CronField::Any);
    }

    #[test]
    fn test_parse_cron_every_2_hours() {
        let cron = parse_schedule("every 2 hours").unwrap();
        assert_eq!(cron.minute, CronField::Exact(0));
        assert_eq!(cron.hour, CronField::Step(0, 2));
    }

    #[test]
    fn test_parse_cron_every_day_at_0900() {
        let cron = parse_schedule("every day at 09:00").unwrap();
        assert_eq!(cron.minute, CronField::Exact(0));
        assert_eq!(cron.hour, CronField::Exact(9));
        assert_eq!(cron.day_of_week, CronField::Any);
    }

    #[test]
    fn test_parse_cron_every_monday_at_1430() {
        let cron = parse_schedule("every monday at 14:30").unwrap();
        assert_eq!(cron.minute, CronField::Exact(30));
        assert_eq!(cron.hour, CronField::Exact(14));
        assert_eq!(cron.day_of_week, CronField::Exact(1));
    }

    #[test]
    fn test_parse_explicit_cron() {
        let cron = parse_schedule("*/5 * * * *").unwrap();
        assert_eq!(cron.minute, CronField::Step(0, 5));
    }

    #[test]
    fn test_parse_invalid_cron() {
        let result = parse_schedule("*/5 * *");
        assert!(matches!(result, Err(ScheduleParseError::InvalidCron(_))));
    }

    #[test]
    fn test_parse_unsupported_phrase() {
        let result = parse_schedule("every year on christmas");
        assert!(matches!(
            result,
            Err(ScheduleParseError::UnsupportedPhrase(_))
        ));
    }

    #[test]
    fn test_next_occurrence_basic() {
        let cron = parse_schedule("every 5 minutes").unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 12, 10, 7, 30).unwrap();
        let next = next_occurrence(&cron, base).unwrap();
        assert_eq!(next.minute(), 10);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn test_cron_field_display() {
        assert_eq!(CronField::Any.to_string(), "*");
        assert_eq!(CronField::Exact(5).to_string(), "5");
        assert_eq!(CronField::Range(1, 5).to_string(), "1-5");
        assert_eq!(CronField::Step(0, 5).to_string(), "0/5");
    }

    #[test]
    fn test_cron_expression_display() {
        let cron = CronExpression {
            interval_seconds: None,
            minute: CronField::Step(0, 5),
            hour: CronField::Any,
            day_of_month: CronField::Any,
            month: CronField::Any,
            day_of_week: CronField::Any,
            original: "*/5 * * * *".to_string(),
        };
        assert_eq!(cron.to_string(), "0/5 * * * *");
    }

    #[test]
    fn test_second_interval_occurrence() {
        let cron = parse_schedule("every 10 seconds").unwrap();
        let base = Utc.with_ymd_and_hms(2026, 4, 12, 10, 7, 30).unwrap();
        let next = next_occurrence(&cron, base).unwrap();
        assert_eq!(next.second(), 40);
    }

    #[test]
    fn test_zero_interval_rejected() {
        let result = parse_schedule("every 0 minutes");
        assert!(matches!(result, Err(ScheduleParseError::InvalidCron(_))));
    }

    #[test]
    fn test_parse_cron_list() {
        let cron = parse_schedule("0,15,30,45 * * * *").unwrap();
        assert!(matches!(cron.minute, CronField::List(_)));
    }

    #[test]
    fn test_parse_cron_range() {
        let cron = parse_schedule("0 9-17 * * 1-5").unwrap();
        assert_eq!(cron.hour, CronField::Range(9, 17));
        assert_eq!(cron.day_of_week, CronField::Range(1, 5));
    }
}
