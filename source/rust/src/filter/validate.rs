//! Filter validation against the index schema.

use crate::storage::{FieldDefinition, IndexDefinition};

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
            if !field_def.is_collection() {
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
