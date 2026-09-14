//! Index-schema and document validation.

use serde_json::Value;

use crate::error::ApiError;
use crate::storage::{Document, FieldDefinition, IndexDefinition};
use crate::vector::parse_vector_search;

pub(crate) fn key_field_name(definition: &IndexDefinition) -> String {
    definition
        .key_field()
        .map(|f| f.name.clone())
        .unwrap_or_default()
}

pub(crate) fn key_display(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Field types accepted by the schema validator.
const SUPPORTED_FIELD_TYPES: &[&str] = &[
    "Edm.String",
    "Edm.Int32",
    "Edm.Int64",
    "Edm.Single",
    "Edm.Double",
    "Edm.Boolean",
    "Edm.DateTimeOffset",
    "Edm.Guid",
    "Edm.GeographyPoint",
    "Edm.Collection(Edm.String)",
    "Edm.Collection(Edm.Int32)",
    "Edm.Collection(Edm.Int64)",
    "Edm.Collection(Edm.Single)",
    "Edm.Collection(Edm.Double)",
    "Edm.Collection(Edm.Half)",
    "Edm.Collection(Edm.Boolean)",
    "Edm.Collection(Edm.DateTimeOffset)",
    "Edm.Collection(Edm.Guid)",
];

/// Inserts `name` into `seen`; on a duplicate, returns the `InvalidIndex`
/// error produced by `message`.
fn ensure_unique<'a>(
    seen: &mut std::collections::BTreeSet<&'a str>,
    name: &'a str,
    message: impl FnOnce() -> String,
) -> Result<(), ApiError> {
    if !seen.insert(name) {
        return Err(ApiError::bad_request("InvalidIndex", message()));
    }
    Ok(())
}

pub(crate) fn validate_schema(
    definition: &IndexDefinition,
    max_vector_dimension: usize,
) -> Result<(), ApiError> {
    let mut key_count = 0;
    let mut seen = std::collections::BTreeSet::new();
    for field in &definition.fields {
        validate_vector_field(field, max_vector_dimension)?;
        ensure_unique(&mut seen, field.name.as_str(), || {
            format!("Duplicate field name {:?} in index schema.", field.name)
        })?;
        if field.is_key {
            if field.is_complex_type() {
                return Err(ApiError::bad_request(
                    "InvalidIndex",
                    format!(
                        "Field {:?} cannot be the key: complex type fields cannot be keys.",
                        field.name
                    ),
                ));
            }
            key_count += 1;
        }
        if field.is_complex_type() {
            if field.searchable || field.sortable || field.facetable {
                return Err(ApiError::bad_request(
                    "InvalidIndex",
                    format!(
                        "Field {:?} is a complex type and cannot be searchable, sortable, or facetable; \
                         set those attributes on its subfields instead.",
                        field.name
                    ),
                ));
            }
            validate_subfields(field)?;
        } else if !SUPPORTED_FIELD_TYPES.contains(&field.field_type.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Unsupported field type {:?} for field {:?}. Supported types: {}, Edm.ComplexType.",
                    field.field_type,
                    field.name,
                    SUPPORTED_FIELD_TYPES.join(", ")
                ),
            ));
        }
    }
    if key_count != 1 {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!("Index schema must define exactly one key field; found {key_count}."),
        ));
    }
    validate_suggesters(definition)?;
    validate_vector_search_config(definition)?;
    Ok(())
}

pub(crate) fn validate_vector_field(
    field: &FieldDefinition,
    max_dimension: usize,
) -> Result<(), ApiError> {
    let attempts_vector = field.has_dimensions_property() || field.vector_search_profile.is_some();
    if !attempts_vector {
        return Ok(());
    }
    if !matches!(
        field.field_type.as_str(),
        "Edm.Collection(Edm.Single)" | "Edm.Collection(Edm.Half)"
    ) {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Unsupported vector field type {:?} for field {:?}; only 'Collection(Edm.Single)' and 'Collection(Edm.Half)' are supported.",
                field.field_type, field.name
            ),
        ));
    }
    match field.vector_dimensions {
        Some(dimensions) if dimensions <= max_dimension => {}
        _ => {
            let raw = field
                .raw
                .get("dimensions")
                .or_else(|| field.raw.get("vector_search_dimensions"))
                .map_or("missing".to_owned(), Value::to_string);
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Vector field {:?} has invalid dimensions {raw}; must be 1-{max_dimension}.",
                    field.name
                ),
            ));
        }
    }
    if field
        .vector_search_profile
        .as_deref()
        .is_none_or(str::is_empty)
    {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Vector field {:?} is missing required \"vectorSearchProfile\".",
                field.name
            ),
        ));
    }
    if !field.searchable {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Vector field {:?} must be searchable; set \"searchable\": true.",
                field.name
            ),
        ));
    }
    if field.is_key {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!("Vector field {:?} cannot be the key.", field.name),
        ));
    }
    for (attribute, set) in [
        ("filterable", field.filterable),
        ("sortable", field.sortable),
        ("facetable", field.facetable),
    ] {
        if set {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!("Vector field {:?} cannot be {attribute}.", field.name),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_vector_search_config(definition: &IndexDefinition) -> Result<(), ApiError> {
    let vector_fields: Vec<&FieldDefinition> = definition
        .fields
        .iter()
        .filter(|f| f.is_vector_field())
        .collect();
    if vector_fields.len() > crate::vector::MAX_VECTOR_FIELDS {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Index {:?} has {} vector fields; at most {} are supported.",
                definition.name,
                vector_fields.len(),
                crate::vector::MAX_VECTOR_FIELDS
            ),
        ));
    }
    if vector_fields.is_empty() {
        return Ok(());
    }
    let has_profiles = definition
        .vector_search
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|obj| obj.get("profiles").or_else(|| obj.get("profile")))
        .and_then(Value::as_array)
        .is_some_and(|profiles| !profiles.is_empty());
    if !has_profiles {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Index {:?} has vector fields but no vectorSearch configuration.",
                definition.name
            ),
        ));
    }
    let config = parse_vector_search(definition.vector_search.as_ref())
        .map_err(|message| ApiError::bad_request("InvalidIndex", message))?;
    for field in vector_fields {
        let profile = field.vector_search_profile.clone().unwrap_or_default();
        if !config.profiles.contains_key(&profile) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Vector field {:?} references unknown vector search profile {profile:?}.",
                    field.name
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_suggesters(definition: &IndexDefinition) -> Result<(), ApiError> {
    let mut seen = std::collections::BTreeSet::new();
    for suggester in &definition.suggesters {
        ensure_unique(&mut seen, suggester.name.as_str(), || {
            format!(
                "Duplicate suggester name {:?} in index schema.",
                suggester.name
            )
        })?;
        if suggester.search_fields.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Suggester {:?} must define a non-empty \"searchFields\" array.",
                    suggester.name
                ),
            ));
        }
        for field_name in &suggester.search_fields {
            let field_def = definition.field_path(field_name).ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidIndex",
                    format!(
                        "Suggester {:?} references unknown field {:?}.",
                        suggester.name, field_name
                    ),
                )
            })?;
            if !field_def.searchable {
                return Err(ApiError::bad_request(
                    "InvalidIndex",
                    format!(
                        "Suggester {:?} references field {:?}, which is not searchable; \
                         mark it \"searchable\": true in the index schema.",
                        suggester.name, field_name
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_subfields(field: &FieldDefinition) -> Result<(), ApiError> {
    if field.subfields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Complex type field {:?} must define a non-empty \"fields\" array of subfields.",
                field.name
            ),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for subfield in &field.subfields {
        ensure_unique(&mut seen, subfield.name.as_str(), || {
            format!(
                "Duplicate subfield name {:?} in complex type field {:?}.",
                subfield.name, field.name
            )
        })?;
        if subfield.is_key {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Subfield {:?} of complex type field {:?} cannot be a key.",
                    subfield.name, field.name
                ),
            ));
        }
        if subfield.has_dimensions_property() || subfield.vector_search_profile.is_some() {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Subfield {:?} of complex type field {:?} cannot be a vector field.",
                    subfield.name, field.name
                ),
            ));
        }
        if !SUPPORTED_FIELD_TYPES.contains(&subfield.field_type.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Unsupported subfield type {:?} for subfield {:?} of complex type field {:?}. \
                     Subfields must be scalar or collection-of-scalar types.",
                    subfield.field_type, subfield.name, field.name
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_document(
    definition: &IndexDefinition,
    document: &Value,
) -> Result<Document, String> {
    let obj = document
        .as_object()
        .ok_or_else(|| "Document must be a JSON object.".to_owned())?;
    let key_name = definition
        .key_field()
        .map(|f| f.name.clone())
        .ok_or_else(|| "Index has no key field.".to_owned())?;
    let key_value = obj
        .get(&key_name)
        .ok_or_else(|| format!("Document is missing the key field {key_name:?}."))?;
    let key = key_display(key_value).ok_or_else(|| {
        format!("Key field {key_name:?} must be a string or number, got {key_value:?}.")
    })?;
    for (name, value) in obj {
        let field = definition
            .field(name)
            .ok_or_else(|| format!("Document contains unknown field {name:?}."))?;
        check_field_type(field, value)?;
    }
    Ok(Document {
        key,
        fields: obj.clone(),
    })
}

pub(crate) fn check_field_type(field: &FieldDefinition, value: &Value) -> Result<(), String> {
    if field.is_complex_type() {
        return if field.is_collection() {
            check_complex_collection_value(field, value)
        } else {
            check_complex_value(field, value)
        };
    }
    if field.is_vector_field() {
        return check_vector_value(field, value);
    }
    let name = &field.name;
    let field_type = &field.field_type;
    let ok = if let Some(inner) = field_type
        .strip_prefix("Edm.Collection(")
        .and_then(|s| s.strip_suffix(')'))
    {
        value
            .as_array()
            .is_some_and(|items| items.iter().all(|item| type_ok(inner, item)))
    } else {
        type_ok(field_type, value)
    };
    if ok {
        Ok(())
    } else {
        let hint = if field_type == "Edm.GeographyPoint" {
            " Expected a GeoJSON point object \
             {\"type\": \"Point\", \"coordinates\": [lon, lat]} or a string."
        } else {
            ""
        };
        Err(format!(
            "Value for field {name:?} is not compatible with type {field_type:?}.{hint}"
        ))
    }
}

/// Narrows an `f64` JSON number to `f32` (`Edm.Single`). Returns `None` for
/// non-finite values and for `f64` magnitudes that overflow `f32` (which
/// would poison distance math).
pub(crate) fn finite_f32(n: f64) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = n as f32;
    (n.is_finite() && narrowed.is_finite()).then_some(narrowed)
}

/// Why an array of JSON values could not be narrowed to finite `f32`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FiniteF32ArrayError {
    /// A numeric value that is not finite (or overflows `f32`).
    NonFinite,
    /// A value that is not a number at all.
    NonNumeric,
}

/// Narrows a JSON array of values to finite `f32`s (`Edm.Single`): integers
/// are widened, and non-finite or overflowing numbers are rejected.
pub(crate) fn parse_finite_f32_array(items: &[Value]) -> Result<Vec<f32>, FiniteF32ArrayError> {
    let mut vector = Vec::with_capacity(items.len());
    for item in items {
        match item.as_f64().and_then(finite_f32) {
            Some(narrowed) => vector.push(narrowed),
            None if item.is_number() => return Err(FiniteF32ArrayError::NonFinite),
            None => return Err(FiniteF32ArrayError::NonNumeric),
        }
    }
    Ok(vector)
}

/// Validates a vector field value: a JSON array of exactly the declared
/// number of finite numbers. Integers are accepted (widened to `f32` at
/// index time).
pub(crate) fn check_vector_value(field: &FieldDefinition, value: &Value) -> Result<(), String> {
    let dimensions = field.vector_dimensions.unwrap_or(0);
    let Some(items) = value.as_array() else {
        return Err(format!(
            "Field {:?} must be an array of numbers (vector of dimension {dimensions}).",
            field.name
        ));
    };
    if items.len() != dimensions {
        return Err(format!(
            "Field {:?} expects a vector of dimension {dimensions}, got {}.",
            field.name,
            items.len()
        ));
    }
    parse_finite_f32_array(items)
        .map(|_| ())
        .map_err(|error| match error {
            FiniteF32ArrayError::NonFinite => format!(
                "Field {:?} must contain only finite numeric values.",
                field.name
            ),
            FiniteF32ArrayError::NonNumeric => {
                format!("Field {:?} must contain only numeric values.", field.name)
            }
        })
}

/// Validates a complex-type value: a JSON object whose members are known
/// subfields with type-compatible values. Missing subfields are allowed.
pub(crate) fn check_complex_value(field: &FieldDefinition, value: &Value) -> Result<(), String> {
    let obj = value.as_object().ok_or_else(|| {
        format!(
            "Value for field {:?} must be a JSON object with the subfields of the complex type.",
            field.name
        )
    })?;
    for (sub_name, sub_value) in obj {
        let subfield = field
            .subfields
            .iter()
            .find(|f| f.name == *sub_name)
            .ok_or_else(|| {
                format!(
                    "Complex field {:?} has no subfield {:?}; known subfields: {}.",
                    field.name,
                    sub_name,
                    field
                        .subfields
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        check_field_type(subfield, sub_value)?;
    }
    Ok(())
}

/// Validates a collection-of-complex value: a JSON array whose elements are
/// each a valid complex-type object (see [`check_complex_value`]).
pub(crate) fn check_complex_collection_value(
    field: &FieldDefinition,
    value: &Value,
) -> Result<(), String> {
    let items = value.as_array().ok_or_else(|| {
        format!(
            "Value for field {:?} must be a JSON array of objects with the subfields of the complex type.",
            field.name
        )
    })?;
    for item in items {
        check_complex_value(field, item)?;
    }
    Ok(())
}

/// Whether a value is an acceptable `Edm.GeographyPoint`: the `GeoJSON` point
/// object the SDKs send (an object with `type` set to "Point" and a two- or
/// three-element numeric `coordinates` array) or a plain string.
pub(crate) fn is_geography_point(value: &Value) -> bool {
    if value.is_string() {
        return true;
    }
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.get("type").and_then(Value::as_str) != Some("Point") {
        return false;
    }
    obj.get("coordinates")
        .and_then(Value::as_array)
        .is_some_and(|coords| {
            (coords.len() == 2 || coords.len() == 3) && coords.iter().all(Value::is_number)
        })
}

/// Whether a value is compatible with a scalar `Edm.*` field type. The single
/// scalar type check: used directly for scalar fields and per-element for
/// `Edm.Collection(...)` fields. Unknown types pass (schema validation has
/// already rejected unsupported types).
pub(crate) fn type_ok(inner: &str, value: &Value) -> bool {
    match inner {
        "Edm.String" | "Edm.DateTimeOffset" | "Edm.Guid" => value.is_string(),
        "Edm.GeographyPoint" => is_geography_point(value),
        "Edm.Int32" | "Edm.Int64" => value.is_i64() || value.is_u64(),
        "Edm.Single" | "Edm.Double" => value.is_number(),
        "Edm.Boolean" => value.is_boolean(),
        _ => true,
    }
}
