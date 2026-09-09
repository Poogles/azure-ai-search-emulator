//! OData-style `$filter` parser and evaluator.
//!
//! Supports the operator set required by the supported-operations matrix:
//! `and` / `or` / `not` with parentheses, `eq` / `ne` / `gt` / `ge` / `lt` /
//! `le` on string, numeric, and boolean values, and collection filtering with
//! `any` / `all`. Anything else is rejected with a clear parse error.
//!
//! The parser produces an internal expression tree ([`FilterExpr`]) that is
//! decoupled from the HTTP representation; the service layer validates the
//! tree against the index schema and the search pipeline evaluates it per
//! document.

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

/// A literal value in a filter expression.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
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
    Any {
        field: String,
        inner: Box<FilterExpr>,
    },
    All {
        field: String,
        inner: Box<FilterExpr>,
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
                let Some(actual) = fields.get(field) else {
                    // A missing field compares like `null`.
                    return null_matches(*op, value);
                };
                if actual.is_null() {
                    return null_matches(*op, value);
                }
                if actual.is_array() {
                    // Collection field compared to a scalar: `eq` matches when
                    // any element equals the value, `ne` when no element does.
                    match op {
                        FilterOp::Eq => actual.as_array().is_some_and(|items| {
                            items.iter().any(|item| values_equal(item, value))
                        }),
                        FilterOp::Ne => actual.as_array().is_some_and(|items| {
                            !items.iter().any(|item| values_equal(item, value))
                        }),
                        _ => false,
                    }
                } else {
                    compare(actual, *op, value)
                }
            }
            FilterExpr::Any { field, inner } => fields
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| element_matches(inner, item))),
            FilterExpr::All { field, inner } => fields
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().all(|item| element_matches(inner, item))),
        }
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
        FilterExpr::Any { field, inner } | FilterExpr::All { field, inner } => {
            let field_def = require_filterable(field, definition)?;
            if !is_collection_type(&field_def.field_type) {
                return Err(format!(
                    "Field {field:?} is not a collection; any/all require a collection field."
                ));
            }
            match inner.as_ref() {
                FilterExpr::Compare { .. } => Ok(()),
                _ => Err(format!(
                    "any/all on field {field:?} must contain a single comparison on the lambda variable."
                )),
            }
        }
    }
}

fn require_filterable<'a>(
    field: &str,
    definition: &'a IndexDefinition,
) -> Result<&'a FieldDefinition, String> {
    let field_def = definition
        .field(field)
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
                    if next_ch.is_ascii_alphanumeric() || next_ch == '_' {
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
        let op = self.parse_op()?;
        let value = self.parse_value()?;
        Ok(FilterExpr::Compare {
            field: first,
            op,
            value,
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
            "price in (1, 2)",
            "bogus price eq 5",
            "price eq 'unterminated",
            "price eq 5 5",
        ] {
            assert!(parse_filter(input).is_err(), "expected error for {input:?}");
        }
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
    }
}
