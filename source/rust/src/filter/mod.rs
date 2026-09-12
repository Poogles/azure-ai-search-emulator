//! OData-style `$filter` parser and evaluator.
//!
//! Supports the operator set required by the supported-operations matrix:
//! `and` / `or` / `not` with parentheses, `eq` / `ne` / `gt` / `ge` / `lt` /
//! `le` / `in` on string, numeric, and boolean values, the string functions
//! `startswith` / `endswith` / `contains`, and collection filtering with
//! `any` / `all`. Anything else is rejected with a clear parse error.
//!
//! The parser produces an internal expression tree ([`FilterExpr`]) that is
//! decoupled from the HTTP representation; the service layer validates the
//! tree against the index schema and the search pipeline evaluates it per
//! document.

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde_json::{Map, Value};

use crate::storage::{FieldDefinition, IndexDefinition};

/// Comparison operators supported in filter expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl FilterOp {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "gt" => Some(Self::Gt),
            "ge" => Some(Self::Ge),
            "lt" => Some(Self::Lt),
            "le" => Some(Self::Le),
            _ => None,
        }
    }

    fn is_ordering(self) -> bool {
        matches!(self, Self::Gt | Self::Ge | Self::Lt | Self::Le)
    }
}

/// Reverses a comparison operator (`a op b` ⇔ `b reverse(op) a`), for
/// leading-`utcdatetime` comparisons (`utcdatetime('...') op field`).
fn reverse_op(op: FilterOp) -> FilterOp {
    match op {
        FilterOp::Eq => FilterOp::Eq,
        FilterOp::Ne => FilterOp::Ne,
        FilterOp::Gt => FilterOp::Lt,
        FilterOp::Ge => FilterOp::Le,
        FilterOp::Lt => FilterOp::Gt,
        FilterOp::Le => FilterOp::Ge,
    }
}

/// A literal value in a filter expression.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
}

/// A string function supported in filter expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringFunc {
    StartsWith,
    EndsWith,
    Contains,
}

impl StringFunc {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "startswith" => Some(Self::StartsWith),
            "endswith" => Some(Self::EndsWith),
            "contains" => Some(Self::Contains),
            _ => None,
        }
    }
}

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
    fn parse(name: &str) -> Option<Self> {
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
    fn parse(name: &str) -> Option<Self> {
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

/// The internal filter expression tree.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
    Compare {
        field: String,
        op: FilterOp,
        value: FilterValue,
    },
    /// `field in (value, ...)` membership test.
    In {
        field: String,
        values: Vec<FilterValue>,
    },
    /// `startswith(field, 'prefix')` / `endswith(field, 'suffix')` /
    /// `contains(field, 'substring')` (ordinal, case-sensitive).
    StringFunc {
        func: StringFunc,
        field: String,
        arg: String,
    },
    Any {
        field: String,
        inner: Box<FilterExpr>,
    },
    All {
        field: String,
        inner: Box<FilterExpr>,
    },
    /// A date comparison: `datepart(...) op value`, `dateadd(...) op value`,
    /// `datediff(...) op value`, `field op utcdatetime('...')`, or
    /// `utcdatetime('...') op field`. The left side evaluates to a number
    /// (`datepart`/`datediff`) or a normalized ISO-8601 string
    /// (`dateadd`/`utcdatetime`/field); unparseable dates and type mismatches
    /// never match.
    DateCompare {
        left: DateOperand,
        op: FilterOp,
        value: FilterValue,
    },
    /// `search.ismatch('pattern', field)`: case-insensitive substring match
    /// of the pattern against the field's string value(s).
    IsMatch {
        field: String,
        pattern: String,
    },
    /// `search.isempty(field)`: the field is missing, null, an empty string,
    /// or an empty array.
    IsEmpty {
        field: String,
    },
    /// `search.isnull(field)`: the field is missing or null.
    IsNull {
        field: String,
    },
}

impl FilterExpr {
    /// Evaluates the expression against a document's field map.
    ///
    /// Type mismatches and missing fields never match (they evaluate to
    /// `false`); they are not errors, matching Azure behaviour where a
    /// document simply does not satisfy the filter.
    #[must_use]
    pub fn matches(&self, fields: &Map<String, Value>) -> bool {
        match self {
            FilterExpr::And(clauses) => clauses.iter().all(|c| c.matches(fields)),
            FilterExpr::Or(clauses) => clauses.iter().any(|c| c.matches(fields)),
            FilterExpr::Not(inner) => !inner.matches(fields),
            FilterExpr::Compare { field, op, value } => {
                compare_field_values(&resolve_paths(fields, field), *op, value)
            }
            FilterExpr::In { field, values } => {
                let resolved = resolve_paths(fields, field);
                if resolved.is_empty() {
                    // A missing field matches only a list containing `null`.
                    return values.iter().any(value_is_null);
                }
                flatten_values(&resolved)
                    .iter()
                    .any(|item| values.iter().any(|value| values_equal(item, value)))
            }
            FilterExpr::StringFunc { func, field, arg } => {
                let is_match = |text: &str| match func {
                    StringFunc::StartsWith => text.starts_with(arg.as_str()),
                    StringFunc::EndsWith => text.ends_with(arg.as_str()),
                    StringFunc::Contains => text.contains(arg.as_str()),
                };
                flatten_values(&resolve_paths(fields, field))
                    .iter()
                    .filter_map(|value| value.as_str())
                    .any(is_match)
            }
            FilterExpr::Any { field, inner } => fields
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| element_matches(inner, item))),
            FilterExpr::All { field, inner } => fields
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().all(|item| element_matches(inner, item))),
            FilterExpr::DateCompare { left, op, value } => {
                let Some(actual) = left.evaluate(fields) else {
                    return false;
                };
                let actual_value = match &actual {
                    FilterValue::Number(n) => Value::from(*n),
                    FilterValue::String(s) => Value::String(s.clone()),
                    _ => return false,
                };
                compare(&actual_value, *op, value)
            }
            FilterExpr::IsMatch { field, pattern } => {
                let lowered = pattern.to_lowercase();
                flatten_values(&resolve_paths(fields, field))
                    .iter()
                    .filter_map(|value| value.as_str())
                    .any(|text| text.to_lowercase().contains(lowered.as_str()))
            }
            FilterExpr::IsEmpty { field } => {
                let resolved = resolve_paths(fields, field);
                if resolved.is_empty() {
                    return true;
                }
                let flat = flatten_values(&resolved);
                flat.is_empty()
                    || flat
                        .iter()
                        .all(|value| value.is_null() || value.as_str() == Some(""))
            }
            FilterExpr::IsNull { field } => {
                let resolved = resolve_paths(fields, field);
                if resolved.is_empty() {
                    return true;
                }
                flatten_values(&resolved)
                    .iter()
                    .any(|value| value.is_null())
            }
        }
    }
}

/// Resolves a field path (`Address/StateProvince`, or a plain field name)
/// against a document's field map, walking into complex-type objects. When a
/// segment resolves to a JSON array (a collection field or a
/// collection-of-complex field), the remaining path is resolved against every
/// element, so a collection-of-complex path yields one value per element. A
/// plain (non-collection) path yields at most one value.
fn resolve_paths<'a>(fields: &'a Map<String, Value>, path: &str) -> Vec<&'a Value> {
    let mut segments = path.split('/');
    let Some(first) = segments.next() else {
        return Vec::new();
    };
    let mut current = match fields.get(first) {
        Some(value) => vec![value],
        None => return Vec::new(),
    };
    for segment in segments {
        let mut next = Vec::new();
        for value in current {
            match value {
                Value::Array(items) => {
                    for item in items {
                        if let Some(sub) = item.as_object().and_then(|o| o.get(segment)) {
                            next.push(sub);
                        }
                    }
                }
                _ => {
                    if let Some(sub) = value.as_object().and_then(|o| o.get(segment)) {
                        next.push(sub);
                    }
                }
            }
        }
        current = next;
    }
    current
}

/// Flattens resolved path values one level: a single collection field becomes
/// its elements; anything else passes through unchanged.
fn flatten_values<'a>(values: &[&'a Value]) -> Vec<&'a Value> {
    if let [Value::Array(items)] = values {
        items.iter().collect()
    } else {
        values.to_vec()
    }
}

/// Evaluates a comparison against the values a field path resolved to:
/// - no values (a missing field or path): compares like `null`;
/// - a single scalar (or explicit `null`): a direct comparison;
/// - otherwise (a collection field, or a path through a collection):
///   `eq`/`in`-style matching when any value equals, `ne` when none does,
///   and ordering operators when any value satisfies them.
fn compare_field_values(values: &[&Value], op: FilterOp, expected: &FilterValue) -> bool {
    if values.is_empty() {
        return null_matches(op, expected);
    }
    if values.len() == 1 && !values[0].is_array() {
        let actual = values[0];
        if actual.is_null() {
            return null_matches(op, expected);
        }
        return compare(actual, op, expected);
    }
    let flat = flatten_values(values);
    if matches!(expected, FilterValue::Null) {
        let has_null = flat.iter().any(|value| value.is_null());
        return match op {
            FilterOp::Eq => has_null,
            FilterOp::Ne => !has_null,
            _ => false,
        };
    }
    match op {
        // Collection field compared to a scalar: `eq` matches when any
        // element equals the value, `ne` when no element does.
        FilterOp::Eq => flat.iter().any(|item| values_equal(item, expected)),
        FilterOp::Ne => !flat.iter().any(|item| values_equal(item, expected)),
        _ => flat
            .iter()
            .any(|item| !item.is_null() && compare(item, op, expected)),
    }
}

fn value_is_null(value: &FilterValue) -> bool {
    matches!(value, FilterValue::Null)
}

/// Whether a `null` (or missing) value satisfies a comparison against
/// `value`.
fn null_matches(op: FilterOp, value: &FilterValue) -> bool {
    match op {
        FilterOp::Eq => value_is_null(value),
        FilterOp::Ne => !value_is_null(value),
        _ => false,
    }
}

/// Evaluates an `any`/`all` inner expression against a single collection
/// element. The inner expression must be a comparison on the lambda variable.
fn element_matches(inner: &FilterExpr, element: &Value) -> bool {
    match inner {
        FilterExpr::Compare { op, value, .. } => {
            if element.is_null() {
                return null_matches(*op, value);
            }
            compare(element, *op, value)
        }
        _ => false,
    }
}

/// Exact float comparison is the correct filter semantics (JSON numbers
/// compare exactly, as in Azure); epsilon comparison would be wrong here.
#[allow(clippy::float_cmp)]
fn compare(actual: &Value, op: FilterOp, expected: &FilterValue) -> bool {
    match expected {
        // `actual` is known to be present and non-null here, so it equals
        // `null` only for `eq` (never) and differs from `null` for `ne`
        // (always). Ordering comparisons against `null` never match.
        FilterValue::Null => matches!(op, FilterOp::Ne),
        FilterValue::String(text) => match actual.as_str() {
            Some(actual_text) => match op {
                FilterOp::Eq => actual_text == text.as_str(),
                FilterOp::Ne => actual_text != text.as_str(),
                FilterOp::Gt => actual_text > text.as_str(),
                FilterOp::Ge => actual_text >= text.as_str(),
                FilterOp::Lt => actual_text < text.as_str(),
                FilterOp::Le => actual_text <= text.as_str(),
            },
            None => false,
        },
        FilterValue::Number(number) => match actual.as_f64() {
            Some(actual_number) => match op {
                FilterOp::Eq => actual_number == *number,
                FilterOp::Ne => actual_number != *number,
                FilterOp::Gt => actual_number > *number,
                FilterOp::Ge => actual_number >= *number,
                FilterOp::Lt => actual_number < *number,
                FilterOp::Le => actual_number <= *number,
            },
            None => false,
        },
        FilterValue::Bool(flag) => match actual.as_bool() {
            Some(actual_bool) => match op {
                FilterOp::Eq => actual_bool == *flag,
                FilterOp::Ne => actual_bool != *flag,
                _ => false,
            },
            None => false,
        },
    }
}

fn values_equal(item: &Value, expected: &FilterValue) -> bool {
    match expected {
        FilterValue::Null => item.is_null(),
        FilterValue::String(text) => item.as_str() == Some(text.as_str()),
        FilterValue::Number(number) => item.as_f64() == Some(*number),
        FilterValue::Bool(flag) => item.as_bool() == Some(*flag),
    }
}

/// Parses a document or literal date/time string into UTC. Accepts full
/// RFC-3339 (`2024-01-15T10:30:00Z`, with offsets and fractional seconds), a
/// date-only value (`2024-01-15`, midnight UTC), and a timezone-less datetime
/// (`2024-01-15T10:30:00`, assumed UTC). Returns `None` when unparseable.
fn parse_datetime(text: &str) -> Option<DateTime<Utc>> {
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
fn normalize_datetime(text: &str) -> Option<String> {
    parse_datetime(text).map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Resolves a field path to its first date/time value, parsed to UTC. Paths
/// through collections resolve to the first parseable element.
fn resolve_datetime(fields: &Map<String, Value>, field: &str) -> Option<DateTime<Utc>> {
    for value in flatten_values(&resolve_paths(fields, field)) {
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
    fn evaluate(&self, fields: &Map<String, Value>) -> Option<FilterValue> {
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
    fn evaluate(&self, fields: &Map<String, Value>) -> Option<FilterValue> {
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

/// Validates a parsed filter expression against an index schema.
///
/// # Errors
///
/// Returns an error string when a referenced field is missing, not marked
/// `filterable`, or used with an incompatible operator (ordering comparisons
/// on booleans or collections).
pub fn validate(expr: &FilterExpr, definition: &IndexDefinition) -> Result<(), String> {
    match expr {
        FilterExpr::And(clauses) | FilterExpr::Or(clauses) => {
            for clause in clauses {
                validate(clause, definition)?;
            }
            Ok(())
        }
        FilterExpr::Not(inner) => validate(inner, definition),
        FilterExpr::Compare { field, op, value } => {
            let field_def = require_filterable(field, definition)?;
            if op.is_ordering() && matches!(value, FilterValue::Bool(_)) {
                return Err(format!(
                    "Filter operator {op:?} is not supported for boolean values in field {field:?}."
                ));
            }
            if op.is_ordering() && is_collection_type(&field_def.field_type) {
                return Err(format!(
                    "Filter operator {op:?} is not supported for collection field {field:?}; \
                     use any/all for collection filtering."
                ));
            }
            Ok(())
        }
        FilterExpr::In { field, .. }
        | FilterExpr::IsEmpty { field }
        | FilterExpr::IsNull { field } => {
            require_filterable(field, definition)?;
            Ok(())
        }
        FilterExpr::StringFunc { func, field, .. } => {
            let field_def = require_filterable(field, definition)?;
            if field_def.field_type != "Edm.String"
                && field_def.field_type != "Edm.Collection(Edm.String)"
            {
                let name = match func {
                    StringFunc::StartsWith => "startswith",
                    StringFunc::EndsWith => "endswith",
                    StringFunc::Contains => "contains",
                };
                return Err(format!(
                    "Filter function {name} on field {field:?} requires a string field; \
                     field type is {:?}.",
                    field_def.field_type
                ));
            }
            Ok(())
        }
        FilterExpr::Any { field, inner } | FilterExpr::All { field, inner } => {
            let field_def = require_filterable(field, definition)?;
            if !is_collection_type(&field_def.field_type) {
                return Err(format!(
                    "Field {field:?} is not a collection; any/all require a collection field."
                ));
            }
            match inner.as_ref() {
                FilterExpr::Compare { field: inner_field, .. } => {
                    if inner_field.contains('/') {
                        return Err(format!(
                            "any/all on field {field:?} must contain a single comparison on the \
                              lambda variable; paths into the element (e.g. {inner_field:?}) are \
                              not supported."
                        ));
                    }
                    Ok(())
                }
                _ => Err(format!(
                    "any/all on field {field:?} must contain a single comparison on the lambda variable."
                )),
            }
        }
        FilterExpr::DateCompare { left, .. } => {
            for field in left.referenced_fields() {
                let field_def = require_filterable(&field, definition)?;
                if field_def.field_type != "Edm.DateTimeOffset" {
                    return Err(format!(
                        "Date filter on field {field:?} requires an Edm.DateTimeOffset field; \
                         field type is {:?}.",
                        field_def.field_type
                    ));
                }
            }
            Ok(())
        }
        FilterExpr::IsMatch { field, .. } => {
            let field_def = require_filterable(field, definition)?;
            if field_def.field_type != "Edm.String"
                && field_def.field_type != "Edm.Collection(Edm.String)"
            {
                return Err(format!(
                    "Filter function search.ismatch on field {field:?} requires a string field; \
                     field type is {:?}.",
                    field_def.field_type
                ));
            }
            Ok(())
        }
    }
}

impl DateOperand {
    /// All field names referenced by a date-comparison left side.
    fn referenced_fields(&self) -> Vec<String> {
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

fn require_filterable<'a>(
    field: &str,
    definition: &'a IndexDefinition,
) -> Result<&'a FieldDefinition, String> {
    let field_def = definition
        .field_path(field)
        .ok_or_else(|| format!("Filter references unknown field {field:?}."))?;
    if !field_def.filterable {
        return Err(format!(
            "Field {field:?} is not filterable; mark it \"filterable\": true in the index schema."
        ));
    }
    Ok(field_def)
}

fn is_collection_type(field_type: &str) -> bool {
    field_type.starts_with("Edm.Collection(")
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    String(String),
    Number(f64),
    LParen,
    RParen,
    Dot,
    Comma,
    Colon,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        match ch {
            ' ' | '\t' | '\r' | '\n' => {}
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            '.' => tokens.push(Token::Dot),
            ',' => tokens.push(Token::Comma),
            ':' => tokens.push(Token::Colon),
            '\'' => {
                let mut text = String::new();
                loop {
                    match chars.next() {
                        None => {
                            return Err(format!(
                                "Unterminated string literal in filter at position {start}."
                            ))
                        }
                        Some((_, '\'')) => {
                            // Either an escaped quote ('') or the closing quote.
                            if chars.peek().is_some_and(|&(_, next)| next == '\'') {
                                chars.next();
                                text.push('\'');
                                continue;
                            }
                            break;
                        }
                        Some((_, other)) => text.push(other),
                    }
                }
                tokens.push(Token::String(text));
            }
            c if c.is_ascii_digit() || (c == '-' && is_number_start(chars.peek())) => {
                let mut text = String::new();
                if c == '-' {
                    text.push('-');
                } else {
                    text.push(c);
                }
                let mut is_float = false;
                while let Some(&(_, next_ch)) = chars.peek() {
                    if next_ch.is_ascii_digit() {
                        chars.next();
                        text.push(next_ch);
                    } else if next_ch == '.' && !is_float {
                        is_float = true;
                        chars.next();
                        text.push(next_ch);
                    } else if (next_ch == 'e' || next_ch == 'E')
                        && text
                            .chars()
                            .filter(|t| *t != 'e' && *t != 'E')
                            .any(|t| t.is_ascii_digit())
                    {
                        chars.next();
                        text.push(next_ch);
                        if chars
                            .peek()
                            .is_some_and(|&(_, sign)| sign == '+' || sign == '-')
                        {
                            if let Some((_, sign)) = chars.next() {
                                text.push(sign);
                            }
                        }
                    } else {
                        break;
                    }
                }
                let number: f64 = text
                    .parse()
                    .map_err(|_| format!("Invalid numeric literal {text:?} in filter."))?;
                tokens.push(Token::Number(number));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut text = String::new();
                text.push(c);
                while let Some(&(_, next_ch)) = chars.peek() {
                    // `/` continues an identifier so complex-type field paths
                    // (`Address/StateProvince`) parse as a single field name.
                    if next_ch.is_ascii_alphanumeric() || next_ch == '_' || next_ch == '/' {
                        chars.next();
                        text.push(next_ch);
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(text));
            }
            other => {
                return Err(format!(
                    "Unexpected character {other:?} in filter expression."
                ))
            }
        }
    }
    Ok(tokens)
}

fn is_number_start(peek: Option<&(usize, char)>) -> bool {
    matches!(peek, Some((_, ch)) if ch.is_ascii_digit())
}

/// Splits a collection-lambda field reference (`Tags/any`, `Tags/all`) into
/// the collection field and the lambda operator. Returns `None` for plain
/// field names and complex paths that do not end in `/any` or `/all`.
fn split_lambda_field(name: &str) -> Option<(String, String)> {
    if let Some(field) = name.strip_suffix("/any") {
        return Some((field.to_owned(), "any".to_owned()));
    }
    if let Some(field) = name.strip_suffix("/all") {
        return Some((field.to_owned(), "all".to_owned()));
    }
    None
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, String> {
        match self.next() {
            Some(Token::Ident(name)) => Ok(name),
            other => Err(format!(
                "Expected {what}, found {}.",
                describe_token(other.as_ref())
            )),
        }
    }

    fn parse_expression(&mut self) -> Result<FilterExpr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<FilterExpr, String> {
        let first = self.parse_and()?;
        if !self.peek_ident_is("or") {
            return Ok(first);
        }
        let mut clauses = vec![first];
        while self.peek_ident_is("or") {
            self.next();
            clauses.push(self.parse_and()?);
        }
        Ok(FilterExpr::Or(clauses))
    }

    fn parse_and(&mut self) -> Result<FilterExpr, String> {
        let first = self.parse_not()?;
        if !self.peek_ident_is("and") {
            return Ok(first);
        }
        let mut clauses = vec![first];
        while self.peek_ident_is("and") {
            self.next();
            clauses.push(self.parse_not()?);
        }
        Ok(FilterExpr::And(clauses))
    }

    fn parse_not(&mut self) -> Result<FilterExpr, String> {
        if self.peek_ident_is("not") {
            self.next();
            return Ok(FilterExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<FilterExpr, String> {
        if matches!(self.peek(), Some(Token::LParen)) {
            self.next();
            let expr = self.parse_or()?;
            match self.next() {
                Some(Token::RParen) => Ok(expr),
                other => Err(format!(
                    "Expected ')' after filter sub-expression, found {}.",
                    describe_token(other.as_ref())
                )),
            }
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<FilterExpr, String> {
        // Collection filtering: `field any var op value` / `field all var op value`.
        let first = self.expect_ident("field name")?;
        // OData lambda syntax: `field/any(var: body)` / `field/all(var: body)`.
        // The tokenizer folds `field/any` into a single identifier (because `/`
        // continues an identifier), so detect the lambda operator as a suffix.
        if let Some((field, kind)) = split_lambda_field(&first) {
            if matches!(self.peek(), Some(Token::LParen)) {
                self.next();
                let _variable = self.expect_ident("lambda variable")?;
                match self.next() {
                    Some(Token::Colon) => {}
                    other => {
                        return Err(format!(
                            "Expected ':' after lambda variable, found {}.",
                            describe_token(other.as_ref())
                        ))
                    }
                }
                let inner = self.parse_comparison()?;
                match self.next() {
                    Some(Token::RParen) => {}
                    other => {
                        return Err(format!(
                            "Expected ')' after lambda body, found {}.",
                            describe_token(other.as_ref())
                        ))
                    }
                }
                return match kind.as_str() {
                    "any" => Ok(FilterExpr::Any {
                        field,
                        inner: Box::new(inner),
                    }),
                    _ => Ok(FilterExpr::All {
                        field,
                        inner: Box::new(inner),
                    }),
                };
            }
        }
        if self.peek_ident_is("any") || self.peek_ident_is("all") {
            let Some(Token::Ident(kind)) = self.next() else {
                return Err("Expected 'any' or 'all' keyword.".to_owned());
            };
            let variable = self.expect_ident("lambda variable")?;
            let op = self.parse_op()?;
            let value = self.parse_value()?;
            let inner = FilterExpr::Compare {
                field: variable,
                op,
                value,
            };
            return match kind.as_str() {
                "any" => Ok(FilterExpr::Any {
                    field: first,
                    inner: Box::new(inner),
                }),
                _ => Ok(FilterExpr::All {
                    field: first,
                    inner: Box::new(inner),
                }),
            };
        }
        // Date functions: `datepart(...)`, `dateadd(...)`, `datediff(...)`,
        // or a leading `utcdatetime('...')`.
        if matches!(
            first.as_str(),
            "datepart" | "dateadd" | "datediff" | "utcdatetime"
        ) && matches!(self.peek(), Some(Token::LParen))
        {
            return self.parse_date_compare(&first);
        }
        // Search functions: `search.ismatch(...)`, `search.ismatchscoring(...)`,
        // `search.isempty(...)`, `search.isnull(...)`. The tokenizer splits
        // `search.ismatch` into an identifier, a dot, and an identifier.
        if first == "search" && matches!(self.peek(), Some(Token::Dot)) {
            self.next(); // Consume '.'.
            let func = self.expect_ident("search function name")?;
            if matches!(self.peek(), Some(Token::LParen)) {
                return self.parse_search_function(&func);
            }
            return Err(format!(
                "Expected '(' after 'search.{func}'; supported search functions: \
                 ismatch, ismatchscoring, isempty, isnull."
            ));
        }
        // Function calls: `startswith(field, 'prefix')`, `endswith(field,
        // 'suffix')`, `contains(field, 'substring')`.
        if matches!(self.peek(), Some(Token::LParen)) {
            return self.parse_function_call(&first);
        }
        // Membership test: `field in (value, ...)`.
        if self.peek_ident_is("in") {
            self.next();
            return self.parse_in_list(&first);
        }
        let op = self.parse_op()?;
        // A trailing `utcdatetime('...')`: `field op utcdatetime('...')`.
        if let Some(Token::Ident(name)) = self.peek() {
            if name == "utcdatetime" && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                self.next(); // Consume `utcdatetime`.
                return self.parse_field_vs_utcdatetime(&first, op);
            }
        }
        let value = self.parse_value()?;
        Ok(FilterExpr::Compare {
            field: first,
            op,
            value,
        })
    }

    /// Parses a string-function call after the function name: `(field,
    /// 'literal')`.
    fn parse_function_call(&mut self, name: &str) -> Result<FilterExpr, String> {
        let Some(func) = StringFunc::parse(name) else {
            return Err(format!(
                "Unsupported filter function {name:?}; supported functions: \
                 startswith, endswith, contains."
            ));
        };
        self.next(); // Consume '('.
        let field = self.expect_ident("field name")?;
        match self.next() {
            Some(Token::Comma) => {}
            other => {
                return Err(format!(
                    "Expected ',' after filter function field name, found {}.",
                    describe_token(other.as_ref())
                ))
            }
        }
        let arg = match self.next() {
            Some(Token::String(text)) => text,
            other => {
                return Err(format!(
                    "Expected a string literal as the filter function argument, found {}.",
                    describe_token(other.as_ref())
                ))
            }
        };
        match self.next() {
            Some(Token::RParen) => {}
            other => {
                return Err(format!(
                    "Expected ')' after filter function argument, found {}.",
                    describe_token(other.as_ref())
                ))
            }
        }
        Ok(FilterExpr::StringFunc { func, field, arg })
    }

    /// Expects a `,` separator in a function argument list.
    fn expect_comma(&mut self, func: &str) -> Result<(), String> {
        match self.next() {
            Some(Token::Comma) => Ok(()),
            other => Err(format!(
                "Expected ',' in {func} arguments, found {}.",
                describe_token(other.as_ref())
            )),
        }
    }

    /// Expects a `)` closing a function argument list.
    fn expect_rparen(&mut self, func: &str) -> Result<(), String> {
        match self.next() {
            Some(Token::RParen) => Ok(()),
            other => Err(format!(
                "Expected ')' to close {func} arguments, found {}.",
                describe_token(other.as_ref())
            )),
        }
    }

    /// Parses a date comparison after the function name (`datepart`,
    /// `dateadd`, `datediff`, or a leading `utcdatetime`), with `(` peeked:
    /// `(args) op value`.
    fn parse_date_compare(&mut self, name: &str) -> Result<FilterExpr, String> {
        // A leading `utcdatetime('...') op field` consumes its own parens.
        if name == "utcdatetime" {
            let iso = self.parse_utcdatetime_literal()?;
            let op = self.parse_op()?;
            let field = self.expect_ident("field name")?;
            return Ok(FilterExpr::DateCompare {
                left: DateOperand::Field(field),
                op: reverse_op(op),
                value: FilterValue::String(iso),
            });
        }
        self.next(); // Consume '('.
        let left = match name {
            "datepart" => {
                let part_name = self.expect_ident("date part")?;
                let part = DatePart::parse(&part_name).ok_or_else(|| {
                    format!(
                        "Unknown datepart {part_name:?}; supported parts: year, quarter, month, \
                         week, day, hour, minute, second, dayofweek, dayofyear."
                    )
                })?;
                self.expect_comma("datepart")?;
                let field = self.expect_ident("field name")?;
                self.expect_rparen("datepart")?;
                DateOperand::Expr(DateExpr::DatePart { part, field })
            }
            "dateadd" => {
                let unit_name = self.expect_ident("date unit")?;
                let unit = DateUnit::parse(&unit_name).ok_or_else(|| {
                    format!(
                        "Unknown dateadd unit {unit_name:?}; supported units: year, quarter, \
                         month, week, day, hour, minute, second."
                    )
                })?;
                self.expect_comma("dateadd")?;
                let interval = match self.next() {
                    Some(Token::Number(n)) if n.fract() == 0.0 => {
                        // `as` saturates on overflow; the `try_from` below
                        // rejects the saturated value as out of range.
                        #[allow(clippy::cast_possible_truncation)]
                        let as_i128 = n as i128;
                        i64::try_from(as_i128).map_err(|_| {
                            format!("dateadd interval {n} is out of range; expected an integer.")
                        })?
                    }
                    other => {
                        return Err(format!(
                            "Expected an integer interval in dateadd, found {}.",
                            describe_token(other.as_ref())
                        ))
                    }
                };
                self.expect_comma("dateadd")?;
                let field = self.expect_ident("field name")?;
                self.expect_rparen("dateadd")?;
                DateOperand::Expr(DateExpr::DateAdd {
                    unit,
                    interval,
                    field,
                })
            }
            "datediff" => {
                let unit_name = self.expect_ident("date unit")?;
                let unit = DateUnit::parse(&unit_name).ok_or_else(|| {
                    format!(
                        "Unknown datediff unit {unit_name:?}; supported units: year, quarter, \
                         month, week, day, hour, minute, second."
                    )
                })?;
                self.expect_comma("datediff")?;
                let start = self.parse_date_ref("datediff")?;
                self.expect_comma("datediff")?;
                let end = self.parse_date_ref("datediff")?;
                self.expect_rparen("datediff")?;
                DateOperand::Expr(DateExpr::DateDiff { unit, start, end })
            }
            _ => {
                return Err(format!(
                    "Unsupported date function {name:?}; supported functions: \
                     datepart, dateadd, datediff, utcdatetime."
                ));
            }
        };
        let op = self.parse_op()?;
        let value = self.parse_value_or_utcdatetime()?;
        Ok(FilterExpr::DateCompare { left, op, value })
    }

    /// Parses a comparison right-hand value: a `utcdatetime('...')` literal
    /// (normalized to ISO-8601) or a plain literal value.
    fn parse_value_or_utcdatetime(&mut self) -> Result<FilterValue, String> {
        if let Some(Token::Ident(name)) = self.peek() {
            if name == "utcdatetime" && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                self.next(); // Consume `utcdatetime`.
                let iso = self.parse_utcdatetime_literal()?;
                return Ok(FilterValue::String(iso));
            }
        }
        self.parse_value()
    }

    /// Parses a `datediff` endpoint: a field name or a `utcdatetime('...')`
    /// literal.
    fn parse_date_ref(&mut self, func: &str) -> Result<DateRef, String> {
        if let Some(Token::Ident(name)) = self.peek() {
            if name == "utcdatetime" && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen))
            {
                self.next(); // Consume `utcdatetime`.
                let iso = self.parse_utcdatetime_literal()?;
                return Ok(DateRef::Literal(iso));
            }
        }
        Ok(DateRef::Field(
            self.expect_ident(&format!("{func} date field"))?,
        ))
    }

    /// Parses a `utcdatetime('...')` literal after the function name, with
    /// `(` peeked, returning the normalized ISO-8601 string.
    fn parse_utcdatetime_literal(&mut self) -> Result<String, String> {
        self.next(); // Consume '('.
        let literal = match self.next() {
            Some(Token::String(text)) => text,
            other => {
                return Err(format!(
                    "Expected a date string literal in utcdatetime, found {}.",
                    describe_token(other.as_ref())
                ))
            }
        };
        self.expect_rparen("utcdatetime")?;
        normalize_datetime(&literal).ok_or_else(|| {
            format!(
                "Invalid date {literal:?} in utcdatetime; expected ISO-8601 \
                 (e.g. '2024-01-15T10:30:00Z')."
            )
        })
    }

    /// Parses `field op utcdatetime('...')` after the field name and operator.
    fn parse_field_vs_utcdatetime(
        &mut self,
        field: &str,
        op: FilterOp,
    ) -> Result<FilterExpr, String> {
        let iso = self.parse_utcdatetime_literal()?;
        Ok(FilterExpr::DateCompare {
            left: DateOperand::Field(field.to_owned()),
            op,
            value: FilterValue::String(iso),
        })
    }

    /// Parses a `search.*` function call after the function name, with `(`
    /// peeked: `search.ismatch('pattern', field)`,
    /// `search.ismatchscoring('pattern', field)`, `search.isempty(field)`,
    /// `search.isnull(field)`.
    fn parse_search_function(&mut self, name: &str) -> Result<FilterExpr, String> {
        self.next(); // Consume '('.
        match name {
            "ismatch" | "ismatchscoring" => {
                let pattern = match self.next() {
                    Some(Token::String(text)) => text,
                    other => {
                        return Err(format!(
                            "Expected a search pattern string in search.{name}, found {}.",
                            describe_token(other.as_ref())
                        ))
                    }
                };
                self.expect_comma(&format!("search.{name}"))?;
                // The field list is a field name or a comma-separated string
                // of field names (the documented Azure form).
                let mut fields = Vec::new();
                match self.next() {
                    Some(Token::Ident(field)) => fields.push(field),
                    Some(Token::String(list)) => {
                        for field in list.split(',') {
                            let field = field.trim();
                            if field.is_empty() {
                                return Err(format!(
                                    "Empty field name in search.{name} field list {list:?}."
                                ));
                            }
                            fields.push(field.to_owned());
                        }
                    }
                    other => {
                        return Err(format!(
                            "Expected a field name in search.{name}, found {}.",
                            describe_token(other.as_ref())
                        ))
                    }
                }
                // Extra parameters (query type, search mode) are accepted but
                // inert.
                while matches!(self.peek(), Some(Token::Comma)) {
                    self.next();
                    match self.next() {
                        Some(Token::String(_) | Token::Ident(_)) => {}
                        other => {
                            return Err(format!(
                                "Expected a string in search.{name} options, found {}.",
                                describe_token(other.as_ref())
                            ))
                        }
                    }
                }
                self.expect_rparen(&format!("search.{name}"))?;
                let mut exprs: Vec<FilterExpr> = fields
                    .into_iter()
                    .map(|field| FilterExpr::IsMatch {
                        field,
                        pattern: pattern.clone(),
                    })
                    .collect();
                if exprs.len() == 1 {
                    Ok(exprs.pop().unwrap_or_else(|| FilterExpr::IsMatch {
                        field: String::new(),
                        pattern,
                    }))
                } else {
                    Ok(FilterExpr::Or(exprs))
                }
            }
            "isempty" => {
                let field = self.expect_ident("field name")?;
                self.expect_rparen("search.isempty")?;
                Ok(FilterExpr::IsEmpty { field })
            }
            "isnull" => {
                let field = self.expect_ident("field name")?;
                self.expect_rparen("search.isnull")?;
                Ok(FilterExpr::IsNull { field })
            }
            _ => Err(format!(
                "Unsupported search function 'search.{name}'; supported functions: \
                 ismatch, ismatchscoring, isempty, isnull."
            )),
        }
    }

    /// Parses an `in` value list after the field name and `in` keyword:
    /// `(value, ...)`.
    fn parse_in_list(&mut self, field: &str) -> Result<FilterExpr, String> {
        match self.next() {
            // Consume '(' (the `in` keyword was already consumed).
            Some(Token::LParen) => {}
            other => {
                return Err(format!(
                    "Expected '(' after 'in', found {}.",
                    describe_token(other.as_ref())
                ))
            }
        }
        let mut values = Vec::new();
        loop {
            values.push(self.parse_value()?);
            match self.next() {
                Some(Token::Comma) => {}
                Some(Token::RParen) => break,
                other => {
                    return Err(format!(
                        "Expected ',' or ')' in 'in' value list, found {}.",
                        describe_token(other.as_ref())
                    ))
                }
            }
        }
        Ok(FilterExpr::In {
            field: field.to_owned(),
            values,
        })
    }

    fn parse_op(&mut self) -> Result<FilterOp, String> {
        match self.next() {
            Some(Token::Ident(name)) => FilterOp::parse(&name).ok_or_else(|| {
                format!("Expected a filter operator (eq, ne, gt, ge, lt, le), found {name:?}.")
            }),
            other => Err(format!(
                "Expected a filter operator (eq, ne, gt, ge, lt, le), found {}.",
                describe_token(other.as_ref())
            )),
        }
    }

    fn parse_value(&mut self) -> Result<FilterValue, String> {
        match self.next() {
            Some(Token::String(text)) => Ok(FilterValue::String(text)),
            Some(Token::Number(number)) => Ok(FilterValue::Number(number)),
            Some(Token::Ident(name)) => match name.as_str() {
                "true" => Ok(FilterValue::Bool(true)),
                "false" => Ok(FilterValue::Bool(false)),
                "null" => Ok(FilterValue::Null),
                other => Err(format!(
                    "Expected a filter value (string, number, true, false, null), found {other:?}."
                )),
            },
            other => Err(format!(
                "Expected a filter value (string, number, true, false, null), found {}.",
                describe_token(other.as_ref())
            )),
        }
    }

    fn peek_ident_is(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(text)) if text == name)
    }
}

fn describe_token(token: Option<&Token>) -> String {
    match token {
        None => "end of expression".to_owned(),
        Some(Token::Ident(name)) => format!("identifier {name:?}"),
        Some(Token::String(text)) => format!("string {text:?}"),
        Some(Token::Number(number)) => format!("number {number}"),
        Some(Token::LParen) => "'('".to_owned(),
        Some(Token::RParen) => "')'".to_owned(),
        Some(Token::Dot) => "'.'".to_owned(),
        Some(Token::Comma) => "','".to_owned(),
        Some(Token::Colon) => "':'".to_owned(),
    }
}

/// Parses a `$filter` expression into an internal expression tree.
///
/// # Errors
///
/// Returns an error string describing the first syntax problem found.
pub fn parse_filter(input: &str) -> Result<FilterExpr, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err("Filter expression is empty.".to_owned());
    }
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_expression()?;
    if let Some(token) = parser.peek() {
        return Err(format!(
            "Unexpected {} after filter expression.",
            describe_token(Some(token))
        ));
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition() -> IndexDefinition {
        IndexDefinition::from_json(json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true, "filterable": true},
                {"name": "title", "type": "Edm.String", "filterable": true},
                {"name": "price", "type": "Edm.Double", "filterable": true},
                {"name": "active", "type": "Edm.Boolean", "filterable": true},
                {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true},
                {"name": "locked", "type": "Edm.String"}
            ]
        }))
        .unwrap_or_else(|e| panic!("valid definition: {e}"))
    }

    fn doc(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn parse_ok(input: &str) -> FilterExpr {
        parse_filter(input).unwrap_or_else(|e| panic!("parse failed for {input:?}: {e}"))
    }

    fn matches(expr: &FilterExpr, pairs: &[(&str, Value)]) -> bool {
        expr.matches(&doc(pairs))
    }

    #[test]
    fn parses_and_evaluates_comparisons() {
        assert!(matches(&parse_ok("price gt 10"), &[("price", json!(11))]));
        assert!(!matches(&parse_ok("price gt 10"), &[("price", json!(10))]));
        assert!(matches(&parse_ok("price ge 10"), &[("price", json!(10))]));
        assert!(matches(&parse_ok("price lt 10"), &[("price", json!(9.5))]));
        assert!(matches(&parse_ok("price le 10"), &[("price", json!(10))]));
        assert!(matches(
            &parse_ok("title eq 'hello'"),
            &[("title", json!("hello"))]
        ));
        assert!(!matches(
            &parse_ok("title eq 'hello'"),
            &[("title", json!("Hello"))]
        ));
        assert!(matches(
            &parse_ok("title ne 'hello'"),
            &[("title", json!("world"))]
        ));
        assert!(matches(
            &parse_ok("active eq true"),
            &[("active", json!(true))]
        ));
        assert!(matches(
            &parse_ok("active ne false"),
            &[("active", json!(true))]
        ));
    }

    #[test]
    fn missing_fields_and_type_mismatches_do_not_match() {
        assert!(!matches(&parse_ok("price gt 10"), &[]));
        assert!(!matches(
            &parse_ok("price gt 10"),
            &[("price", json!("ten"))]
        ));
        assert!(matches(&parse_ok("title eq null"), &[]));
        assert!(matches(
            &parse_ok("title ne null"),
            &[("title", json!("x"))]
        ));
        assert!(!matches(&parse_ok("title ne null"), &[]));
    }

    #[test]
    fn parses_logical_operators_and_precedence() {
        // `and` binds tighter than `or`.
        let expr = parse_ok("a eq 1 or b eq 2 and c eq 3");
        assert!(matches!(expr, FilterExpr::Or(_)));
        assert!(matches(
            &expr,
            &[("a", json!(1)), ("b", json!(0)), ("c", json!(0)),]
        ));
        assert!(!matches(
            &expr,
            &[("a", json!(0)), ("b", json!(2)), ("c", json!(0)),]
        ));
        let expr = parse_ok("not (price gt 10)");
        assert!(matches(&expr, &[("price", json!(5))]));
        assert!(!matches(&expr, &[("price", json!(50))]));
        let expr = parse_ok("(title eq 'a' or title eq 'b') and active eq true");
        assert!(matches(
            &expr,
            &[("title", json!("b")), ("active", json!(true)),]
        ));
        assert!(!matches(
            &expr,
            &[("title", json!("b")), ("active", json!(false)),]
        ));
    }

    #[test]
    fn parses_collection_any_all() {
        assert!(matches(
            &parse_ok("tags any x eq 'red'"),
            &[("tags", json!(["blue", "red"]))]
        ));
        assert!(!matches(
            &parse_ok("tags any x eq 'red'"),
            &[("tags", json!(["blue"]))]
        ));
        assert!(matches(
            &parse_ok("tags all x ne 'banned'"),
            &[("tags", json!(["a", "b"]))]
        ));
        assert!(!matches(
            &parse_ok("tags all x ne 'banned'"),
            &[("tags", json!(["a", "banned"]))]
        ));
        // Empty collection: any is false, all is vacuously true.
        assert!(!matches(
            &parse_ok("tags any x eq 'red'"),
            &[("tags", json!([]))]
        ));
        assert!(matches(
            &parse_ok("tags all x ne 'banned'"),
            &[("tags", json!([]))]
        ));
    }

    #[test]
    fn parses_odata_lambda_any_all() {
        // OData lambda syntax: `field/any(var: body)` / `field/all(var: body)`.
        assert!(matches(
            &parse_ok("tags/any(t: t eq 'red')"),
            &[("tags", json!(["blue", "red"]))]
        ));
        assert!(!matches(
            &parse_ok("tags/any(t: t eq 'red')"),
            &[("tags", json!(["blue"]))]
        ));
        assert!(matches(
            &parse_ok("tags/all(t: t ne 'banned')"),
            &[("tags", json!(["a", "b"]))]
        ));
        assert!(!matches(
            &parse_ok("tags/all(t: t ne 'banned')"),
            &[("tags", json!(["a", "banned"]))]
        ));
        // The lambda variable name is arbitrary and need not match the field.
        assert!(matches(
            &parse_ok("tags/any(x: x eq 'red')"),
            &[("tags", json!(["red"]))]
        ));
        // Numeric and boolean bodies.
        assert!(matches(
            &parse_ok("tags/any(t: t gt 2)"),
            &[("tags", json!([1, 3]))]
        ));
        assert!(matches(
            &parse_ok("tags/all(t: t eq true)"),
            &[("tags", json!([true, true]))]
        ));
        // A lambda operator only applies when followed by '('.
        assert!(parse_filter("tags/any").is_err());
        assert!(parse_filter("tags/any(t eq 'red')").is_err());
        assert!(parse_filter("tags/any(t: t eq 'red'").is_err());
    }

    #[test]
    fn scalar_eq_on_collection_uses_any_element_semantics() {
        assert!(matches(
            &parse_ok("tags eq 'red'"),
            &[("tags", json!(["blue", "red"]))]
        ));
        assert!(!matches(
            &parse_ok("tags eq 'red'"),
            &[("tags", json!(["blue"]))]
        ));
        assert!(matches(
            &parse_ok("tags ne 'red'"),
            &[("tags", json!(["blue"]))]
        ));
    }

    #[test]
    fn string_escapes_and_literals() {
        assert!(matches(
            &parse_ok("title eq 'it''s'"),
            &[("title", json!("it's"))]
        ));
        assert!(matches(&parse_ok("price gt -5"), &[("price", json!(-1))]));
    }

    #[test]
    fn rejects_invalid_syntax() {
        for input in [
            "",
            "   ",
            "price",
            "price eq",
            "eq 5",
            "price = 5",
            "price eqq 5",
            "(price eq 5",
            "price eq 5)",
            "price eq 5 and",
            "price in 5",
            "price in (5",
            "price in (5,)",
            "price in ()",
            "bogus price eq 5",
            "price eq 'unterminated",
            "price eq 5 5",
            "nonsense(title, 'x')",
            "startswith(title)",
            "startswith(title, 5)",
        ] {
            assert!(parse_filter(input).is_err(), "expected error for {input:?}");
        }
    }

    #[test]
    fn parses_and_evaluates_in_operator() {
        assert!(matches(
            &parse_ok("price in (5, 10, 15)"),
            &[("price", json!(10))]
        ));
        assert!(!matches(
            &parse_ok("price in (5, 10, 15)"),
            &[("price", json!(7))]
        ));
        assert!(matches(
            &parse_ok("title in ('a', 'b')"),
            &[("title", json!("b"))]
        ));
        assert!(!matches(
            &parse_ok("title in ('a', 'b')"),
            &[("title", json!("c"))]
        ));
        assert!(matches(
            &parse_ok("active in (true, false)"),
            &[("active", json!(false))]
        ));
        // Collections: any element in the list matches.
        assert!(matches(
            &parse_ok("tags in ('red', 'blue')"),
            &[("tags", json!(["green", "blue"]))]
        ));
        assert!(!matches(
            &parse_ok("tags in ('red', 'blue')"),
            &[("tags", json!(["green"]))]
        ));
        // Missing fields match only lists containing null.
        assert!(matches(&parse_ok("title in ('a', null)"), &[]));
        assert!(!matches(&parse_ok("title in ('a', 'b')"), &[]));
        // Combines with logical operators.
        assert!(matches(
            &parse_ok("price in (1, 2) or title eq 'x'"),
            &[("title", json!("x")), ("price", json!(9))]
        ));
    }

    #[test]
    fn parses_and_evaluates_string_functions() {
        assert!(matches(
            &parse_ok("startswith(title, 'hel')"),
            &[("title", json!("hello world"))]
        ));
        assert!(!matches(
            &parse_ok("startswith(title, 'hel')"),
            &[("title", json!("say hello"))]
        ));
        assert!(matches(
            &parse_ok("endswith(title, 'rld')"),
            &[("title", json!("hello world"))]
        ));
        assert!(!matches(
            &parse_ok("endswith(title, 'rld')"),
            &[("title", json!("worldly"))]
        ));
        assert!(matches(
            &parse_ok("contains(title, 'lo wo')"),
            &[("title", json!("hello world"))]
        ));
        assert!(!matches(
            &parse_ok("contains(title, 'lo wo')"),
            &[("title", json!("hello"))]
        ));
        // Matching is ordinal and case-sensitive.
        assert!(!matches(
            &parse_ok("startswith(title, 'HEL')"),
            &[("title", json!("hello"))]
        ));
        // Collections: any matching element satisfies the function.
        assert!(matches(
            &parse_ok("contains(tags, 'ed')"),
            &[("tags", json!(["green", "red"]))]
        ));
        assert!(!matches(
            &parse_ok("contains(tags, 'ed')"),
            &[("tags", json!(["blue"]))]
        ));
        // Missing fields and non-strings never match.
        assert!(!matches(&parse_ok("contains(title, 'x')"), &[]));
        assert!(!matches(
            &parse_ok("startswith(price, '1')"),
            &[("price", json!(10))]
        ));
    }

    #[test]
    fn validation_checks_schema() {
        let definition = definition();
        // Unknown field.
        assert!(validate(&parse_ok("missing eq 1"), &definition).is_err());
        // Not filterable.
        assert!(validate(&parse_ok("locked eq 1"), &definition).is_err());
        // Ordering on boolean.
        assert!(validate(&parse_ok("active gt true"), &definition).is_err());
        // Ordering on collection.
        assert!(validate(&parse_ok("tags gt 'a'"), &definition).is_err());
        // any/all on non-collection.
        assert!(validate(&parse_ok("title any x eq 'a'"), &definition).is_err());
        // Valid.
        assert!(validate(&parse_ok("price gt 1 and tags any x eq 'a'"), &definition).is_ok());
        // `in` on a filterable field is valid; on unknown/non-filterable
        // fields it is rejected like any other comparison.
        assert!(validate(&parse_ok("price in (1, 2)"), &definition).is_ok());
        assert!(validate(&parse_ok("tags in ('a', 'b')"), &definition).is_ok());
        assert!(validate(&parse_ok("missing in (1, 2)"), &definition).is_err());
        assert!(validate(&parse_ok("locked in (1, 2)"), &definition).is_err());
        // String functions on string fields are valid.
        assert!(validate(&parse_ok("startswith(title, 'a')"), &definition).is_ok());
        assert!(validate(&parse_ok("contains(tags, 'a')"), &definition).is_ok());
        // String functions on non-string fields are rejected.
        assert!(validate(&parse_ok("startswith(price, '1')"), &definition).is_err());
        assert!(validate(&parse_ok("endswith(active, 'x')"), &definition).is_err());
        assert!(validate(&parse_ok("contains(missing, 'x')"), &definition).is_err());
    }

    fn hotels_definition() -> IndexDefinition {
        IndexDefinition::from_json(json!({
            "name": "hotels",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {
                    "name": "Address",
                    "type": "Edm.ComplexType",
                    "fields": [
                        {"name": "City", "type": "Edm.String", "filterable": true},
                        {"name": "StateProvince", "type": "Edm.String", "filterable": true},
                        {"name": "Country", "type": "Edm.String"}
                    ]
                }
            ]
        }))
        .unwrap_or_else(|e| panic!("valid definition: {e}"))
    }

    #[test]
    fn parses_and_evaluates_complex_field_paths() {
        let expr = parse_ok("Address/StateProvince eq 'FL' and Address/City eq 'Miami'");
        assert!(matches(
            &expr,
            &[("Address", json!({"City": "Miami", "StateProvince": "FL"}))]
        ));
        assert!(!matches(
            &expr,
            &[("Address", json!({"City": "Miami", "StateProvince": "WA"}))]
        ));
        // A missing path segment compares like `null`.
        assert!(!matches(&expr, &[("Address", json!({"City": "Miami"}))]));
        assert!(!matches(&expr, &[]));
        assert!(matches(
            &parse_ok("Address/Country eq null"),
            &[("Address", json!({"City": "Miami"}))]
        ));
    }

    #[test]
    fn validation_checks_complex_field_paths() {
        let definition = hotels_definition();
        // Valid nested path on filterable subfields.
        assert!(validate(
            &parse_ok("Address/StateProvince eq 'FL' and Address/City eq 'Miami'"),
            &definition
        )
        .is_ok());
        // Unknown nested path.
        assert!(validate(&parse_ok("Address/Missing eq 'x'"), &definition).is_err());
        // Non-filterable subfield.
        assert!(validate(&parse_ok("Address/Country eq 'USA'"), &definition).is_err());
        // Path through a non-complex field.
        assert!(validate(&parse_ok("id/City eq 'x'"), &definition).is_err());
        // Paths into a lambda element are rejected explicitly.
        assert!(validate(&parse_ok("tags/any(t: t/City eq 'x')"), &definition).is_err());
    }

    fn collection_complex_definition() -> IndexDefinition {
        IndexDefinition::from_json(json!({
            "name": "hotels",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {
                    "name": "Rooms",
                    "type": "Edm.Collection(Edm.ComplexType)",
                    "fields": [
                        {"name": "Type", "type": "Edm.String", "filterable": true},
                        {"name": "Rate", "type": "Edm.Double", "filterable": true}
                    ]
                }
            ]
        }))
        .unwrap_or_else(|e| panic!("valid definition: {e}"))
    }

    #[test]
    fn parses_and_evaluates_collection_of_complex_paths() {
        // A direct path through a collection-of-complex field matches when any
        // element satisfies the comparison.
        let expr = parse_ok("Rooms/Type eq 'suite'");
        assert!(matches(
            &expr,
            &[("Rooms", json!([{"Type": "standard"}, {"Type": "suite"}]))]
        ));
        assert!(!matches(&expr, &[("Rooms", json!([{"Type": "standard"}]))]));
        assert!(matches(
            &parse_ok("Rooms/Type ne 'suite'"),
            &[("Rooms", json!([{"Type": "standard"}]))]
        ));
        // Ordering through a collection is existential.
        assert!(matches(
            &parse_ok("Rooms/Rate gt 100"),
            &[("Rooms", json!([{"Rate": 50}, {"Rate": 150}]))]
        ));
        assert!(!matches(
            &parse_ok("Rooms/Rate gt 100"),
            &[("Rooms", json!([{"Rate": 50}]))]
        ));
        // String functions and `in` through a collection.
        assert!(matches(
            &parse_ok("startswith(Rooms/Type, 'sui')"),
            &[("Rooms", json!([{"Type": "suite"}]))]
        ));
        assert!(matches(
            &parse_ok("Rooms/Type in ('suite', 'loft')"),
            &[("Rooms", json!([{"Type": "suite"}]))]
        ));
        assert!(!matches(
            &parse_ok("Rooms/Type in ('suite', 'loft')"),
            &[("Rooms", json!([{"Type": "standard"}]))]
        ));
        // Validation accepts filterable collection-of-complex subfield paths.
        let definition = collection_complex_definition();
        assert!(validate(&parse_ok("Rooms/Type eq 'suite'"), &definition).is_ok());
        assert!(validate(&parse_ok("Rooms/Rate gt 100"), &definition).is_ok());
    }

    fn dated_definition() -> IndexDefinition {
        IndexDefinition::from_json(json!({
            "name": "articles",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "filterable": true},
                {"name": "body", "type": "Edm.String", "filterable": true},
                {"name": "published", "type": "Edm.DateTimeOffset", "filterable": true},
                {"name": "archived", "type": "Edm.DateTimeOffset", "filterable": true},
                {"name": "price", "type": "Edm.Double", "filterable": true}
            ]
        }))
        .unwrap_or_else(|e| panic!("valid definition: {e}"))
    }

    #[test]
    fn parses_and_evaluates_datepart() {
        // 2024-03-15 is a Friday (dayofweek 5), ISO week 11, day 75 of leap 2024.
        let doc = [("published", json!("2024-03-15T10:30:45Z"))];
        assert!(matches(
            &parse_ok("datepart(year, published) eq 2024"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datepart(quarter, published) eq 1"),
            &doc
        ));
        assert!(matches(&parse_ok("datepart(month, published) eq 3"), &doc));
        assert!(matches(&parse_ok("datepart(week, published) eq 11"), &doc));
        assert!(matches(&parse_ok("datepart(day, published) eq 15"), &doc));
        assert!(matches(&parse_ok("datepart(hour, published) eq 10"), &doc));
        assert!(matches(
            &parse_ok("datepart(minute, published) eq 30"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datepart(second, published) eq 45"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datepart(dayofweek, published) eq 5"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datepart(dayofyear, published) eq 75"),
            &doc
        ));
        assert!(!matches(
            &parse_ok("datepart(year, published) eq 2023"),
            &doc
        ));
        // Missing or unparseable dates never match.
        assert!(!matches(
            &parse_ok("datepart(year, published) eq 2024"),
            &[]
        ));
        assert!(!matches(
            &parse_ok("datepart(year, published) eq 2024"),
            &[("published", json!("not a date"))]
        ));
    }

    #[test]
    fn parses_and_evaluates_dateadd() {
        let doc = [("published", json!("2024-03-15T10:30:00Z"))];
        assert!(matches(
            &parse_ok("dateadd(day, 1, published) gt utcdatetime('2024-03-15T10:30:00Z')"),
            &doc
        ));
        assert!(matches(
            &parse_ok("dateadd(month, -1, published) eq utcdatetime('2024-02-15T10:30:00Z')"),
            &doc
        ));
        assert!(matches(
            &parse_ok("dateadd(year, 1, published) gt utcdatetime('2025-01-01T00:00:00Z')"),
            &doc
        ));
        assert!(!matches(
            &parse_ok("dateadd(day, 1, published) lt utcdatetime('2024-03-15T10:30:00Z')"),
            &doc
        ));
        // Missing dates never match.
        assert!(!matches(
            &parse_ok("dateadd(day, 1, published) gt utcdatetime('2024-01-01T00:00:00Z')"),
            &[]
        ));
    }

    #[test]
    fn parses_and_evaluates_datediff() {
        let doc = [
            ("published", json!("2024-03-15T00:00:00Z")),
            ("archived", json!("2024-03-10T00:00:00Z")),
        ];
        assert!(matches(
            &parse_ok("datediff(day, archived, published) eq 5"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datediff(day, published, archived) eq -5"),
            &doc
        ));
        assert!(matches(
            &parse_ok("datediff(hour, archived, published) eq 120"),
            &doc
        ));
        assert!(matches(
            &parse_ok(
                "datediff(month, archived, published) eq 0 and datediff(year, archived, published) eq 0"
            ),
            &doc
        ));
        // Literal endpoints.
        assert!(matches(
            &parse_ok("datediff(day, utcdatetime('2024-03-01T00:00:00Z'), published) eq 14"),
            &doc
        ));
        // Missing dates never match.
        assert!(!matches(
            &parse_ok("datediff(day, archived, published) eq 5"),
            &[("published", json!("2024-03-15T00:00:00Z"))]
        ));
    }

    #[test]
    fn parses_and_evaluates_utcdatetime_comparisons() {
        let doc = [("published", json!("2024-06-01T12:00:00Z"))];
        assert!(matches(
            &parse_ok("published gt utcdatetime('2024-01-01T00:00:00Z')"),
            &doc
        ));
        assert!(!matches(
            &parse_ok("published lt utcdatetime('2024-01-01T00:00:00Z')"),
            &doc
        ));
        // Leading utcdatetime with a reversed operator.
        assert!(matches(
            &parse_ok("utcdatetime('2024-01-01T00:00:00Z') lt published"),
            &doc
        ));
        assert!(matches(
            &parse_ok("utcdatetime('2024-06-01T12:00:00Z') eq published"),
            &doc
        ));
        // Timezone offsets normalize to UTC for comparison.
        assert!(matches(
            &parse_ok("published eq utcdatetime('2024-06-01T14:00:00+02:00')"),
            &doc
        ));
        // Missing dates never match.
        assert!(!matches(
            &parse_ok("published gt utcdatetime('2024-01-01T00:00:00Z')"),
            &[]
        ));
    }

    #[test]
    fn parses_and_evaluates_search_functions() {
        let doc = [("title", json!("Azure Search Basics"))];
        // `search.ismatch` is case-insensitive substring matching.
        assert!(matches(&parse_ok("search.ismatch('azure', title)"), &doc));
        assert!(matches(&parse_ok("search.ismatch('SEARCH', title)"), &doc));
        assert!(!matches(&parse_ok("search.ismatch('solr', title)"), &doc));
        // The documented multi-field string form fans out to an OR.
        let doc = [("title", json!("hello")), ("body", json!("azure world"))];
        assert!(matches(
            &parse_ok("search.ismatch('azure', 'title,body')"),
            &doc
        ));
        assert!(!matches(&parse_ok("search.ismatch('azure', title)"), &doc));
        // `search.ismatchscoring` is an alias for filtering.
        assert!(matches(
            &parse_ok("search.ismatchscoring('azure', body)"),
            &doc
        ));
        // Missing fields never match.
        assert!(!matches(&parse_ok("search.ismatch('azure', title)"), &[]));
    }

    #[test]
    fn parses_and_evaluates_isempty_isnull() {
        assert!(matches(&parse_ok("search.isempty(title)"), &[]));
        assert!(matches(
            &parse_ok("search.isempty(title)"),
            &[("title", json!(null))]
        ));
        assert!(matches(
            &parse_ok("search.isempty(title)"),
            &[("title", json!(""))]
        ));
        assert!(matches(
            &parse_ok("search.isempty(title)"),
            &[("title", json!([]))]
        ));
        assert!(!matches(
            &parse_ok("search.isempty(title)"),
            &[("title", json!("x"))]
        ));
        assert!(matches(&parse_ok("search.isnull(title)"), &[]));
        assert!(matches(
            &parse_ok("search.isnull(title)"),
            &[("title", json!(null))]
        ));
        assert!(!matches(
            &parse_ok("search.isnull(title)"),
            &[("title", json!(""))]
        ));
        assert!(!matches(
            &parse_ok("search.isnull(title)"),
            &[("title", json!("x"))]
        ));
    }

    #[test]
    fn rejects_invalid_date_and_search_syntax() {
        for input in [
            "datepart(year published) eq 2024",
            "datepart(century, published) eq 21",
            "datepart(year) eq 2024",
            "dateadd(day, 1.5, published) gt utcdatetime('2024-01-01T00:00:00Z')",
            "dateadd(fortnight, 1, published) eq utcdatetime('2024-01-01T00:00:00Z')",
            "datediff(day, published) eq 5",
            "published gt utcdatetime('not a date')",
            "published gt utcdatetime(5)",
            "search.ismatch(title)",
            "search.ismatch('azure')",
            "search.unknownfunc(title)",
            "search.isempty()",
            "search.isnull(title, body)",
        ] {
            assert!(parse_filter(input).is_err(), "expected error for {input:?}");
        }
    }

    #[test]
    fn validation_checks_date_and_search_fields() {
        let definition = dated_definition();
        assert!(validate(&parse_ok("datepart(year, published) eq 2024"), &definition).is_ok());
        assert!(validate(
            &parse_ok("published gt utcdatetime('2024-01-01T00:00:00Z')"),
            &definition
        )
        .is_ok());
        assert!(validate(&parse_ok("search.ismatch('a', title)"), &definition).is_ok());
        assert!(validate(&parse_ok("search.isempty(title)"), &definition).is_ok());
        assert!(validate(&parse_ok("search.isnull(title)"), &definition).is_ok());
        // Date functions on non-date fields are rejected.
        assert!(validate(&parse_ok("datepart(year, title) eq 2024"), &definition).is_err());
        assert!(validate(
            &parse_ok("price gt utcdatetime('2024-01-01T00:00:00Z')"),
            &definition
        )
        .is_err());
        // `search.ismatch` on a non-string field is rejected.
        assert!(validate(&parse_ok("search.ismatch('a', price)"), &definition).is_err());
        // Unknown fields are rejected like any other comparison.
        assert!(validate(&parse_ok("search.isempty(missing)"), &definition).is_err());
        assert!(validate(&parse_ok("datepart(year, missing) eq 2024"), &definition).is_err());
    }
}
