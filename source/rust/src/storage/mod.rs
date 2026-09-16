//! Storage abstraction for index definitions and documents.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::semantic::SemanticConfig;
use crate::sync_util::{read_unpoisoned, write_unpoisoned};

/// A normalized `Edm.*` field type. Recognized types are named variants;
/// anything else is preserved verbatim in [`FieldType::Unknown`] so error
/// messages can echo the original spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    String,
    Int32,
    Int64,
    Single,
    Double,
    Boolean,
    DateTimeOffset,
    Guid,
    GeographyPoint,
    /// `Edm.Int8` (REST spelling; the SDKs send `Edm.SByte`).
    Int8,
    /// `Edm.Int16`.
    Int16,
    /// `Edm.Time` (time of day, `"HH:MM:SS[.fraction]"`).
    Time,
    /// `Edm.Duration` (ISO 8601 duration, `"P1DT2H"`).
    Duration,
    /// `Edm.Binary` (base64 string).
    Binary,
    ComplexType,
    Half,
    CollectionString,
    CollectionInt32,
    CollectionInt64,
    CollectionSingle,
    CollectionDouble,
    CollectionHalf,
    CollectionBoolean,
    CollectionDateTimeOffset,
    CollectionGuid,
    CollectionInt8,
    CollectionInt16,
    CollectionBinary,
    CollectionComplexType,
    /// An unrecognized type, kept verbatim for echo in error messages.
    Unknown(String),
}

impl FieldType {
    /// The canonical `Edm.*` spelling (the stored string for `Unknown`), for
    /// echo in error messages.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        use FieldType as T;
        match self {
            T::Unknown(raw) => Cow::Borrowed(raw),
            T::String => Cow::Borrowed("Edm.String"),
            T::Int32 => Cow::Borrowed("Edm.Int32"),
            T::Int64 => Cow::Borrowed("Edm.Int64"),
            T::Single => Cow::Borrowed("Edm.Single"),
            T::Double => Cow::Borrowed("Edm.Double"),
            T::Boolean => Cow::Borrowed("Edm.Boolean"),
            T::DateTimeOffset => Cow::Borrowed("Edm.DateTimeOffset"),
            T::Guid => Cow::Borrowed("Edm.Guid"),
            T::GeographyPoint => Cow::Borrowed("Edm.GeographyPoint"),
            T::Int8 => Cow::Borrowed("Edm.Int8"),
            T::Int16 => Cow::Borrowed("Edm.Int16"),
            T::Time => Cow::Borrowed("Edm.Time"),
            T::Duration => Cow::Borrowed("Edm.Duration"),
            T::Binary => Cow::Borrowed("Edm.Binary"),
            T::ComplexType => Cow::Borrowed("Edm.ComplexType"),
            T::Half => Cow::Borrowed("Edm.Half"),
            T::CollectionString => Cow::Borrowed("Edm.Collection(Edm.String)"),
            T::CollectionInt32 => Cow::Borrowed("Edm.Collection(Edm.Int32)"),
            T::CollectionInt64 => Cow::Borrowed("Edm.Collection(Edm.Int64)"),
            T::CollectionInt8 => Cow::Borrowed("Edm.Collection(Edm.Int8)"),
            T::CollectionInt16 => Cow::Borrowed("Edm.Collection(Edm.Int16)"),
            T::CollectionBinary => Cow::Borrowed("Edm.Collection(Edm.Binary)"),
            T::CollectionSingle => Cow::Borrowed("Edm.Collection(Edm.Single)"),
            T::CollectionDouble => Cow::Borrowed("Edm.Collection(Edm.Double)"),
            T::CollectionHalf => Cow::Borrowed("Edm.Collection(Edm.Half)"),
            T::CollectionBoolean => Cow::Borrowed("Edm.Collection(Edm.Boolean)"),
            T::CollectionDateTimeOffset => Cow::Borrowed("Edm.Collection(Edm.DateTimeOffset)"),
            T::CollectionGuid => Cow::Borrowed("Edm.Collection(Edm.Guid)"),
            T::CollectionComplexType => Cow::Borrowed("Edm.Collection(Edm.ComplexType)"),
        }
    }

    /// Parses a normalized `Edm.*` spelling into a variant; unrecognized
    /// spellings are preserved in [`FieldType::Unknown`].
    #[must_use]
    pub fn from_normalized(raw: &str) -> Self {
        use FieldType as T;
        match raw {
            "Edm.String" => T::String,
            "Edm.Int32" => T::Int32,
            "Edm.Int64" => T::Int64,
            "Edm.Single" => T::Single,
            "Edm.Double" => T::Double,
            "Edm.Boolean" => T::Boolean,
            "Edm.DateTimeOffset" => T::DateTimeOffset,
            "Edm.Guid" => T::Guid,
            "Edm.GeographyPoint" => T::GeographyPoint,
            // The SDKs serialize Int8 as `Edm.SByte`; accept both spellings.
            "Edm.Int8" | "Edm.SByte" => T::Int8,
            "Edm.Int16" => T::Int16,
            "Edm.Time" => T::Time,
            "Edm.Duration" => T::Duration,
            "Edm.Binary" => T::Binary,
            "Edm.ComplexType" => T::ComplexType,
            "Edm.Half" => T::Half,
            "Edm.Collection(Edm.String)" => T::CollectionString,
            "Edm.Collection(Edm.Int32)" => T::CollectionInt32,
            "Edm.Collection(Edm.Int64)" => T::CollectionInt64,
            "Edm.Collection(Edm.Int8)" | "Edm.Collection(Edm.SByte)" => T::CollectionInt8,
            "Edm.Collection(Edm.Int16)" => T::CollectionInt16,
            "Edm.Collection(Edm.Binary)" => T::CollectionBinary,
            "Edm.Collection(Edm.Single)" => T::CollectionSingle,
            "Edm.Collection(Edm.Double)" => T::CollectionDouble,
            "Edm.Collection(Edm.Half)" => T::CollectionHalf,
            "Edm.Collection(Edm.Boolean)" => T::CollectionBoolean,
            "Edm.Collection(Edm.DateTimeOffset)" => T::CollectionDateTimeOffset,
            "Edm.Collection(Edm.Guid)" => T::CollectionGuid,
            "Edm.Collection(Edm.ComplexType)" => T::CollectionComplexType,
            other => T::Unknown(other.to_owned()),
        }
    }

    /// The element type for a collection field, `None` for scalar types.
    #[must_use]
    pub fn inner_type(&self) -> Option<Self> {
        use FieldType as T;
        match self {
            T::CollectionString => Some(T::String),
            T::CollectionInt32 => Some(T::Int32),
            T::CollectionInt64 => Some(T::Int64),
            T::CollectionSingle => Some(T::Single),
            T::CollectionDouble => Some(T::Double),
            T::CollectionHalf => Some(T::Half),
            T::CollectionBoolean => Some(T::Boolean),
            T::CollectionDateTimeOffset => Some(T::DateTimeOffset),
            T::CollectionGuid => Some(T::Guid),
            T::CollectionInt8 => Some(T::Int8),
            T::CollectionInt16 => Some(T::Int16),
            T::CollectionBinary => Some(T::Binary),
            T::CollectionComplexType => Some(T::ComplexType),
            _ => None,
        }
    }

    /// Whether this is a collection (`Edm.Collection(...)`) type.
    #[must_use]
    pub fn is_collection(&self) -> bool {
        use FieldType as T;
        matches!(
            self,
            T::CollectionString
                | T::CollectionInt32
                | T::CollectionInt64
                | T::CollectionSingle
                | T::CollectionDouble
                | T::CollectionHalf
                | T::CollectionBoolean
                | T::CollectionDateTimeOffset
                | T::CollectionGuid
                | T::CollectionInt8
                | T::CollectionInt16
                | T::CollectionBinary
                | T::CollectionComplexType
        )
    }

    /// Whether this is a complex type (scalar or collection of complex).
    #[must_use]
    pub fn is_complex_type(&self) -> bool {
        matches!(
            self,
            FieldType::ComplexType | FieldType::CollectionComplexType
        )
    }

    /// Whether this type supports only `eq`/`ne` filters (ordering
    /// comparisons are ambiguous): `Edm.Duration` and `Edm.Binary` (scalar
    /// or collection forms).
    #[must_use]
    pub fn is_equality_only(&self) -> bool {
        use FieldType as T;
        matches!(self, T::Duration | T::Binary | T::CollectionBinary)
    }

    /// Whether this type can back a vector field.
    #[must_use]
    pub fn is_vector_field_type(&self) -> bool {
        matches!(
            self,
            FieldType::CollectionSingle | FieldType::CollectionHalf
        )
    }
}

impl std::str::FromStr for FieldType {
    type Err = std::convert::Infallible;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_normalized(raw))
    }
}

/// A single field definition from an index schema.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: FieldType,
    pub is_key: bool,
    pub searchable: bool,
    pub filterable: bool,
    pub sortable: bool,
    pub facetable: bool,
    pub retrievable: bool,
    /// Whether the field value is persisted (available via `get_document` and
    /// `select`). `retrievable` controls search-result visibility; `stored`
    /// controls persistence. Both default to `true`.
    pub stored: bool,
    /// Declared vector dimensions (`dimensions`, or the SDK alias
    /// `vector_search_dimensions`). `Some` only when the property is present
    /// and a positive integer; the service layer validates range and
    /// consistency (using [`FieldDefinition::raw`] to distinguish "present
    /// but invalid" from "absent").
    pub vector_dimensions: Option<usize>,
    /// The `vectorSearchProfile` name (`vector_search_profile_name` SDK
    /// alias accepted).
    pub vector_search_profile: Option<String>,
    /// Subfields of an `Edm.ComplexType` field; empty for all other types.
    pub subfields: Vec<FieldDefinition>,
    /// The field's declared analyzer name (`analyzer` property); `None` means
    /// the index default (the emulator's English analyzer).
    pub analyzer: Option<String>,
    /// Synonym maps associated with this field (`synonymMaps` property, as
    /// sent by the SDKs via `synonym_map_names` / `SynonymMapNames`). Only
    /// these maps plus the index-level `synonymMaps` are applied to full-text
    /// search on this index.
    pub synonym_maps: Vec<String>,
    /// The raw JSON field definition, preserved for echo in responses.
    pub raw: Value,
}

impl FieldDefinition {
    /// Parses a field definition from its raw JSON representation.
    ///
    /// # Errors
    ///
    /// Returns an error string if the value is not an object or is missing a
    /// non-empty `name` or `type`.
    pub fn from_json(raw: Value) -> Result<Self, String> {
        let obj = raw
            .as_object()
            .ok_or_else(|| "field definition must be a JSON object".to_owned())?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "field is missing a non-empty \"name\"".to_owned())?;
        let field_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("field {name:?} is missing a non-empty \"type\""))?;
        let mut subfields = Vec::new();
        if let Some(subfields_raw) = obj.get("fields").and_then(Value::as_array) {
            for subfield in subfields_raw {
                subfields.push(FieldDefinition::from_json(subfield.clone())?);
            }
        }
        // Vector markers: `dimensions` (SDK alias `vector_search_dimensions`)
        // and `vectorSearchProfile` (SDK alias `vector_search_profile_name`).
        // Parsed leniently here; the service layer validates range and
        // consistency and reports `400 InvalidIndex` diagnostics.
        let vector_dimensions = obj
            .get("dimensions")
            .or_else(|| obj.get("vector_search_dimensions"))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0);
        let vector_search_profile = obj
            .get("vectorSearchProfile")
            .or_else(|| obj.get("vector_search_profile_name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let analyzer = get_opt_string(obj, "analyzer");
        let synonym_maps = obj
            .get("synonymMaps")
            .or_else(|| obj.get("synonym_map_names"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(FieldDefinition {
            name: name.to_owned(),
            field_type: FieldType::from_normalized(&normalize_field_type(field_type)),
            vector_dimensions,
            vector_search_profile,
            is_key: get_bool(obj, "key", false),
            searchable: get_bool(obj, "searchable", false),
            filterable: get_bool(obj, "filterable", false),
            sortable: get_bool(obj, "sortable", false),
            facetable: get_bool(obj, "facetable", false),
            retrievable: get_bool(obj, "retrievable", true),
            stored: get_bool(obj, "stored", true),
            subfields,
            analyzer,
            synonym_maps,
            raw,
        })
    }

    /// Whether this field is a vector field: `Collection(Edm.Single)` or
    /// `Collection(Edm.Half)` (quantized) with declared `dimensions`. Fields
    /// merely *attempting* to be vector fields (e.g. `dimensions` present but
    /// invalid, or a profile without dimensions) are caught by service-level
    /// schema validation.
    #[must_use]
    pub fn is_vector_field(&self) -> bool {
        self.field_type.is_vector_field_type() && self.vector_dimensions.is_some()
    }

    /// Whether the raw definition carries a `dimensions` property (under
    /// either the REST or SDK key), even when its value is invalid. Used by
    /// schema validation to report precise `400 InvalidIndex` errors.
    #[must_use]
    pub fn has_dimensions_property(&self) -> bool {
        let Some(obj) = self.raw.as_object() else {
            return false;
        };
        obj.contains_key("dimensions") || obj.contains_key("vector_search_dimensions")
    }

    /// Whether this field is a complex type: a single `Edm.ComplexType`
    /// object or a collection of them (`Edm.Collection(Edm.ComplexType)`).
    /// Both carry subfields and share the same validation and indexing rules.
    #[must_use]
    pub fn is_complex_type(&self) -> bool {
        self.field_type.is_complex_type()
    }

    /// Whether this field is a collection type (`Edm.Collection(...)`).
    #[must_use]
    pub fn is_collection(&self) -> bool {
        self.field_type.is_collection()
    }
}

/// Normalizes a field type to the canonical `Edm.*` form. The SDK serializes
/// collection types as `Collection(Edm.String)` (without the `Edm.` prefix),
/// while the documented REST form is `Edm.Collection(Edm.String)`; both are
/// accepted and normalized so downstream code only handles one shape. The raw
/// definition is still echoed verbatim in responses.
fn normalize_field_type(field_type: &str) -> String {
    if let Some(inner) = field_type
        .strip_prefix("Collection(")
        .and_then(|s| s.strip_suffix(')'))
    {
        format!("Edm.Collection({inner})")
    } else {
        field_type.to_owned()
    }
}

/// Reads an optional boolean property, returning `default` when the key is
/// absent or its value is not a boolean.
fn get_bool(obj: &Map<String, Value>, key: &str, default: bool) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Reads an optional non-empty string property; `None` when the key is absent,
/// its value is not a string, or the string is empty.
fn get_opt_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Collects per-field `synonymMaps` (including nested complex subfields)
/// into `out`. The SDK wire format attaches synonym maps to searchable
/// fields; the emulator unions them with the index-level array.
fn collect_field_synonym_maps(field: &FieldDefinition, out: &mut Vec<String>) {
    out.extend(field.synonym_maps.iter().cloned());
    for subfield in &field.subfields {
        collect_field_synonym_maps(subfield, out);
    }
}

/// A suggester: a named set of searchable fields that the autocomplete and
/// suggest operations match against.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggester {
    pub name: String,
    /// Field names (or complex-type paths) the suggester searches.
    pub search_fields: Vec<String>,
}

impl Suggester {
    /// Parses a suggester definition from its raw JSON representation.
    ///
    /// The search fields are read from `searchFields` (the documented REST
    /// key) or `sourceFields` (the key the pinned Python SDK serializes);
    /// `searchFields` wins when both are present.
    ///
    /// # Errors
    ///
    /// Returns an error string if the value is not an object, is missing a
    /// non-empty `name` or a search-fields array, or any entry is not a
    /// non-empty string.
    pub fn from_json(raw: &Value) -> Result<Self, String> {
        let obj = raw
            .as_object()
            .ok_or_else(|| "suggester definition must be a JSON object".to_owned())?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "suggester is missing a non-empty \"name\"".to_owned())?;
        let fields_raw = obj
            .get("searchFields")
            .or_else(|| obj.get("sourceFields"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "suggester {name:?} is missing a \"searchFields\" (or \"sourceFields\") array"
                )
            })?;
        let mut search_fields = Vec::with_capacity(fields_raw.len());
        for field in fields_raw {
            let field_name = field.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                format!("suggester {name:?} search fields entries must be non-empty strings")
            })?;
            search_fields.push(field_name.to_owned());
        }
        Ok(Suggester {
            name: name.to_owned(),
            search_fields,
        })
    }
}

/// An index definition (schema).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexDefinition {
    pub name: String,
    pub fields: Vec<FieldDefinition>,
    /// Suggesters declared on the index; used by the autocomplete and suggest
    /// operations.
    pub suggesters: Vec<Suggester>,
    /// The raw `vectorSearch` object (`vector_search` SDK alias accepted),
    /// when present. Parsed and validated by the service layer (via the
    /// `vector` module); preserved here so the vector engine can build its
    /// per-field indexes from the stored definition.
    pub vector_search: Option<Value>,
    /// The names of the synonym maps associated with the index (the
    /// `synonymMaps` array). Only these maps are applied to full-text search
    /// on this index; maps not referenced here are inert for it.
    pub synonym_maps: Vec<String>,
    /// The `semantic` block, when present. Parsed and validated by the
    /// service layer; preserved here so the search pipeline can access the
    /// configuration at query time.
    pub semantic: Option<SemanticConfig>,
    /// The raw JSON index definition, preserved for echo in responses.
    pub raw: Value,
}

impl IndexDefinition {
    /// Parses an index definition from its raw JSON representation.
    ///
    /// # Errors
    ///
    /// Returns an error string if the value is not an object, is missing a
    /// non-empty `name` or a non-empty `fields` array, or any field is invalid.
    pub fn from_json(raw: Value) -> Result<Self, String> {
        let obj = raw
            .as_object()
            .ok_or_else(|| "index definition must be a JSON object".to_owned())?;
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "index is missing a non-empty \"name\"".to_owned())?;
        let fields_raw = obj
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| "index is missing a \"fields\" array".to_owned())?;
        if fields_raw.is_empty() {
            return Err("index must define at least one field".to_owned());
        }
        let mut fields = Vec::with_capacity(fields_raw.len());
        for field in fields_raw {
            fields.push(FieldDefinition::from_json(field.clone())?);
        }
        let mut suggesters = Vec::new();
        if let Some(suggesters_raw) = obj.get("suggesters").and_then(Value::as_array) {
            for suggester in suggesters_raw {
                suggesters.push(Suggester::from_json(suggester)?);
            }
        }
        let vector_search = obj
            .get("vectorSearch")
            .or_else(|| obj.get("vector_search"))
            .cloned();
        let mut synonym_maps: Vec<String> = obj
            .get("synonymMaps")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // Per-field `synonymMaps` (the SDK wire format: `synonym_map_names` /
        // `SynonymMapNames` serialize as `synonymMaps` on the field) are
        // unioned with the index-level array so both shapes associate maps
        // with the index. Nested complex subfields are included.
        for field in &fields {
            collect_field_synonym_maps(field, &mut synonym_maps);
        }
        synonym_maps.sort();
        synonym_maps.dedup();
        let semantic = match obj.get("semantic") {
            None | Some(Value::Null) => None,
            Some(raw) => crate::semantic::SemanticConfig::from_json(raw, &fields)?,
        };
        Ok(IndexDefinition {
            name: name.to_owned(),
            fields,
            suggesters,
            vector_search,
            synonym_maps,
            semantic,
            raw,
        })
    }

    #[must_use]
    pub fn key_field(&self) -> Option<&FieldDefinition> {
        self.fields.iter().find(|f| f.is_key)
    }

    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldDefinition> {
        self.fields.iter().find(|f| f.name == name)
    }

    #[must_use]
    pub fn suggester(&self, name: &str) -> Option<&Suggester> {
        self.suggesters.iter().find(|s| s.name == name)
    }

    /// Resolves a field path (`Address/StateProvince`) through complex-type
    /// subfields. A plain name resolves exactly like [`field`](Self::field).
    #[must_use]
    pub fn field_path(&self, path: &str) -> Option<&FieldDefinition> {
        let (first, rest) = path_segments(path);
        let mut current = self.fields.iter().find(|f| f.name == first)?;
        for segment in rest {
            current = current.subfields.iter().find(|f| f.name == segment)?;
        }
        Some(current)
    }
}

/// Splits a field path (`Address/City`) into its first segment and the
/// remaining segments. The first segment is always present (an empty path
/// yields an empty first segment).
fn path_segments(path: &str) -> (&str, std::str::Split<'_, char>) {
    let mut segments = path.split('/');
    let first = segments.next().unwrap_or("");
    (first, segments)
}

/// A document stored in an index, keyed by the index's key field.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub key: String,
    pub fields: Map<String, Value>,
}

impl Document {
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    /// Resolves a field path against this document's field map; see
    /// [`resolve_field_path`] for the resolution semantics.
    #[must_use]
    pub fn resolve_path(&self, path: &str) -> Vec<&Value> {
        resolve_field_path(&self.fields, path)
    }
}

/// Resolves a field path (`Address/City`, or a plain field name) against a
/// document's field map, walking into complex-type objects. When a segment
/// resolves to a JSON array (a collection field or a collection-of-complex
/// field), the remaining path is resolved against every element, so a
/// collection-of-complex path yields one value per element. A plain
/// (non-collection) path yields at most one value.
#[must_use]
pub fn resolve_field_path<'a>(fields: &'a Map<String, Value>, path: &str) -> Vec<&'a Value> {
    let (first, rest) = path_segments(path);
    let mut current = match fields.get(first) {
        Some(value) => vec![value],
        None => return Vec::new(),
    };
    for segment in rest {
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

/// Errors produced by the storage layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("index {0:?} already exists")]
    IndexAlreadyExists(String),
    #[error("index {0:?} not found")]
    IndexNotFound(String),
}

/// The storage abstraction from the initial design.
///
/// Implementations must be safe for concurrent use: concurrent requests must
/// not corrupt state.
pub trait Storage: Send + Sync {
    /// Creates an index.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexAlreadyExists`] if the index exists.
    fn create_index(&self, index: &IndexDefinition) -> Result<(), StorageError>;

    /// Creates or replaces an index, discarding any existing documents.
    fn upsert_index(&self, index: &IndexDefinition);

    /// Returns a clone of the index definition, if present.
    #[must_use]
    fn get_index(&self, name: &str) -> Option<IndexDefinition>;

    /// Deletes an index, returning `true` if it existed.
    fn delete_index(&self, name: &str) -> bool;

    /// Returns the names of all indexes, sorted.
    #[must_use]
    fn list_index_names(&self) -> Vec<String>;

    /// Stores documents, upserting by key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexNotFound`] if the index does not exist.
    fn put_documents(&self, index: &str, documents: Vec<Document>) -> Result<(), StorageError>;

    /// Returns a clone of the document with the given key, if present.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexNotFound`] if the index does not exist.
    fn get_document(&self, index: &str, key: &str) -> Result<Option<Document>, StorageError>;

    /// Deletes the documents with the given keys.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexNotFound`] if the index does not exist.
    fn delete_documents(&self, index: &str, keys: &[String]) -> Result<(), StorageError>;

    /// Returns all documents in the index, ordered by key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::IndexNotFound`] if the index does not exist.
    fn get_documents(&self, index: &str) -> Result<Vec<Document>, StorageError>;

    /// Clears all indexes and documents.
    fn reset(&self);
}

/// In-memory storage guarded by a read/write lock.
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    inner: std::sync::RwLock<BTreeMap<String, IndexEntry>>,
}

#[derive(Debug)]
struct IndexEntry {
    definition: IndexDefinition,
    documents: BTreeMap<String, Document>,
}

impl InMemoryStorage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStorage {
    fn create_index(&self, index: &IndexDefinition) -> Result<(), StorageError> {
        let mut indexes = write_unpoisoned(&self.inner);
        if indexes.contains_key(&index.name) {
            return Err(StorageError::IndexAlreadyExists(index.name.clone()));
        }
        indexes.insert(
            index.name.clone(),
            IndexEntry {
                definition: index.clone(),
                documents: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn upsert_index(&self, index: &IndexDefinition) {
        let mut indexes = write_unpoisoned(&self.inner);
        indexes.insert(
            index.name.clone(),
            IndexEntry {
                definition: index.clone(),
                documents: BTreeMap::new(),
            },
        );
    }

    fn get_index(&self, name: &str) -> Option<IndexDefinition> {
        let indexes = read_unpoisoned(&self.inner);
        indexes.get(name).map(|entry| entry.definition.clone())
    }

    fn delete_index(&self, name: &str) -> bool {
        let mut indexes = write_unpoisoned(&self.inner);
        indexes.remove(name).is_some()
    }

    fn list_index_names(&self) -> Vec<String> {
        let indexes = read_unpoisoned(&self.inner);
        indexes.keys().cloned().collect()
    }

    fn put_documents(&self, index: &str, documents: Vec<Document>) -> Result<(), StorageError> {
        let mut indexes = write_unpoisoned(&self.inner);
        let entry = indexes
            .get_mut(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        for document in documents {
            entry.documents.insert(document.key.clone(), document);
        }
        Ok(())
    }

    fn get_document(&self, index: &str, key: &str) -> Result<Option<Document>, StorageError> {
        let indexes = read_unpoisoned(&self.inner);
        let entry = indexes
            .get(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        Ok(entry.documents.get(key).cloned())
    }

    fn delete_documents(&self, index: &str, keys: &[String]) -> Result<(), StorageError> {
        let mut indexes = write_unpoisoned(&self.inner);
        let entry = indexes
            .get_mut(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        for key in keys {
            entry.documents.remove(key);
        }
        Ok(())
    }

    fn get_documents(&self, index: &str) -> Result<Vec<Document>, StorageError> {
        // Clones under the read lock are intentional: handing out owned
        // documents keeps readers lock-free for the (longer) search that
        // follows, at the cost of one clone per document. The emulator is
        // in-memory with small test-sized indexes, so this is cheap.
        let indexes = read_unpoisoned(&self.inner);
        let entry = indexes
            .get(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        Ok(entry.documents.values().cloned().collect())
    }

    fn reset(&self) {
        let mut indexes = write_unpoisoned(&self.inner);
        indexes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{err, ok};
    use serde_json::json;

    fn test_index(name: &str) -> IndexDefinition {
        ok(IndexDefinition::from_json(json!({
            "name": name,
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true}
            ]
        })))
    }

    fn doc(key: &str, title: &str) -> Document {
        let mut fields = Map::new();
        fields.insert("id".to_owned(), Value::String(key.to_owned()));
        fields.insert("title".to_owned(), Value::String(title.to_owned()));
        Document {
            key: key.to_owned(),
            fields,
        }
    }

    #[test]
    fn create_get_delete_index() {
        let storage = InMemoryStorage::new();
        let index = test_index("items");
        ok(storage.create_index(&index));
        assert_eq!(storage.get_index("items").as_ref(), Some(&index));
        assert!(storage.delete_index("items"));
        assert!(!storage.delete_index("items"));
        assert!(storage.get_index("items").is_none());
    }

    #[test]
    fn duplicate_index_rejected() {
        let storage = InMemoryStorage::new();
        let index = test_index("items");
        ok(storage.create_index(&index));
        let storage_error = err(storage.create_index(&index));
        assert_eq!(
            storage_error,
            StorageError::IndexAlreadyExists("items".to_owned())
        );
    }

    #[test]
    fn documents_round_trip_and_ordered_by_key() {
        let storage = InMemoryStorage::new();
        ok(storage.create_index(&test_index("items")));
        ok(storage.put_documents("items", vec![doc("3", "c"), doc("1", "a"), doc("2", "b")]));
        let docs = ok(storage.get_documents("items"));
        let keys: Vec<_> = docs.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, vec!["1", "2", "3"]);
    }

    #[test]
    fn put_documents_unknown_index_fails() {
        let storage = InMemoryStorage::new();
        let storage_error = err(storage.put_documents("missing", vec![doc("1", "a")]));
        assert_eq!(
            storage_error,
            StorageError::IndexNotFound("missing".to_owned())
        );
    }

    #[test]
    fn reset_clears_all_state() {
        let storage = InMemoryStorage::new();
        ok(storage.create_index(&test_index("items")));
        ok(storage.put_documents("items", vec![doc("1", "a")]));
        storage.reset();
        assert!(storage.list_index_names().is_empty());
    }

    #[test]
    fn index_definition_parsing_validates() {
        assert!(IndexDefinition::from_json(json!({"name": "x"})).is_err());
        assert!(IndexDefinition::from_json(json!({"name": "x", "fields": []})).is_err());
        assert!(IndexDefinition::from_json(json!({
            "name": "x",
            "fields": [{"name": "id"}]
        }))
        .is_err());
        let index = test_index("x");
        let key = index
            .key_field()
            .unwrap_or_else(|| panic!("missing key field"));
        assert_eq!(key.name, "id");
        let title = index
            .field("title")
            .unwrap_or_else(|| panic!("missing title field"));
        assert!(title.searchable);
    }
}
