//! Filter validation against the index schema.

use crate::storage::{FieldDefinition, FieldType, IndexDefinition};

use super::{FilterExpr, FilterOp, FilterValue, StringFunc};

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
        FilterExpr::Compare { field, op, value } => validate_compare(field, *op, value, definition),
        FilterExpr::In { field, .. }
        | FilterExpr::IsEmpty { field }
        | FilterExpr::IsNull { field } => {
            require_filterable(field, definition)?;
            Ok(())
        }
        FilterExpr::StringFunc { func, field, .. } => {
            let field_def = require_filterable(field, definition)?;
            if !matches!(
                field_def.field_type,
                FieldType::String | FieldType::CollectionString
            ) {
                let name = match func {
                    StringFunc::StartsWith => "startswith",
                    StringFunc::EndsWith => "endswith",
                    StringFunc::Contains => "contains",
                };
                return Err(format!(
                    "Filter function {name} on field {field:?} requires a string field; \
                     field type is {:?}.",
                    field_def.field_type.as_str()
                ));
            }
            Ok(())
        }
        FilterExpr::StringFuncCompare { expr, .. } => {
            let field_def = require_filterable(expr.field(), definition)?;
            if !matches!(
                field_def.field_type,
                FieldType::String | FieldType::CollectionString
            ) {
                return Err(format!(
                    "Filter function {} on field {:?} requires a string field; \
                     field type is {:?}.",
                    expr.name(),
                    expr.field(),
                    field_def.field_type.as_str()
                ));
            }
            Ok(())
        }
        FilterExpr::Any { field, inner } | FilterExpr::All { field, inner } => {
            validate_lambda(field, inner, definition)
        }
        FilterExpr::DateCompare { left, .. } => {
            for field in left.referenced_fields() {
                let field_def = require_filterable(&field, definition)?;
                if !matches!(field_def.field_type, FieldType::DateTimeOffset) {
                    return Err(format!(
                        "Date filter on field {field:?} requires an Edm.DateTimeOffset field; \
                         field type is {:?}.",
                        field_def.field_type.as_str()
                    ));
                }
            }
            Ok(())
        }
        FilterExpr::IsMatch { field, .. } => {
            let field_def = require_filterable(field, definition)?;
            if !matches!(
                field_def.field_type,
                FieldType::String | FieldType::CollectionString
            ) {
                return Err(format!(
                    "Filter function search.ismatch on field {field:?} requires a string field; \
                     field type is {:?}.",
                    field_def.field_type.as_str()
                ));
            }
            Ok(())
        }
    }
}

/// Validates an `any`/`all` collection filter: the field must be a
/// collection, and the lambda body must be a single comparison on the lambda
/// variable, optionally addressing (possibly nested) subfields
/// (`var/Subfield`, `var/A/B`, ...) through the variable.
fn validate_lambda(
    field: &str,
    inner: &FilterExpr,
    definition: &IndexDefinition,
) -> Result<(), String> {
    let field_def = definition
        .field_path(field)
        .ok_or_else(|| format!("Filter references unknown field {field:?}."))?;
    if !field_def.is_collection() {
        return Err(format!(
            "Field {field:?} is not a collection; any/all require a collection field."
        ));
    }
    let FilterExpr::Compare {
        field: inner_field, ..
    } = inner
    else {
        return Err(format!(
            "any/all on field {field:?} must contain a single comparison on the lambda variable."
        ));
    };
    let mut segments = inner_field.split('/');
    // The first segment is the lambda variable itself; what follows is an
    // optional subfield path through the element.
    segments.next();
    let sub_path: Vec<&str> = segments.collect();
    if sub_path.is_empty() {
        // A plain comparison on the element (a scalar collection) requires
        // the collection field to be filterable.
        if !field_def.filterable {
            return Err(format!(
                "Field {field:?} is not filterable; mark it \"filterable\": true in the index schema."
            ));
        }
        return Ok(());
    }
    // Subfield access (`var/Subfield/...`): walk through (possibly nested)
    // complex subfields of the collection element. The terminal subfield
    // must exist and be filterable. (The collection field itself need not
    // be: collection-of-complex subfields carry the `filterable` flag.)
    if !matches!(field_def.field_type, FieldType::CollectionComplexType) {
        return Err(format!(
            "any/all on field {field:?} addresses subfield {:?}, but field {field:?} is not a \
             collection of complex objects.",
            sub_path.join("/")
        ));
    }
    let mut current = field_def;
    for (depth, segment) in sub_path.iter().enumerate() {
        let subfield = current
            .subfields
            .iter()
            .find(|s| &s.name == segment)
            .ok_or_else(|| {
                format!("any/all on field {field:?} references unknown subfield {segment:?}.")
            })?;
        let last = depth == sub_path.len() - 1;
        if !last && !subfield.is_complex_type() {
            return Err(format!(
                "any/all on field {field:?} traverses non-complex subfield {segment:?}."
            ));
        }
        if last && !subfield.filterable {
            return Err(format!(
                "Subfield {segment:?} of field {field:?} is not filterable; \
                 mark it \"filterable\": true in the index schema."
            ));
        }
        current = subfield;
    }
    Ok(())
}

/// Validates one comparison: the field must be filterable, and ordering
/// operators are rejected on booleans, collections, and equality-only
/// (`Edm.Duration`/`Edm.Binary`) types.
fn validate_compare(
    field: &str,
    op: FilterOp,
    value: &FilterValue,
    definition: &IndexDefinition,
) -> Result<(), String> {
    let field_def = require_filterable(field, definition)?;
    if op.is_ordering() && matches!(value, FilterValue::Bool(_)) {
        return Err(format!(
            "Filter operator {op:?} is not supported for boolean values in field {field:?}."
        ));
    }
    if op.is_ordering() && field_def.is_collection() {
        return Err(format!(
            "Filter operator {op:?} is not supported for collection field {field:?}; \
             use any/all for collection filtering."
        ));
    }
    if op.is_ordering() && field_def.field_type.is_equality_only() {
        return Err(format!(
            "Filter operator {op:?} is not supported for field {field:?} of type {:?}; \
             only eq/ne comparisons are supported.",
            field_def.field_type.as_str()
        ));
    }
    Ok(())
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
