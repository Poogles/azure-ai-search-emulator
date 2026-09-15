//! Index-schema and document validation.

use serde_json::Value;

use crate::error::ApiError;
use crate::storage::{Document, FieldDefinition, FieldType, IndexDefinition};
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
const SUPPORTED_FIELD_TYPES: &[FieldType] = &[
    FieldType::String,
    FieldType::Int32,
    FieldType::Int64,
    FieldType::Int8,
    FieldType::Int16,
    FieldType::Single,
    FieldType::Double,
    FieldType::Boolean,
    FieldType::DateTimeOffset,
    FieldType::Time,
    FieldType::Duration,
    FieldType::Binary,
    FieldType::Guid,
    FieldType::GeographyPoint,
    FieldType::CollectionString,
    FieldType::CollectionInt32,
    FieldType::CollectionInt64,
    FieldType::CollectionInt8,
    FieldType::CollectionInt16,
    FieldType::CollectionSingle,
    FieldType::CollectionDouble,
    FieldType::CollectionHalf,
    FieldType::CollectionBoolean,
    FieldType::CollectionDateTimeOffset,
    FieldType::CollectionBinary,
    FieldType::CollectionGuid,
];

/// Inserts `name` into `seen`; on a duplicate, returns the `InvalidIndex`
/// error produced by `message`.
fn ensure_unique<'a>(
    seen: &mut std::collections::BTreeSet<&'a str>,
    name: &'a str,
    message: impl FnOnce() -> String,
) -> Result<(), ApiError> {
    if !seen.insert(name) {
        return Err(ApiError::invalid_index(message()));
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
                return Err(ApiError::invalid_index(format!(
                    "Field {:?} cannot be the key: complex type fields cannot be keys.",
                    field.name
                )));
            }
            key_count += 1;
        }
        if field.is_complex_type() {
            if field.searchable || field.sortable || field.facetable {
                return Err(ApiError::invalid_index(
                    format!(
                        "Field {:?} is a complex type and cannot be searchable, sortable, or facetable; \
                         set those attributes on its subfields instead.",
                        field.name
                    ),
                ));
            }
            validate_subfields(field)?;
        } else if !SUPPORTED_FIELD_TYPES.contains(&field.field_type) {
            return Err(ApiError::invalid_index(format!(
                "Unsupported field type {:?} for field {:?}. Supported types: {}, Edm.ComplexType.",
                field.field_type.as_str(),
                field.name,
                SUPPORTED_FIELD_TYPES
                    .iter()
                    .map(FieldType::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if field.sortable && field.field_type.is_equality_only() {
            return Err(ApiError::invalid_index(format!(
                "Field {:?} has type {:?}, which supports only eq/ne comparisons and cannot be sortable.",
                field.name,
                field.field_type.as_str()
            )));
        }
    }
    if key_count != 1 {
        return Err(ApiError::invalid_index(format!(
            "Index schema must define exactly one key field; found {key_count}."
        )));
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
    if !field.field_type.is_vector_field_type() {
        return Err(ApiError::invalid_index(
            format!(
                "Unsupported vector field type {:?} for field {:?}; only 'Collection(Edm.Single)' and 'Collection(Edm.Half)' are supported.",
                field.field_type.as_str(), field.name
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
            return Err(ApiError::invalid_index(format!(
                "Vector field {:?} has invalid dimensions {raw}; must be 1-{max_dimension}.",
                field.name
            )));
        }
    }
    if field
        .vector_search_profile
        .as_deref()
        .is_none_or(str::is_empty)
    {
        return Err(ApiError::invalid_index(format!(
            "Vector field {:?} is missing required \"vectorSearchProfile\".",
            field.name
        )));
    }
    if !field.searchable {
        return Err(ApiError::invalid_index(format!(
            "Vector field {:?} must be searchable; set \"searchable\": true.",
            field.name
        )));
    }
    if field.is_key {
        return Err(ApiError::invalid_index(format!(
            "Vector field {:?} cannot be the key.",
            field.name
        )));
    }
    for (attribute, set) in [
        ("filterable", field.filterable),
        ("sortable", field.sortable),
        ("facetable", field.facetable),
    ] {
        if set {
            return Err(ApiError::invalid_index(format!(
                "Vector field {:?} cannot be {attribute}.",
                field.name
            )));
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
        return Err(ApiError::invalid_index(format!(
            "Index {:?} has {} vector fields; at most {} are supported.",
            definition.name,
            vector_fields.len(),
            crate::vector::MAX_VECTOR_FIELDS
        )));
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
        return Err(ApiError::invalid_index(format!(
            "Index {:?} has vector fields but no vectorSearch configuration.",
            definition.name
        )));
    }
    let config =
        parse_vector_search(definition.vector_search.as_ref()).map_err(ApiError::invalid_index)?;
    for field in vector_fields {
        let profile = field.vector_search_profile.clone().unwrap_or_default();
        if !config.profiles.contains_key(&profile) {
            return Err(ApiError::invalid_index(format!(
                "Vector field {:?} references unknown vector search profile {profile:?}.",
                field.name
            )));
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
            return Err(ApiError::invalid_index(format!(
                "Suggester {:?} must define a non-empty \"searchFields\" array.",
                suggester.name
            )));
        }
        for field_name in &suggester.search_fields {
            let field_def = definition.field_path(field_name).ok_or_else(|| {
                ApiError::invalid_index(format!(
                    "Suggester {:?} references unknown field {:?}.",
                    suggester.name, field_name
                ))
            })?;
            if !field_def.searchable {
                return Err(ApiError::invalid_index(format!(
                    "Suggester {:?} references field {:?}, which is not searchable; \
                         mark it \"searchable\": true in the index schema.",
                    suggester.name, field_name
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_subfields(field: &FieldDefinition) -> Result<(), ApiError> {
    if field.subfields.is_empty() {
        return Err(ApiError::invalid_index(format!(
            "Complex type field {:?} must define a non-empty \"fields\" array of subfields.",
            field.name
        )));
    }
    validate_subfields_at(&field.subfields, &field.name)
}

/// Recursively validates the subfields of a (possibly nested) complex type:
/// each level's names are unique, subfields may be scalar,
/// collection-of-scalar, or further complex types (to any depth), and no
/// subfield may be a key or a vector field. Complex-typed subfields cannot
/// themselves be searchable, sortable, or facetable (set those attributes on
/// their scalar subfields instead). `path` is the `/`-joined path to the
/// complex field holding `subfields`, for error messages.
fn validate_subfields_at(subfields: &[FieldDefinition], path: &str) -> Result<(), ApiError> {
    let mut seen = std::collections::BTreeSet::new();
    for subfield in subfields {
        ensure_unique(&mut seen, subfield.name.as_str(), || {
            format!(
                "Duplicate subfield name {:?} in complex type field {path:?}.",
                subfield.name
            )
        })?;
        if subfield.is_key {
            return Err(ApiError::invalid_index(format!(
                "Subfield {:?} of complex type field {path:?} cannot be a key.",
                subfield.name
            )));
        }
        if subfield.has_dimensions_property() || subfield.vector_search_profile.is_some() {
            return Err(ApiError::invalid_index(format!(
                "Subfield {:?} of complex type field {path:?} cannot be a vector field.",
                subfield.name
            )));
        }
        if subfield.is_complex_type() {
            if subfield.searchable || subfield.sortable || subfield.facetable {
                return Err(ApiError::invalid_index(format!(
                    "Subfield {:?} of complex type field {path:?} is a complex type and cannot be \
                     searchable, sortable, or facetable; set those attributes on its subfields instead.",
                    subfield.name
                )));
            }
            if subfield.subfields.is_empty() {
                return Err(ApiError::invalid_index(format!(
                    "Complex type subfield {:?} of complex type field {path:?} must define a \
                     non-empty \"fields\" array of subfields.",
                    subfield.name
                )));
            }
            let nested_path = format!("{path}/{}", subfield.name);
            validate_subfields_at(&subfield.subfields, &nested_path)?;
            continue;
        }
        if !SUPPORTED_FIELD_TYPES.contains(&subfield.field_type) {
            return Err(ApiError::invalid_index(format!(
                "Unsupported subfield type {:?} for subfield {:?} of complex type field {path:?}. \
                 Supported types: {}.",
                subfield.field_type.as_str(),
                subfield.name,
                SUPPORTED_FIELD_TYPES
                    .iter()
                    .map(FieldType::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if subfield.sortable && subfield.field_type.is_equality_only() {
            return Err(ApiError::invalid_index(format!(
                "Subfield {:?} of complex type field {path:?} has type {:?}, which supports only \
                 eq/ne comparisons and cannot be sortable.",
                subfield.name,
                subfield.field_type.as_str()
            )));
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
    let ok = if let Some(inner) = field_type.inner_type() {
        value
            .as_array()
            .is_some_and(|items| items.iter().all(|item| scalar_type_ok(&inner, item)))
    } else {
        scalar_type_ok(field_type, value)
    };
    if ok {
        Ok(())
    } else {
        let hint = if matches!(field_type, FieldType::GeographyPoint) {
            " Expected a GeoJSON point object \
             {\"type\": \"Point\", \"coordinates\": [lon, lat]} or a string."
        } else if matches!(
            field_type.inner_type().as_ref().unwrap_or(field_type),
            FieldType::Int8 | FieldType::Int16
        ) {
            " Expected an integer in range."
        } else if matches!(
            field_type.inner_type().as_ref().unwrap_or(field_type),
            FieldType::Time
        ) {
            " Expected a time-of-day string \"HH:MM:SS\" (fractional seconds allowed)."
        } else if matches!(
            field_type.inner_type().as_ref().unwrap_or(field_type),
            FieldType::Duration
        ) {
            " Expected an ISO 8601 duration string (e.g. \"P1DT2H\")."
        } else if matches!(
            field_type.inner_type().as_ref().unwrap_or(field_type),
            FieldType::Binary
        ) {
            " Expected a base64-encoded string."
        } else {
            ""
        };
        Err(format!(
            "Value for field {name:?} is not compatible with type {:?}.{hint}",
            field_type.as_str()
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

/// Whether a value is compatible with a scalar `Edm.*` type, including the
/// range and format rules for the narrow integer, time, duration, and binary
/// types: `Edm.Int8` accepts integers in -128–127, `Edm.Int16` in
/// -32768–32767, `Edm.Time` accepts zero-padded `"HH:MM:SS"` strings
/// (optional fractional seconds), `Edm.Duration` accepts ISO 8601 duration
/// strings, and `Edm.Binary` accepts base64-encoded strings.
pub(crate) fn scalar_type_ok(inner: &FieldType, value: &Value) -> bool {
    match inner {
        FieldType::String | FieldType::DateTimeOffset | FieldType::Guid => value.is_string(),
        FieldType::GeographyPoint => is_geography_point(value),
        FieldType::Int32 | FieldType::Int64 => value.is_i64() || value.is_u64(),
        FieldType::Int8 => int_in_range(value, -128, 127),
        FieldType::Int16 => int_in_range(value, -32_768, 32_767),
        FieldType::Time => value.as_str().is_some_and(is_time_string),
        FieldType::Duration => value.as_str().is_some_and(is_duration_string),
        FieldType::Binary => value.as_str().is_some_and(is_base64_string),
        FieldType::Single | FieldType::Double => value.is_number(),
        FieldType::Boolean => value.is_boolean(),
        _ => true,
    }
}

/// Whether a JSON value is an integer in the inclusive range `[min, max]`.
fn int_in_range(value: &Value, min: i64, max: i64) -> bool {
    if let Some(n) = value.as_i64() {
        (min..=max).contains(&n)
    } else if let Some(n) = value.as_u64() {
        i64::try_from(n).is_ok_and(|n| (min..=max).contains(&n))
    } else {
        false
    }
}

/// Whether a string is a time of day in `"HH:MM:SS"` form with an optional
/// fractional-seconds suffix (`"10:30:45"`, `"10:30:45.123"`). Hours are
/// 00–23, minutes and seconds 00–59. Zero-padded fixed width keeps
/// lexicographic comparison chronological.
pub(crate) fn is_time_string(text: &str) -> bool {
    let (time, fraction) = match text.split_once('.') {
        Some((time, fraction)) => (time, Some(fraction)),
        None => (text, None),
    };
    if let Some(fraction) = fraction {
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return false;
        }
    }
    let mut parts = time.split(':');
    let (Some(hour), Some(minute), Some(second), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    hour.len() == 2
        && minute.len() == 2
        && second.len() == 2
        && hour.parse::<u32>().is_ok_and(|h| h <= 23)
        && minute.parse::<u32>().is_ok_and(|m| m <= 59)
        && second.parse::<u32>().is_ok_and(|s| s <= 59)
}

/// Whether a string is an ISO 8601 duration (`"P1Y2M3DT4H5M6S"`,
/// `"PT30S"`, `"P1W"`): a leading `P` with date and/or time components. At
/// least one component is required; time components follow a `T` separator.
pub(crate) fn is_duration_string(text: &str) -> bool {
    let rest = text.strip_prefix('P').unwrap_or("");
    if rest.is_empty() {
        return false;
    }
    let (date_part, time_part) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    // A bare `T` separator with no time components is invalid, as is a
    // second `T`.
    if time_part.is_some_and(|time| time.is_empty() || time.contains('T')) {
        return false;
    }
    let mut has_component = false;
    for (part, markers) in [(date_part, "YMWD"), (time_part.unwrap_or(""), "HMS")] {
        let mut current = part;
        for marker in markers.chars() {
            // Each component is `<number><marker>`; at most one of each.
            // Seconds additionally allow fractional values (`PT0.5S`).
            let Some((number, after)) = current.split_once(marker) else {
                continue;
            };
            let number_ok = if marker == 'S' {
                is_duration_seconds(number)
            } else {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            };
            if !number_ok {
                return false;
            }
            has_component = true;
            current = after;
        }
        if !current.is_empty() {
            return false;
        }
    }
    // Week (`W`) cannot be combined with other components.
    if date_part.contains('W') {
        let digits = date_part.strip_suffix('W').unwrap_or("");
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        return time_part.is_none() && has_component;
    }
    has_component
}

/// Whether a duration seconds component is valid: digits with an optional
/// single fractional part (`"6"`, `"0.5"`).
fn is_duration_seconds(number: &str) -> bool {
    match number.split_once('.') {
        Some((whole, fraction)) => {
            !whole.is_empty()
                && !fraction.is_empty()
                && whole.bytes().all(|b| b.is_ascii_digit())
                && fraction.bytes().all(|b| b.is_ascii_digit())
        }
        None => !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// Whether a string is base64-decodable (standard or URL-safe alphabet).
fn is_base64_string(text: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(text))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn time_strings_require_zero_padded_clock_form() {
        for valid in [
            "00:00:00",
            "10:30:45",
            "23:59:59",
            "10:30:45.123",
            "01:02:03.000000001",
        ] {
            assert!(is_time_string(valid), "expected valid: {valid:?}");
        }
        for invalid in [
            "",
            "10:30",
            "10:30:45:00",
            "24:00:00",
            "10:60:00",
            "10:30:60",
            "1:02:03",
            "10:30:45.",
            "10:30:45.1234567890",
            "10:30:45Z",
            "not a time",
        ] {
            assert!(!is_time_string(invalid), "expected invalid: {invalid:?}");
        }
    }

    #[test]
    fn duration_strings_require_iso_8601_form() {
        for valid in [
            "P1D",
            "P1Y2M3DT4H5M6S",
            "PT30S",
            "PT0.5S",
            "P1W",
            "P2M",
            "PT1H",
        ] {
            assert!(is_duration_string(valid), "expected valid: {valid:?}");
        }
        for invalid in [
            "", "P", "PT", "1D", "P1X", "P1Y2Y", "PT1S1H", "P1W2D", "P1WT2H", "T1H", "P1DT",
        ] {
            assert!(
                !is_duration_string(invalid),
                "expected invalid: {invalid:?}"
            );
        }
    }

    #[test]
    fn narrow_integers_enforce_range() {
        assert!(scalar_type_ok(&FieldType::Int8, &json!(127)));
        assert!(scalar_type_ok(&FieldType::Int8, &json!(-128)));
        assert!(!scalar_type_ok(&FieldType::Int8, &json!(128)));
        assert!(!scalar_type_ok(&FieldType::Int8, &json!(-129)));
        assert!(!scalar_type_ok(&FieldType::Int8, &json!(1.5)));
        assert!(!scalar_type_ok(&FieldType::Int8, &json!("5")));
        assert!(scalar_type_ok(&FieldType::Int16, &json!(32_767)));
        assert!(scalar_type_ok(&FieldType::Int16, &json!(-32_768)));
        assert!(!scalar_type_ok(&FieldType::Int16, &json!(32_768)));
        assert!(!scalar_type_ok(&FieldType::Int16, &json!(-32_769)));
    }

    #[test]
    fn time_duration_binary_accept_formatted_strings() {
        assert!(scalar_type_ok(&FieldType::Time, &json!("10:30:45")));
        assert!(!scalar_type_ok(&FieldType::Time, &json!("10:30")));
        assert!(!scalar_type_ok(&FieldType::Time, &json!(103_045)));
        assert!(scalar_type_ok(&FieldType::Duration, &json!("P1DT2H")));
        assert!(!scalar_type_ok(&FieldType::Duration, &json!("tomorrow")));
        assert!(scalar_type_ok(&FieldType::Binary, &json!("aGVsbG8=")));
        assert!(!scalar_type_ok(
            &FieldType::Binary,
            &json!("*** not base64 ***")
        ));
        assert!(!scalar_type_ok(&FieldType::Binary, &json!(42)));
    }
}
