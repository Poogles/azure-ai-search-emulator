//! `OData` filter parser: tokenizes and parses `$filter` strings.

use super::date::{normalize_datetime, DateExpr, DateOperand, DatePart, DateRef, DateUnit};
use super::{reverse_op, FilterExpr, FilterOp, FilterValue, StringFunc};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
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
        if let Some(iso) = self.try_parse_utcdatetime()? {
            return Ok(FilterExpr::DateCompare {
                left: DateOperand::Field(first.clone()),
                op,
                value: FilterValue::String(iso),
            });
        }
        let value = self.parse_value()?;
        Ok(FilterExpr::Compare {
            field: first,
            op,
            value,
        })
    }

    /// If the next tokens are a `utcdatetime('...')` literal, consumes it and
    /// returns the normalized ISO-8601 string; otherwise returns `None`
    /// without consuming.
    fn try_parse_utcdatetime(&mut self) -> Result<Option<String>, String> {
        let is_utcdatetime = matches!(self.peek(), Some(Token::Ident(name)) if name == "utcdatetime")
            && matches!(self.tokens.get(self.pos + 1), Some(Token::LParen));
        if !is_utcdatetime {
            return Ok(None);
        }
        self.next(); // Consume `utcdatetime`.
        self.parse_utcdatetime_literal().map(Some)
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

    /// Expects a specific token, reporting `ctx` (the expected token plus its
    /// context, e.g. `',' in dateadd arguments`) when a different token is
    /// found.
    fn expect_token(&mut self, expected: &Token, ctx: &str) -> Result<(), String> {
        match self.next() {
            Some(token) if token == *expected => Ok(()),
            other => Err(format!(
                "Expected {ctx}, found {}.",
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
            "datepart" => DateOperand::Expr(self.parse_datepart_args()?),
            "dateadd" => DateOperand::Expr(self.parse_dateadd_args()?),
            "datediff" => DateOperand::Expr(self.parse_datediff_args()?),
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

    /// Parses `datepart(part, field)` after `(`: the date part and the field.
    fn parse_datepart_args(&mut self) -> Result<DateExpr, String> {
        let part_name = self.expect_ident("date part")?;
        let part = DatePart::parse(&part_name).ok_or_else(|| {
            format!(
                "Unknown datepart {part_name:?}; supported parts: year, quarter, month, \
                 week, day, hour, minute, second, dayofweek, dayofyear."
            )
        })?;
        self.expect_token(&Token::Comma, "',' in datepart arguments")?;
        let field = self.expect_ident("field name")?;
        self.expect_token(&Token::RParen, "')' to close datepart arguments")?;
        Ok(DateExpr::DatePart { part, field })
    }

    /// Parses `dateadd(unit, interval, field)` after `(`: the unit, an
    /// integer interval, and the field.
    fn parse_dateadd_args(&mut self) -> Result<DateExpr, String> {
        let unit_name = self.expect_ident("date unit")?;
        let unit = DateUnit::parse(&unit_name).ok_or_else(|| {
            format!(
                "Unknown dateadd unit {unit_name:?}; supported units: year, quarter, \
                 month, week, day, hour, minute, second."
            )
        })?;
        self.expect_token(&Token::Comma, "',' in dateadd arguments")?;
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
        self.expect_token(&Token::Comma, "',' in dateadd arguments")?;
        let field = self.expect_ident("field name")?;
        self.expect_token(&Token::RParen, "')' to close dateadd arguments")?;
        Ok(DateExpr::DateAdd {
            unit,
            interval,
            field,
        })
    }

    /// Parses `datediff(unit, start, end)` after `(`: the unit and the two
    /// endpoints (each a field or a `utcdatetime` literal).
    fn parse_datediff_args(&mut self) -> Result<DateExpr, String> {
        let unit_name = self.expect_ident("date unit")?;
        let unit = DateUnit::parse(&unit_name).ok_or_else(|| {
            format!(
                "Unknown datediff unit {unit_name:?}; supported units: year, quarter, \
                 month, week, day, hour, minute, second."
            )
        })?;
        self.expect_token(&Token::Comma, "',' in datediff arguments")?;
        let start = self.parse_date_ref("datediff")?;
        self.expect_token(&Token::Comma, "',' in datediff arguments")?;
        let end = self.parse_date_ref("datediff")?;
        self.expect_token(&Token::RParen, "')' to close datediff arguments")?;
        Ok(DateExpr::DateDiff { unit, start, end })
    }

    /// Parses a comparison right-hand value: a `utcdatetime('...')` literal
    /// (normalized to ISO-8601) or a plain literal value.
    fn parse_value_or_utcdatetime(&mut self) -> Result<FilterValue, String> {
        if let Some(iso) = self.try_parse_utcdatetime()? {
            return Ok(FilterValue::String(iso));
        }
        self.parse_value()
    }

    /// Parses a `datediff` endpoint: a field name or a `utcdatetime('...')`
    /// literal.
    fn parse_date_ref(&mut self, func: &str) -> Result<DateRef, String> {
        if let Some(iso) = self.try_parse_utcdatetime()? {
            return Ok(DateRef::Literal(iso));
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
        self.expect_token(&Token::RParen, "')' to close utcdatetime arguments")?;
        normalize_datetime(&literal).ok_or_else(|| {
            format!(
                "Invalid date {literal:?} in utcdatetime; expected ISO-8601 \
                 (e.g. '2024-01-15T10:30:00Z')."
            )
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
                let mut exprs = self.parse_ismatch_args(name)?;
                // A single field is the bare expression; multiple fields
                // OR together.
                if exprs.len() == 1 {
                    Ok(exprs.pop().unwrap_or_else(|| FilterExpr::IsMatch {
                        field: String::new(),
                        pattern: String::new(),
                    }))
                } else {
                    Ok(FilterExpr::Or(exprs))
                }
            }
            "isempty" | "isnull" => self.parse_isempty_isnull(name),
            _ => Err(format!(
                "Unsupported search function 'search.{name}'; supported functions: \
                 ismatch, ismatchscoring, isempty, isnull."
            )),
        }
    }

    /// Parses `ismatch('pattern', field[, ...])` / `ismatchscoring(...)`
    /// after `(`: the pattern, the field list, and any inert extra options.
    /// Returns one `IsMatch` expression per listed field.
    fn parse_ismatch_args(&mut self, name: &str) -> Result<Vec<FilterExpr>, String> {
        let pattern = match self.next() {
            Some(Token::String(text)) => text,
            other => {
                return Err(format!(
                    "Expected a search pattern string in search.{name}, found {}.",
                    describe_token(other.as_ref())
                ))
            }
        };
        self.expect_token(&Token::Comma, &format!("',' in search.{name} arguments"))?;
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
        self.expect_token(
            &Token::RParen,
            &format!("')' to close search.{name} arguments"),
        )?;
        Ok(fields
            .into_iter()
            .map(|field| FilterExpr::IsMatch {
                field,
                pattern: pattern.clone(),
            })
            .collect())
    }

    /// Parses `isempty(field)` / `isnull(field)` after `(`.
    fn parse_isempty_isnull(&mut self, name: &str) -> Result<FilterExpr, String> {
        let field = self.expect_ident("field name")?;
        self.expect_token(
            &Token::RParen,
            &format!("')' to close search.{name} arguments"),
        )?;
        match name {
            "isempty" => Ok(FilterExpr::IsEmpty { field }),
            _ => Ok(FilterExpr::IsNull { field }),
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
