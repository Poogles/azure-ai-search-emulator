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

pub mod date;
pub mod parser;
pub mod validate;

pub use date::{DateExpr, DateOperand, DatePart, DateRef, DateUnit};
pub use parser::parse_filter;
pub use validate::validate;

use serde_json::{Map, Value};

use crate::storage::resolve_field_path;

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
    pub(crate) fn parse(token: &str) -> Option<Self> {
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

    pub(crate) fn is_ordering(self) -> bool {
        matches!(self, Self::Gt | Self::Ge | Self::Lt | Self::Le)
    }
}

/// Reverses a comparison operator (`a op b` ⇔ `b reverse(op) a`), for
/// leading-`utcdatetime` comparisons (`utcdatetime('...') op field`).
pub(crate) fn reverse_op(op: FilterOp) -> FilterOp {
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
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "startswith" => Some(Self::StartsWith),
            "endswith" => Some(Self::EndsWith),
            "contains" => Some(Self::Contains),
            _ => None,
        }
    }
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
                compare_field_values(&resolve_field_path(fields, field), *op, value)
            }
            FilterExpr::In { field, values } => {
                let resolved = resolve_field_path(fields, field);
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
                flatten_values(&resolve_field_path(fields, field))
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
                compare_filter_values(&actual, *op, value)
            }
            FilterExpr::IsMatch { field, pattern } => {
                let lowered = pattern.to_lowercase();
                flatten_values(&resolve_field_path(fields, field))
                    .iter()
                    .filter_map(|value| value.as_str())
                    .any(|text| text.to_lowercase().contains(lowered.as_str()))
            }
            FilterExpr::IsEmpty { field } => {
                let resolved = resolve_field_path(fields, field);
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
                let resolved = resolve_field_path(fields, field);
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
    debug_assert!(
        matches!(inner, FilterExpr::Compare { .. }),
        "any/all inner must be a Compare on the lambda variable"
    );
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

/// Compares an evaluated [`FilterValue`] (a date-function result) against an
/// expected literal, mirroring [`compare`] without round-tripping through
/// [`Value`]. Non-scalar actuals never occur here and never match.
#[allow(clippy::float_cmp)]
fn compare_filter_values(actual: &FilterValue, op: FilterOp, expected: &FilterValue) -> bool {
    match actual {
        FilterValue::Number(actual_number) => match expected {
            FilterValue::Number(number) => match op {
                FilterOp::Eq => *actual_number == *number,
                FilterOp::Ne => *actual_number != *number,
                FilterOp::Gt => *actual_number > *number,
                FilterOp::Ge => *actual_number >= *number,
                FilterOp::Lt => *actual_number < *number,
                FilterOp::Le => *actual_number <= *number,
            },
            FilterValue::Null => matches!(op, FilterOp::Ne),
            _ => false,
        },
        FilterValue::String(actual_text) => match expected {
            FilterValue::String(text) => match op {
                FilterOp::Eq => actual_text == text,
                FilterOp::Ne => actual_text != text,
                FilterOp::Gt => actual_text > text,
                FilterOp::Ge => actual_text >= text,
                FilterOp::Lt => actual_text < text,
                FilterOp::Le => actual_text <= text,
            },
            FilterValue::Null => matches!(op, FilterOp::Ne),
            _ => false,
        },
        FilterValue::Bool(_) | FilterValue::Null => false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::IndexDefinition;
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
