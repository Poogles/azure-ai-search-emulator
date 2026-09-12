//! Storage abstraction for index definitions and documents.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// A single field definition from an index schema.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: String,
    pub is_key: bool,
    pub searchable: bool,
    pub filterable: bool,
    pub sortable: bool,
    pub facetable: bool,
    pub retrievable: bool,
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
        let analyzer = obj
            .get("analyzer")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        Ok(FieldDefinition {
            name: name.to_owned(),
            field_type: normalize_field_type(field_type),
            vector_dimensions,
            vector_search_profile,
            is_key: obj.get("key").and_then(Value::as_bool).unwrap_or(false),
            searchable: obj
                .get("searchable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            filterable: obj
                .get("filterable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            sortable: obj
                .get("sortable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            facetable: obj
                .get("facetable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            retrievable: obj
                .get("retrievable")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            subfields,
            analyzer,
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
        matches!(
            self.field_type.as_str(),
            "Edm.Collection(Edm.Single)" | "Edm.Collection(Edm.Half)"
        ) && self.vector_dimensions.is_some()
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
        Ok(IndexDefinition {
            name: name.to_owned(),
            fields,
            suggesters,
            vector_search,
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
        let mut segments = path.split('/');
        let first = segments.next()?;
        let mut current = self.fields.iter().find(|f| f.name == first)?;
        for segment in segments {
            current = current.subfields.iter().find(|f| f.name == segment)?;
        }
        Some(current)
    }
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
}

/// Errors produced by the storage layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    IndexAlreadyExists(String),
    IndexNotFound(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::IndexAlreadyExists(name) => write!(f, "index {name:?} already exists"),
            StorageError::IndexNotFound(name) => write!(f, "index {name:?} not found"),
        }
    }
}

impl std::error::Error for StorageError {}

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
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        indexes.insert(
            index.name.clone(),
            IndexEntry {
                definition: index.clone(),
                documents: BTreeMap::new(),
            },
        );
    }

    fn get_index(&self, name: &str) -> Option<IndexDefinition> {
        let indexes = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        indexes.get(name).map(|entry| entry.definition.clone())
    }

    fn delete_index(&self, name: &str) -> bool {
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        indexes.remove(name).is_some()
    }

    fn list_index_names(&self) -> Vec<String> {
        let indexes = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        indexes.keys().cloned().collect()
    }

    fn put_documents(&self, index: &str, documents: Vec<Document>) -> Result<(), StorageError> {
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = indexes
            .get_mut(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        for document in documents {
            entry.documents.insert(document.key.clone(), document);
        }
        Ok(())
    }

    fn get_document(&self, index: &str, key: &str) -> Result<Option<Document>, StorageError> {
        let indexes = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = indexes
            .get(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        Ok(entry.documents.get(key).cloned())
    }

    fn delete_documents(&self, index: &str, keys: &[String]) -> Result<(), StorageError> {
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = indexes
            .get_mut(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        for key in keys {
            entry.documents.remove(key);
        }
        Ok(())
    }

    fn get_documents(&self, index: &str) -> Result<Vec<Document>, StorageError> {
        let indexes = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = indexes
            .get(index)
            .ok_or_else(|| StorageError::IndexNotFound(index.to_owned()))?;
        Ok(entry.documents.values().cloned().collect())
    }

    fn reset(&self) {
        let mut indexes = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        indexes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(err) => panic!("expected Ok, got Err: {err:?}"),
        }
    }

    fn err<T, E: std::fmt::Debug>(result: Result<T, E>) -> E {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(err) => err,
        }
    }

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
