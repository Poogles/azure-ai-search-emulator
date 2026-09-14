//! Filter validation against the index schema.

use crate::storage::{FieldDefinition, FieldType, IndexDefinition};

use super::{FilterExpr, FilterValue, StringFunc};

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
            if op.is_ordering() && field_def.is_collection() {
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
/// variable, optionally addressing one subfield level (`var/Subfield`).
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
    let segments: Vec<&str> = inner_field.split('/').collect();
    if segments.len() > 2 {
        return Err(format!(
            "any/all on field {field:?} supports at most one level of subfield \
              access through the lambda variable; {inner_field:?} is too deep."
        ));
    }
    if segments.len() == 2 {
        // Subfield access (`var/Subfield`): the subfield must exist and be
        // filterable. The collection field itself need not be
        // (collection-of-complex subfields carry the `filterable` flag, not
        // the parent).
        let subfield_name = segments[1];
        let subfield = field_def
            .subfields
            .iter()
            .find(|s| s.name == subfield_name)
            .ok_or_else(|| {
                format!(
                    "any/all on field {field:?} references unknown subfield \
                      {subfield_name:?}."
                )
            })?;
        if !subfield.filterable {
            return Err(format!(
                "Subfield {subfield_name:?} of field {field:?} is not filterable; \
                  mark it \"filterable\": true in the index schema."
            ));
        }
    } else if !field_def.filterable {
        // A plain comparison on the element (a scalar collection) requires
        // the collection field to be filterable.
        return Err(format!(
            "Field {field:?} is not filterable; mark it \"filterable\": true in the index schema."
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
