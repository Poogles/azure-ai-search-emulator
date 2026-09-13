//! Synonym maps: Solr-format rule parsing and validation.

use serde::Serialize;
use serde_json::Value;

use crate::error::ApiError;

/// One parsed Solr synonym rule: `inputs` (matched query terms) rewrite to
/// `outputs` (additional indexed/query terms). For `a,b,c` every term is both
/// an input and an output; for `a => b` only `a` is an input. Multi-word
/// sides contribute each word separately; matching and expansion are
/// case-insensitive (terms are analyzed with the query field's analyzer at
/// search time, so stemming and stopwords apply consistently).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SynonymRule {
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
}

/// Parses Solr synonym-map rules: one rule per line, either equivalent terms
/// (`USA, United States, America`) or an explicit mapping (`Washington,
/// Wash. => WA`). Blank lines are skipped.
///
/// # Errors
///
/// Returns an error string when a rule is empty, has an empty side, or
/// contains an empty term.
pub(crate) fn parse_synonym_rules(synonyms: &str) -> Result<Vec<SynonymRule>, String> {
    let mut rules = Vec::new();
    for (index, line) in synonyms.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rule_no = index + 1;
        let (inputs, outputs) = if let Some((left, right)) = line.split_once("=>") {
            let inputs = split_synonym_terms(left, rule_no)?;
            let outputs = split_synonym_terms(right, rule_no)?;
            (inputs, outputs)
        } else {
            let terms = split_synonym_terms(line, rule_no)?;
            (terms.clone(), terms)
        };
        if inputs.is_empty() || outputs.is_empty() {
            return Err(format!(
                "Synonym rule {rule_no} {line:?} has an empty side; each side needs at least one term."
            ));
        }
        rules.push(SynonymRule { inputs, outputs });
    }
    Ok(rules)
}

/// Splits one side of a synonym rule into lowercase terms: comma-separated
/// phrases, each split on whitespace.
fn split_synonym_terms(side: &str, rule_no: usize) -> Result<Vec<String>, String> {
    let mut terms = Vec::new();
    for phrase in side.split(',') {
        let mut words = 0;
        for word in phrase.split_whitespace() {
            if word.is_empty() {
                continue;
            }
            words += 1;
            terms.push(word.to_lowercase());
        }
        if words == 0 {
            return Err(format!(
                "Synonym rule {rule_no} has an empty term; terms must be non-empty."
            ));
        }
    }
    if terms.is_empty() {
        return Err(format!(
            "Synonym rule {rule_no} has an empty side; each side needs at least one term."
        ));
    }
    Ok(terms)
}

/// A synonym map: a named collection of synonym rules in Solr format.
/// Synonym maps apply to search: query terms matching a rule's inputs also
/// match its outputs (see `docs/supported_operations.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynonymMap {
    pub name: String,
    /// Always `"solr"`; the only format Azure supports.
    pub format: String,
    /// The synonym rules joined by newlines (the wire format the SDKs use).
    pub synonyms: String,
    /// The parsed rules, for query-time expansion.
    #[serde(skip)]
    pub(crate) rules: Vec<SynonymRule>,
    /// Opaque entity tag, bumped on every create or update.
    #[serde(rename = "@odata.etag")]
    pub etag: String,
}

impl SynonymMap {
    /// The JSON representation returned by the synonym-map routes.
    #[must_use]
    pub fn to_value(&self) -> Value {
        // Serialization of this plain-data struct cannot fail; fall back to
        // null rather than panicking in request handling.
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Validates a synonym-map definition: a non-empty name, the `solr` format
/// (the only format Azure supports), and well-formed synonym rules (at least
/// one non-blank rule; every rule needs non-empty sides and terms).
pub(crate) fn validate_synonym_map(
    name: &str,
    format: &str,
    synonyms: &str,
) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidSynonymMap",
            "The synonym map name is required.",
        ));
    }
    if format != "solr" {
        return Err(ApiError::bad_request(
            "InvalidSynonymMap",
            format!("Synonym map format {format:?} is not supported; only \"solr\" is supported."),
        ));
    }
    match parse_synonym_rules(synonyms) {
        Ok(rules) if !rules.is_empty() => Ok(()),
        Ok(_) => Err(ApiError::bad_request(
            "InvalidSynonymMap",
            "The synonym map must contain at least one synonym rule.",
        )),
        Err(e) => Err(ApiError::bad_request(
            "InvalidSynonymMap",
            format!("Invalid synonym rules: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::ok;

    #[test]
    fn synonym_rules_parse_solr_format() {
        let rules = ok(parse_synonym_rules("USA, United States\nWashington => WA"));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].inputs, vec!["usa", "united", "states"]);
        assert_eq!(rules[0].outputs, vec!["usa", "united", "states"]);
        assert_eq!(rules[1].inputs, vec!["washington"]);
        assert_eq!(rules[1].outputs, vec!["wa"]);
        // Blank lines are skipped.
        assert_eq!(ok(parse_synonym_rules("\n  \na, b\n")).len(), 1);
        // Empty rules, sides, and terms are rejected.
        assert!(parse_synonym_rules("").is_ok());
        assert!(parse_synonym_rules("   ").is_ok());
        assert!(parse_synonym_rules("a, => b").is_err());
        assert!(parse_synonym_rules("=> b").is_err());
        assert!(parse_synonym_rules("a =>").is_err());
        assert!(parse_synonym_rules("a,,b").is_err());
    }
}
