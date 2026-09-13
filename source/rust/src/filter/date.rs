//! Date/time filter functions (`datepart`, `dateadd`, `datediff`, `utcdatetime`).

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde_json::{Map, Value};

use crate::storage::resolve_field_path;

use super::{flatten_values, FilterValue};

/// A calendar part extracted by `datepart` (e.g. `datepart(year, published)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    DayOfWeek,
    DayOfYear,
}

impl DatePart {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "year" => Some(Self::Year),
            "quarter" => Some(Self::Quarter),
            "month" => Some(Self::Month),
            "week" => Some(Self::Week),
            "day" => Some(Self::Day),
            "hour" => Some(Self::Hour),
            "minute" => Some(Self::Minute),
            "second" => Some(Self::Second),
            "dayofweek" => Some(Self::DayOfWeek),
            "dayofyear" => Some(Self::DayOfYear),
            _ => None,
        }
    }
}

/// A date/time unit for `dateadd` and `datediff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateUnit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateUnit {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "year" => Some(Self::Year),
            "quarter" => Some(Self::Quarter),
            "month" => Some(Self::Month),
            "week" => Some(Self::Week),
            "day" => Some(Self::Day),
            "hour" => Some(Self::Hour),
            "minute" => Some(Self::Minute),
            "second" => Some(Self::Second),
            _ => None,
        }
    }
}

/// A date/time value reference for `datediff`: a field or a literal date.
#[derive(Debug, Clone, PartialEq)]
pub enum DateRef {
    /// A field holding a date/time string.
    Field(String),
    /// A literal date, normalized to fixed-width UTC ISO-8601 at parse time.
    Literal(String),
}

/// A date/time value expression: the left side of a date comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum DateExpr {
    /// `datepart(part, field)`: the calendar part as a number.
    DatePart { part: DatePart, field: String },
    /// `dateadd(unit, interval, field)`: the field's date shifted by
    /// `interval` units, as a normalized ISO-8601 string.
    DateAdd {
        unit: DateUnit,
        interval: i64,
        field: String,
    },
    /// `datediff(unit, start, end)`: the whole units between two dates.
    DateDiff {
        unit: DateUnit,
        start: DateRef,
        end: DateRef,
    },
    /// `utcdatetime('...')`: a literal date, normalized at parse time.
    UtcDateTime(String),
}

/// The left side of a date comparison: a field (compared as a normalized
/// date string) or a date expression.
#[derive(Debug, Clone, PartialEq)]
pub enum DateOperand {
    Field(String),
    Expr(DateExpr),
}

/// Parses a document or literal date/time string into UTC. Accepts full
/// RFC-3339 (`2024-01-15T10:30:00Z`, with offsets and fractional seconds), a
/// date-only value (`2024-01-15`, midnight UTC), and a timezone-less datetime
/// (`2024-01-15T10:30:00`, assumed UTC). Returns `None` when unparseable.
pub(crate) fn parse_datetime(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.to_utc());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc());
    }
    None
}

/// Normalizes a date/time string to fixed-width UTC ISO-8601
/// (`YYYY-MM-DDTHH:MM:SS.sssZ`). Fixed width keeps lexicographic string
/// comparison chronological. Returns `None` when unparseable.
pub(crate) fn normalize_datetime(text: &str) -> Option<String> {
    parse_datetime(text).map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Resolves a field path to its first date/time value, parsed to UTC. Paths
/// through collections resolve to the first parseable element.
fn resolve_datetime(fields: &Map<String, Value>, field: &str) -> Option<DateTime<Utc>> {
    for value in flatten_values(&resolve_field_path(fields, field)) {
        if let Some(text) = value.as_str() {
            if let Some(dt) = parse_datetime(text) {
                return Some(dt);
            }
        }
    }
    None
}

impl DateRef {
    /// Resolves a `datediff` endpoint to UTC.
    fn resolve(&self, fields: &Map<String, Value>) -> Option<DateTime<Utc>> {
        match self {
            DateRef::Field(field) => resolve_datetime(fields, field),
            DateRef::Literal(iso) => parse_datetime(iso),
        }
    }
}

/// Extracts a `datepart` calendar component as a number. All components fit
/// exactly in an `f64` (year, month, day, ...), so the conversion is lossless.
fn datepart_value(part: DatePart, dt: &DateTime<Utc>) -> f64 {
    match part {
        DatePart::Year => f64::from(dt.year()),
        DatePart::Quarter => f64::from(dt.month0() / 3 + 1),
        DatePart::Month => f64::from(dt.month()),
        DatePart::Week => f64::from(dt.iso_week().week()),
        DatePart::Day => f64::from(dt.day()),
        DatePart::Hour => f64::from(dt.hour()),
        DatePart::Minute => f64::from(dt.minute()),
        DatePart::Second => f64::from(dt.second()),
        DatePart::DayOfWeek => f64::from(dt.weekday().num_days_from_sunday()),
        DatePart::DayOfYear => f64::from(dt.ordinal()),
    }
}

/// Shifts a date by `interval` `unit`s, returning the normalized ISO-8601
/// string. Returns `None` on overflow or out-of-range intervals.
fn dateadd_value(unit: DateUnit, interval: i64, dt: &DateTime<Utc>) -> Option<String> {
    let shifted = match unit {
        DateUnit::Year => {
            let months = u32::try_from(interval.abs().checked_mul(12)?).ok()?;
            if interval >= 0 {
                dt.checked_add_months(chrono::Months::new(months))?
            } else {
                dt.checked_sub_months(chrono::Months::new(months))?
            }
        }
        DateUnit::Quarter => {
            let months = u32::try_from(interval.abs().checked_mul(3)?).ok()?;
            if interval >= 0 {
                dt.checked_add_months(chrono::Months::new(months))?
            } else {
                dt.checked_sub_months(chrono::Months::new(months))?
            }
        }
        DateUnit::Month => {
            let months = u32::try_from(interval.abs()).ok()?;
            if interval >= 0 {
                dt.checked_add_months(chrono::Months::new(months))?
            } else {
                dt.checked_sub_months(chrono::Months::new(months))?
            }
        }
        DateUnit::Week => dt.checked_add_signed(chrono::Duration::try_weeks(interval)?)?,
        DateUnit::Day => dt.checked_add_signed(chrono::Duration::try_days(interval)?)?,
        DateUnit::Hour => dt.checked_add_signed(chrono::Duration::try_hours(interval)?)?,
        DateUnit::Minute => dt.checked_add_signed(chrono::Duration::try_minutes(interval)?)?,
        DateUnit::Second => dt.checked_add_signed(chrono::Duration::try_seconds(interval)?)?,
    };
    Some(shifted.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Computes the whole `unit`s between `start` and `end` (truncated toward
/// zero, matching Azure's `datediff`). Calendar units count month/year
/// boundaries; clock units divide the exact duration. Results always fit
/// exactly in an `f64` (chrono's date range spans ~262k years, at most ~8e12
/// seconds, far below the 2^52 exact-integer limit).
fn datediff_value(unit: DateUnit, start: &DateTime<Utc>, end: &DateTime<Utc>) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let as_number = |value: i64| value as f64;
    match unit {
        DateUnit::Year | DateUnit::Quarter | DateUnit::Month => {
            let months = (i64::from(end.year()) - i64::from(start.year())) * 12
                + i64::from(end.month())
                - i64::from(start.month());
            as_number(match unit {
                DateUnit::Year => months.div_euclid(12),
                DateUnit::Quarter => months.div_euclid(3),
                _ => months,
            })
        }
        _ => {
            let duration = *end - *start;
            as_number(match unit {
                DateUnit::Week => duration.num_weeks(),
                DateUnit::Day => duration.num_days(),
                DateUnit::Hour => duration.num_hours(),
                DateUnit::Minute => duration.num_minutes(),
                _ => duration.num_seconds(),
            })
        }
    }
}

impl DateExpr {
    /// Evaluates a date expression against a document: `datepart`/`datediff`
    /// produce a number, `dateadd`/`utcdatetime` a normalized ISO-8601
    /// string. Returns `None` when a referenced date is missing or
    /// unparseable, or the arithmetic overflows.
    pub(crate) fn evaluate(&self, fields: &Map<String, Value>) -> Option<FilterValue> {
        match self {
            DateExpr::DatePart { part, field } => {
                let dt = resolve_datetime(fields, field)?;
                Some(FilterValue::Number(datepart_value(*part, &dt)))
            }
            DateExpr::DateAdd {
                unit,
                interval,
                field,
            } => {
                let dt = resolve_datetime(fields, field)?;
                Some(FilterValue::String(dateadd_value(*unit, *interval, &dt)?))
            }
            DateExpr::DateDiff { unit, start, end } => {
                let start_dt = start.resolve(fields)?;
                let end_dt = end.resolve(fields)?;
                Some(FilterValue::Number(datediff_value(
                    *unit, &start_dt, &end_dt,
                )))
            }
            DateExpr::UtcDateTime(iso) => Some(FilterValue::String(iso.clone())),
        }
    }
}

impl DateOperand {
    /// Evaluates a date-comparison left side: a field normalizes to its
    /// ISO-8601 string, a date expression to its number or string.
    pub(crate) fn evaluate(&self, fields: &Map<String, Value>) -> Option<FilterValue> {
        match self {
            DateOperand::Field(field) => {
                let dt = resolve_datetime(fields, field)?;
                Some(FilterValue::String(
                    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                ))
            }
            DateOperand::Expr(expr) => expr.evaluate(fields),
        }
    }
}

impl DateOperand {
    /// All field names referenced by a date-comparison left side.
    pub(crate) fn referenced_fields(&self) -> Vec<String> {
        match self {
            DateOperand::Field(field)
            | DateOperand::Expr(
                DateExpr::DatePart { field, .. } | DateExpr::DateAdd { field, .. },
            ) => vec![field.clone()],
            DateOperand::Expr(DateExpr::DateDiff { start, end, .. }) => {
                let mut fields = Vec::new();
                for date_ref in [start, end] {
                    if let DateRef::Field(field) = date_ref {
                        fields.push(field.clone());
                    }
                }
                fields
            }
            DateOperand::Expr(DateExpr::UtcDateTime(_)) => Vec::new(),
        }
    }
}
