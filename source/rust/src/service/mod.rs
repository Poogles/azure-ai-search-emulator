//! Domain / service layer: index lifecycle, document indexing (upload, merge,
//! merge-or-upload, delete), and search (full-text, filter, ordering,
//! projection, facets, paging with continuation tokens).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE as BASE64_URL_SAFE};
use base64::Engine as _;
use serde_json::{Map, Value};

use crate::error::ApiError;
use crate::filter::{self, FilterExpr};
use crate::query::{
    parse_search_text, Clause, FullTextQuery, QueryError, SearchEngine, SearchMode,
};
use crate::storage::{
    Document, FieldDefinition, IndexDefinition, Storage, StorageError, Suggester,
};
use crate::vector::{parse_vector_search, vector_query_hash, VectorEngine};

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
    "Edm.Collection(Edm.Boolean)",
    "Edm.Collection(Edm.DateTimeOffset)",
    "Edm.Collection(Edm.Guid)",
];

/// Per-document result of an indexing operation, in the response shape
/// expected by the pinned Python SDK (`azure-search-documents==11.6.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexingResultItem {
    pub key: String,
    pub succeeded: bool,
    pub status_code: u16,
    pub error_message: Option<String>,
}

impl IndexingResultItem {
    pub fn to_value(&self) -> Value {
        Value::Object({
            let mut map = Map::new();
            map.insert("key".to_owned(), Value::String(self.key.clone()));
            map.insert("status".to_owned(), Value::Bool(self.succeeded));
            map.insert(
                "statusCode".to_owned(),
                Value::from(u64::from(self.status_code)),
            );
            map.insert(
                "errorMessage".to_owned(),
                self.error_message
                    .clone()
                    .map_or(Value::Null, Value::String),
            );
            map
        })
    }
}

/// A document batch action, parsed from the wire format by the API layer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentAction {
    pub kind: ActionKind,
    /// The document fields (for `delete`, only the key field is meaningful).
    pub document: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Upload,
    Merge,
    MergeOrUpload,
    Delete,
}

/// A parsed search request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    pub search: Option<String>,
    /// How multi-term required clauses combine (`searchMode`).
    pub search_mode: SearchMode,
    pub count: bool,
    pub top: Option<u64>,
    pub skip: u64,
    pub filter: Option<FilterExpr>,
    /// The raw `filter` string, preserved for continuation tokens.
    pub filter_raw: Option<String>,
    pub orderby: Vec<OrderBy>,
    /// The raw `orderby` string, preserved for continuation tokens.
    pub orderby_raw: Option<String>,
    pub select: Vec<String>,
    pub facets: Vec<Facet>,
    pub search_fields: Vec<SearchField>,
    /// Fields to highlight (`highlight`); empty means no highlighting.
    pub highlight_fields: Vec<String>,
    /// Tags wrapping highlighted terms (`highlightPreTag` / `highlightPostTag`).
    pub highlight_pre_tag: String,
    pub highlight_post_tag: String,
    pub continuation: Option<String>,
    /// Parsed `vectorQueries` entries (wire shape; SDK key aliases already
    /// resolved).
    pub vector_queries: Vec<VectorQuery>,
    /// The raw `vectorQueries` array, preserved to bind continuation tokens
    /// to the vector query identity.
    pub vector_queries_raw: Option<Value>,
    pub vector_filter_mode: VectorFilterMode,
}

/// One parsed `searchFields` entry: a field name with its score boost from an
/// optional `field^N` weight (default `1.0`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchField {
    pub name: String,
    pub boost: f32,
}

/// One parsed `vectorQueries[]` entry: a raw-vector kNN query over one or
/// more vector fields. `weight` is accepted but inert (no weighted fusion;
/// see `docs/known_differences.md`).
#[derive(Debug, Clone, PartialEq)]
pub struct VectorQuery {
    pub fields: Vec<String>,
    pub vector: Vec<f32>,
    pub k: usize,
    pub exhaustive: bool,
}

/// The top-level `vectorFilterMode`: `postFilter` (default) retrieves top-k
/// by similarity then applies `filter`; `preFilter` constrains candidates to
/// `filter` matches before top-k.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VectorFilterMode {
    #[default]
    PostFilter,
    PreFilter,
}

impl VectorFilterMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            VectorFilterMode::PostFilter => "postFilter",
            VectorFilterMode::PreFilter => "preFilter",
        }
    }
}

/// A document-key predicate for `preFilter` vector search: whether the
/// document with the given key matches the top-level filter.
type KeyPredicate<'a> = Box<dyn Fn(&str) -> bool + 'a>;

/// One `facets` entry: a field (or the special `$count`) with an optional
/// limit on the number of returned facet values (`count:N` / `top:N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facet {
    pub field: String,
    pub limit: Option<usize>,
}

/// One `orderby` clause: a field and its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub field: String,
    pub descending: bool,
}

/// The result of a search operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub total: u64,
    pub documents: Vec<Document>,
    /// Per-document `@search.score` values keyed by document key: BM25
    /// relevance scores for full-text matches (higher is more relevant),
    /// emulator-defined similarity scores for vector matches, best-score-wins
    /// for hybrid matches. Absent keys default to `1.0` at serialization.
    pub scores: BTreeMap<String, f32>,
    /// Per-document `@search.highlights` values keyed by document key: each
    /// maps a highlight field to its highlighted fragments. Empty when no
    /// `highlight` fields were requested.
    pub highlights: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// The `@search.facets` object, when facets were requested.
    pub facets: Option<Value>,
    /// Whether more results exist beyond the returned page.
    pub has_more: bool,
    /// The `skip` value for the next page.
    pub next_skip: u64,
}

/// A single autocomplete completion: the completed term and the query with
/// the completed term appended.
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteCompletion {
    pub text: String,
    pub query_plus_text: String,
}

/// A single suggestion: a matched document plus the word that matched.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub document: Document,
    pub text: String,
}

/// A synonym map: a named collection of synonym rules in Solr format.
/// Synonym maps are stored and echoed but inert: they do not affect search
/// results (see `docs/known_differences.md`).
#[derive(Debug, Clone, PartialEq)]
pub struct SynonymMap {
    pub name: String,
    /// Always `"solr"`; the only format Azure supports.
    pub format: String,
    /// The synonym rules joined by newlines (the wire format the SDKs use).
    pub synonyms: String,
    /// Opaque entity tag, bumped on every create or update.
    pub etag: String,
}

impl SynonymMap {
    /// The JSON representation returned by the synonym-map routes.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object({
            let mut map = Map::new();
            map.insert("name".to_owned(), Value::String(self.name.clone()));
            map.insert("format".to_owned(), Value::String(self.format.clone()));
            map.insert("synonyms".to_owned(), Value::String(self.synonyms.clone()));
            map.insert("@odata.etag".to_owned(), Value::String(self.etag.clone()));
            map
        })
    }
}

/// A stored named resource (an index alias, knowledge source, or knowledge
/// base). The raw request body is preserved and echoed back verbatim (with an
/// `@odata.etag`) so SDK round-trips (`create` -> `get`) agree without the
/// emulator having to model every resource's full schema.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedResource {
    pub name: String,
    /// Opaque entity tag, bumped on every create or update.
    pub etag: String,
    raw: Value,
}

impl NamedResource {
    /// The JSON representation returned by the resource routes: the stored
    /// request body with an `@odata.etag` added.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = self.raw.as_object().cloned().unwrap_or_default();
        map.insert("@odata.etag".to_owned(), Value::String(self.etag.clone()));
        Value::Object(map)
    }

    /// The alias target: the first entry of the stored `indexes` array.
    /// Returns `None` when the resource has no usable target (only aliases
    /// carry an `indexes` array; other resources always return `None`).
    #[must_use]
    pub fn target_index(&self) -> Option<String> {
        self.raw
            .get("indexes")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    }
}

/// Service-level storage for a collection of named resources, keyed by name
/// (sorted), with an incrementing etag counter. Mirrors the synonym-map
/// storage pattern.
#[derive(Debug, Default)]
struct ResourceStore {
    items: std::sync::RwLock<std::collections::BTreeMap<String, NamedResource>>,
    etags: AtomicU64,
}

impl ResourceStore {
    fn lock_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<String, NamedResource>> {
        self.items
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, std::collections::BTreeMap<String, NamedResource>> {
        self.items
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Creates a new resource, failing if one with the same name exists.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if the name is already taken.
    fn create(&self, name: &str, raw: &Value, code: &str) -> Result<NamedResource, ApiError> {
        let mut items = self.lock_write();
        if items.contains_key(name) {
            return Err(ApiError::conflict(
                code,
                format!("A resource with name {name:?} already exists."),
            ));
        }
        Ok(self.insert(&mut items, name, raw))
    }

    /// Creates or replaces a resource. Replacing issues a new etag.
    fn create_or_update(&self, name: &str, raw: &Value) -> NamedResource {
        let mut items = self.lock_write();
        self.insert(&mut items, name, raw)
    }

    /// Returns a clone of the resource with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    fn get(&self, name: &str, kind: &str) -> Result<NamedResource, ApiError> {
        self.lock_read()
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("{kind} {name:?} was not found.")))
    }

    /// Returns clones of all resources, sorted by name.
    #[must_use]
    fn list(&self) -> Vec<NamedResource> {
        self.lock_read().values().cloned().collect()
    }

    /// Returns `true` when a resource with the given name exists.
    #[must_use]
    fn contains(&self, name: &str) -> bool {
        self.lock_read().contains_key(name)
    }

    /// Deletes a resource by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    fn delete(&self, name: &str, kind: &str) -> Result<(), ApiError> {
        let mut items = self.lock_write();
        if items.remove(name).is_some() {
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "{kind} {name:?} was not found."
            )))
        }
    }

    fn clear(&self) {
        self.lock_write().clear();
    }

    fn insert(
        &self,
        items: &mut std::collections::BTreeMap<String, NamedResource>,
        name: &str,
        raw: &Value,
    ) -> NamedResource {
        // Azure etags are quoted hex strings (e.g. `"0x8D..."`); SDKs echo
        // them back in `If-Match`, so match the shape, not just uniqueness.
        // +1 so the first etag is non-zero, as Azure's (timestamp-derived) are.
        let etag = format!(
            "\"0x{:08X}\"",
            self.etags.fetch_add(1, Ordering::SeqCst) + 1
        );
        let resource = NamedResource {
            name: name.to_owned(),
            etag,
            raw: raw.clone(),
        };
        items.insert(name.to_owned(), resource.clone());
        resource
    }
}

/// A continuation token: the opaque URL-safe `base64(json{filter, orderby,
/// skip, vector_query_hash?})` value carried in `@odata.nextLink` and returned
/// by the client in the `continuation` request parameter. URL-safe encoding
/// keeps the token intact inside query strings (`+`/`/` would otherwise be
/// mangled); decoding still accepts the legacy standard alphabet. Tokens remain
/// valid across document mutations (like Azure; results may shift).
/// `vector_query_hash` binds the token to the `vectorQueries` +
/// `vectorFilterMode` identity when vector search is active.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContinuationToken {
    pub filter: Option<String>,
    pub orderby: Option<String>,
    pub skip: u64,
    #[serde(default)]
    pub vector_query_hash: Option<u64>,
}

impl ContinuationToken {
    #[must_use]
    pub fn encode(&self) -> String {
        // Serialization of this plain-data struct cannot fail; fall back to an
        // empty object (which decodes but carries no paging state) rather than
        // panicking in request handling.
        let json = serde_json::to_string(self).unwrap_or_default();
        BASE64_URL_SAFE.encode(json)
    }

    /// # Errors
    ///
    /// Returns an error string when the token is not valid base64 JSON with
    /// the expected shape.
    pub fn decode(raw: &str) -> Result<Self, String> {
        let bytes = BASE64_URL_SAFE
            .decode(raw.as_bytes())
            .or_else(|_| BASE64.decode(raw.as_bytes()))
            .map_err(|e| format!("continuation token is not valid base64: {e}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|e| format!("continuation token is not valid UTF-8: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("continuation token is not valid JSON: {e}"))
    }
}

pub struct SearchService {
    storage: Arc<dyn Storage>,
    engine: Arc<SearchEngine>,
    vectors: Arc<VectorEngine>,
    /// Cap on accepted vector dimensions (`EMULATOR_VECTOR__MAX_DIMENSION`,
    /// default 3072).
    max_vector_dimension: usize,
    /// Service-level synonym maps, keyed by name (sorted).
    synonym_maps: std::sync::RwLock<std::collections::BTreeMap<String, SynonymMap>>,
    /// Counter for generated synonym-map etags.
    synonym_map_etags: AtomicU64,
    /// Service-level index aliases, keyed by name (sorted).
    aliases: ResourceStore,
    /// Service-level knowledge sources, keyed by name (sorted).
    knowledge_sources: ResourceStore,
    /// Service-level knowledge bases, keyed by name (sorted).
    knowledge_bases: ResourceStore,
}

impl SearchService {
    pub fn new(
        storage: Arc<dyn Storage>,
        engine: Arc<SearchEngine>,
        vectors: Arc<VectorEngine>,
        max_vector_dimension: usize,
    ) -> Self {
        Self {
            storage,
            engine,
            vectors,
            max_vector_dimension,
            synonym_maps: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            synonym_map_etags: AtomicU64::new(0),
            aliases: ResourceStore::default(),
            knowledge_sources: ResourceStore::default(),
            knowledge_bases: ResourceStore::default(),
        }
    }

    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }

    /// Creates an index from a raw Azure `SearchIndex` definition, returning
    /// the echoed definition on success.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is malformed, the schema uses
    /// unsupported field types, or an index with the same name already exists.
    pub fn create_index(&self, raw: &Value) -> Result<Value, ApiError> {
        let definition = parse_index_definition(raw)?;
        validate_schema(&definition, self.max_vector_dimension)?;
        // Index and alias names share the data-plane namespace (aliases
        // resolve where index names are accepted), so neither may shadow the
        // other.
        if self.aliases.contains(&definition.name) {
            return Err(ApiError::conflict(
                "IndexAlreadyExists",
                format!("An alias with name {:?} already exists.", definition.name),
            ));
        }
        match self.storage.create_index(&definition) {
            Ok(()) => {
                if let Err(e) = self
                    .engine
                    .create_index(&definition.name, &definition.fields)
                {
                    // Roll back storage: this index did not exist before, so
                    // removing it restores the prior state and avoids
                    // storage/engine divergence.
                    self.storage.delete_index(&definition.name);
                    return Err(engine_error(&definition.name, e));
                }
                if let Err(message) = self.create_vector_indexes(&definition) {
                    self.storage.delete_index(&definition.name);
                    self.engine.delete_index(&definition.name);
                    return Err(ApiError::bad_request("InvalidIndex", message));
                }
                Ok(definition.raw.clone())
            }
            Err(StorageError::IndexAlreadyExists(name)) => Err(ApiError::conflict(
                "IndexAlreadyExists",
                format!("An index with name {name:?} already exists."),
            )),
            Err(StorageError::IndexNotFound(name)) => Err(ApiError::not_found(format!(
                "Index {name:?} was not found."
            ))),
        }
    }

    /// Creates or replaces an index from a raw Azure `SearchIndex` definition,
    /// returning the echoed definition. Replacing an index discards its
    /// documents.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is malformed or the schema
    /// uses unsupported field types.
    pub fn create_or_update_index(&self, raw: &Value) -> Result<Value, ApiError> {
        let definition = parse_index_definition(raw)?;
        validate_schema(&definition, self.max_vector_dimension)?;
        if self.aliases.contains(&definition.name) {
            return Err(ApiError::conflict(
                "IndexAlreadyExists",
                format!("An alias with name {:?} already exists.", definition.name),
            ));
        }
        self.storage.upsert_index(&definition);
        // Replacing an index discards its documents, so rebuild the search index.
        self.engine.delete_index(&definition.name);
        if let Err(e) = self
            .engine
            .create_index(&definition.name, &definition.fields)
        {
            // Engine rebuild failed; remove the upserted index so storage and
            // engine stay consistent (both absent). The caller can retry.
            self.storage.delete_index(&definition.name);
            return Err(engine_error(&definition.name, e));
        }
        // Replacing an index discards its vectors as well.
        self.vectors.delete_index(&definition.name);
        if let Err(message) = self.create_vector_indexes(&definition) {
            self.storage.delete_index(&definition.name);
            self.engine.delete_index(&definition.name);
            return Err(ApiError::bad_request("InvalidIndex", message));
        }
        Ok(definition.raw.clone())
    }

    /// Returns the raw stored index definition.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn get_index(&self, name: &str) -> Result<Value, ApiError> {
        Ok(self.require_index(name)?.raw)
    }

    /// Returns the raw stored definitions of all indexes, sorted by name.
    #[must_use]
    pub fn list_indexes(&self) -> Vec<Value> {
        self.storage
            .list_index_names()
            .into_iter()
            .filter_map(|name| self.storage.get_index(&name).map(|def| def.raw))
            .collect()
    }

    /// Deletes an index by name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn delete_index(&self, name: &str) -> Result<(), ApiError> {
        if self.storage.delete_index(name) {
            self.engine.delete_index(name);
            self.vectors.delete_index(name);
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "Index {name:?} was not found."
            )))
        }
    }

    /// Builds the per-field vector indexes for a validated definition. The
    /// schema validator has already checked profiles and dimensions, so a
    /// failure here is defensive.
    fn create_vector_indexes(&self, definition: &IndexDefinition) -> Result<(), String> {
        let fields: Vec<(String, usize, String)> = definition
            .fields
            .iter()
            .filter(|f| f.is_vector_field())
            .map(|f| {
                (
                    f.name.clone(),
                    f.vector_dimensions.unwrap_or(0),
                    f.vector_search_profile.clone().unwrap_or_default(),
                )
            })
            .collect();
        if fields.is_empty() && definition.vector_search.is_none() {
            return Ok(());
        }
        self.vectors
            .create_index(&definition.name, definition.vector_search.as_ref(), &fields)
    }

    /// Creates a new synonym map.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is invalid (`400
    /// InvalidSynonymMap`) or a map with the same name exists (`409
    /// SynonymMapAlreadyExists`).
    pub fn create_synonym_map(
        &self,
        name: &str,
        format: &str,
        synonyms: &str,
    ) -> Result<SynonymMap, ApiError> {
        validate_synonym_map(name, format, synonyms)?;
        let mut maps = self
            .synonym_maps
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if maps.contains_key(name) {
            return Err(ApiError::conflict(
                "SynonymMapAlreadyExists",
                format!("A synonym map with name {name:?} already exists."),
            ));
        }
        Ok(self.insert_synonym_map(&mut maps, name, format, synonyms))
    }

    /// Creates or replaces a synonym map. Replacing a map issues a new etag.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is invalid (`400
    /// InvalidSynonymMap`).
    pub fn create_or_update_synonym_map(
        &self,
        name: &str,
        format: &str,
        synonyms: &str,
    ) -> Result<SynonymMap, ApiError> {
        validate_synonym_map(name, format, synonyms)?;
        let mut maps = self
            .synonym_maps
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self.insert_synonym_map(&mut maps, name, format, synonyms))
    }

    /// Returns a clone of the synonym map with the given name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the map does not exist.
    pub fn get_synonym_map(&self, name: &str) -> Result<SynonymMap, ApiError> {
        let maps = self
            .synonym_maps
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        maps.get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Synonym map {name:?} was not found.")))
    }

    /// Returns clones of all synonym maps, sorted by name.
    #[must_use]
    pub fn list_synonym_maps(&self) -> Vec<SynonymMap> {
        let maps = self
            .synonym_maps
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        maps.values().cloned().collect()
    }

    /// Deletes a synonym map by name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the map does not exist.
    pub fn delete_synonym_map(&self, name: &str) -> Result<(), ApiError> {
        let mut maps = self
            .synonym_maps
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if maps.remove(name).is_some() {
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "Synonym map {name:?} was not found."
            )))
        }
    }

    fn insert_synonym_map(
        &self,
        maps: &mut std::collections::BTreeMap<String, SynonymMap>,
        name: &str,
        format: &str,
        synonyms: &str,
    ) -> SynonymMap {
        let etag = self
            .synonym_map_etags
            .fetch_add(1, Ordering::SeqCst)
            .to_string();
        let map = SynonymMap {
            name: name.to_owned(),
            format: format.to_owned(),
            synonyms: synonyms.to_owned(),
            etag,
        };
        maps.insert(name.to_owned(), map.clone());
        map
    }

    // ------------------------------------------------------------------
    // Index aliases
    // ------------------------------------------------------------------

    /// Creates a new index alias.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if an alias with the same name exists,
    /// or if an index with the same name exists (the two share the data-plane
    /// namespace, so neither may shadow the other).
    pub fn create_alias(&self, name: &str, raw: &Value) -> Result<NamedResource, ApiError> {
        self.reject_alias_index_collision(name)?;
        self.aliases.create(name, raw, "AliasAlreadyExists")
    }

    /// Creates or replaces an index alias.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if an index with the same name exists.
    pub fn create_or_update_alias(
        &self,
        name: &str,
        raw: &Value,
    ) -> Result<NamedResource, ApiError> {
        self.reject_alias_index_collision(name)?;
        Ok(self.aliases.create_or_update(name, raw))
    }

    fn reject_alias_index_collision(&self, name: &str) -> Result<(), ApiError> {
        if self.storage.get_index(name).is_some() {
            return Err(ApiError::conflict(
                "AliasAlreadyExists",
                format!("An index with name {name:?} already exists."),
            ));
        }
        Ok(())
    }

    /// Returns a clone of the alias with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the alias does not exist.
    pub fn get_alias(&self, name: &str) -> Result<NamedResource, ApiError> {
        self.aliases.get(name, "Alias")
    }

    /// Returns clones of all aliases, sorted by name.
    #[must_use]
    pub fn list_aliases(&self) -> Vec<NamedResource> {
        self.aliases.list()
    }

    /// Deletes an alias by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the alias does not exist.
    pub fn delete_alias(&self, name: &str) -> Result<(), ApiError> {
        self.aliases.delete(name, "Alias")
    }

    // ------------------------------------------------------------------
    // Knowledge sources
    // ------------------------------------------------------------------

    /// Creates a new knowledge source.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if a source with the same name exists.
    pub fn create_knowledge_source(
        &self,
        name: &str,
        raw: &Value,
    ) -> Result<NamedResource, ApiError> {
        self.knowledge_sources
            .create(name, raw, "KnowledgeSourceAlreadyExists")
    }

    /// Creates or replaces a knowledge source.
    pub fn create_or_update_knowledge_source(&self, name: &str, raw: &Value) -> NamedResource {
        self.knowledge_sources.create_or_update(name, raw)
    }

    /// Returns a clone of the knowledge source with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the source does not exist.
    pub fn get_knowledge_source(&self, name: &str) -> Result<NamedResource, ApiError> {
        self.knowledge_sources.get(name, "Knowledge source")
    }

    /// Returns clones of all knowledge sources, sorted by name.
    #[must_use]
    pub fn list_knowledge_sources(&self) -> Vec<NamedResource> {
        self.knowledge_sources.list()
    }

    /// Deletes a knowledge source by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the source does not exist.
    pub fn delete_knowledge_source(&self, name: &str) -> Result<(), ApiError> {
        self.knowledge_sources.delete(name, "Knowledge source")
    }

    // ------------------------------------------------------------------
    // Knowledge bases
    // ------------------------------------------------------------------

    /// Creates a new knowledge base.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if a base with the same name exists.
    pub fn create_knowledge_base(
        &self,
        name: &str,
        raw: &Value,
    ) -> Result<NamedResource, ApiError> {
        self.knowledge_bases
            .create(name, raw, "KnowledgeBaseAlreadyExists")
    }

    /// Creates or replaces a knowledge base.
    pub fn create_or_update_knowledge_base(&self, name: &str, raw: &Value) -> NamedResource {
        self.knowledge_bases.create_or_update(name, raw)
    }

    /// Returns a clone of the knowledge base with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the base does not exist.
    pub fn get_knowledge_base(&self, name: &str) -> Result<NamedResource, ApiError> {
        self.knowledge_bases.get(name, "Knowledge base")
    }

    /// Returns clones of all knowledge bases, sorted by name.
    #[must_use]
    pub fn list_knowledge_bases(&self) -> Vec<NamedResource> {
        self.knowledge_bases.list()
    }

    /// Deletes a knowledge base by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the base does not exist.
    pub fn delete_knowledge_base(&self, name: &str) -> Result<(), ApiError> {
        self.knowledge_bases.delete(name, "Knowledge base")
    }

    /// Returns a single document by key.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index or the document does not exist.
    pub fn get_document(&self, index: &str, key: &str) -> Result<Value, ApiError> {
        let definition = self.require_index(index)?;
        match self
            .storage
            .get_document(&definition.name, key)
            .map_err(|e| ApiError::not_found(e.to_string()))?
        {
            Some(document) => Ok(document.to_value()),
            None => Err(ApiError::not_found(format!(
                "Document with key {key:?} was not found in index {index:?}."
            ))),
        }
    }

    /// Returns the number of documents in the index.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn count_documents(&self, index: &str) -> Result<u64, ApiError> {
        let definition = self.require_index(index)?;
        let docs = self
            .storage
            .get_documents(&definition.name)
            .map_err(|e| ApiError::not_found(e.to_string()))?;
        Ok(docs.len() as u64)
    }

    /// Validates and applies a batch of document actions (upload, merge,
    /// merge-or-upload, delete), returning one result per action in request
    /// order. Valid actions in the same batch are applied even when others
    /// fail.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    pub fn index_documents(
        &self,
        index: &str,
        actions: Vec<DocumentAction>,
    ) -> Result<Vec<IndexingResultItem>, ApiError> {
        let definition = self.require_index(index)?;
        let key_name = key_field_name(&definition);
        // The final state of each key is its *last* action in the batch: a key
        // appears in at most one of the outcome sets, so the search engine,
        // the vector indexes, and storage converge on the same outcome.
        let mut batch = DocumentBatch::new(actions.len());

        for action in actions {
            self.apply_document_action(&definition, index, &key_name, &action, &mut batch)?;
        }

        if !batch.upserts.is_empty() || !batch.deletes.is_empty() {
            // The per-key sets are disjoint (last action wins), so every
            // backend converges on the same outcome. Engine changes apply
            // first so storage is untouched if the engine rejects the batch.
            // All mutations target the resolved index name so aliases write
            // through to their target.
            let upserts: Vec<Document> = batch.upserts.into_values().collect();
            let deletes: Vec<String> = batch.deletes.into_iter().collect();
            if let Err(e) = self.apply_engine_changes(&definition.name, &upserts, &deletes) {
                return Err(engine_error(&definition.name, e));
            }
            if let Err(message) = self.apply_vector_changes(&definition, &upserts, &deletes) {
                return Err(ApiError::internal(format!(
                    "Vector indexing failed for index {:?}: {message}",
                    definition.name
                )));
            }
            if !upserts.is_empty() {
                self.storage
                    .put_documents(&definition.name, upserts)
                    .map_err(|e| ApiError::not_found(e.to_string()))?;
            }
            if !deletes.is_empty() {
                self.storage
                    .delete_documents(&definition.name, &deletes)
                    .map_err(|e| ApiError::not_found(e.to_string()))?;
            }
        }
        Ok(batch.results)
    }

    /// Applies one document action to the batch's per-key working sets,
    /// appending its per-action result. Merge and delete observe keys upserted
    /// earlier in the same batch, so in-batch sequences (upload-then-delete,
    /// upload-then-merge) resolve in request order with the last action
    /// winning.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist.
    fn apply_document_action(
        &self,
        definition: &IndexDefinition,
        index: &str,
        key_name: &str,
        action: &DocumentAction,
        batch: &mut DocumentBatch,
    ) -> Result<(), ApiError> {
        let key = action
            .document
            .get(key_name)
            .and_then(key_display)
            .unwrap_or_default();
        match action.kind {
            ActionKind::Upload => match validate_document(definition, &action.document) {
                Ok(doc) => {
                    batch.record_upsert(doc);
                    batch.results.push(ok_result(key, 201));
                }
                Err(message) => batch.results.push(fail_result(key, 400, message)),
            },
            ActionKind::Merge => {
                match self.merge_one(definition, key_name, &action.document, &batch.upserts) {
                    MergeOutcome::Applied(doc) => {
                        batch.record_upsert(doc);
                        batch.results.push(ok_result(key, 200));
                    }
                    MergeOutcome::Missing => batch.results.push(fail_result(
                        key.clone(),
                        404,
                        format!("Document with key {key:?} was not found in index {index:?}."),
                    )),
                    MergeOutcome::Invalid(message) => {
                        batch.results.push(fail_result(key, 400, message));
                    }
                }
            }
            ActionKind::MergeOrUpload => {
                match self.merge_one(definition, key_name, &action.document, &batch.upserts) {
                    MergeOutcome::Applied(doc) => {
                        batch.record_upsert(doc);
                        batch.results.push(ok_result(key, 200));
                    }
                    MergeOutcome::Missing => {
                        match validate_document(definition, &action.document) {
                            Ok(doc) => {
                                batch.record_upsert(doc);
                                batch.results.push(ok_result(key, 201));
                            }
                            Err(message) => batch.results.push(fail_result(key, 400, message)),
                        }
                    }
                    MergeOutcome::Invalid(message) => {
                        batch.results.push(fail_result(key, 400, message));
                    }
                }
            }
            ActionKind::Delete => {
                // A key upserted earlier in the same batch counts as present,
                // so upload-then-delete resolves to "deleted".
                let present = batch.upserts.contains_key(&key)
                    || self
                        .storage
                        .get_document(&definition.name, &key)
                        .map_err(|e| ApiError::not_found(e.to_string()))?
                        .is_some();
                if present {
                    batch.deletes.insert(key.clone());
                    batch.upserts.remove(&key);
                    batch.results.push(ok_result(key, 200));
                } else {
                    batch.results.push(fail_result(
                        key.clone(),
                        404,
                        format!("Document with key {key:?} was not found in index {index:?}."),
                    ));
                }
            }
        }
        Ok(())
    }

    fn apply_engine_changes(
        &self,
        index: &str,
        upserts: &[Document],
        deletes: &[String],
    ) -> Result<(), QueryError> {
        if upserts.is_empty() && deletes.is_empty() {
            return Ok(());
        }
        // Delete first so a key that is both deleted and re-uploaded in the
        // same batch ends up indexed.
        if !deletes.is_empty() {
            self.engine.delete_documents(index, deletes)?;
        }
        if !upserts.is_empty() {
            self.engine.index_documents(index, upserts)?;
        }
        Ok(())
    }

    /// Applies validated upserts/deletes to the vector indexes. Vectors are
    /// validated before this point, so a failure here is an internal error.
    /// A vector field absent from an upserted document drops any previously
    /// indexed vector for that (key, field) pair (full-replace semantics).
    fn apply_vector_changes(
        &self,
        definition: &IndexDefinition,
        upserts: &[Document],
        deletes: &[String],
    ) -> Result<(), String> {
        let vector_fields: Vec<&FieldDefinition> = definition
            .fields
            .iter()
            .filter(|f| f.is_vector_field())
            .collect();
        if vector_fields.is_empty() {
            return Ok(());
        }
        if !deletes.is_empty() {
            self.vectors.delete_documents(&definition.name, deletes);
        }
        if upserts.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::new();
        let mut removals = Vec::new();
        for document in upserts {
            for field in &vector_fields {
                match document.fields.get(&field.name) {
                    Some(value) => {
                        let Some(items) = value.as_array() else {
                            return Err(format!(
                                "Field {:?} must be an array of numbers.",
                                field.name
                            ));
                        };
                        let mut vector = Vec::with_capacity(items.len());
                        for item in items {
                            match item.as_f64().and_then(finite_f32) {
                                Some(narrowed) => vector.push(narrowed),
                                None => {
                                    return Err(format!(
                                        "Field {:?} must contain only finite numeric values.",
                                        field.name
                                    ));
                                }
                            }
                        }
                        entries.push((document.key.clone(), field.name.clone(), vector));
                    }
                    None => {
                        removals.push((document.key.clone(), field.name.clone()));
                    }
                }
            }
        }
        // Removals first so a re-uploaded key ends up indexed.
        self.vectors.remove_entries(&definition.name, &removals);
        self.vectors.upsert_documents(&definition.name, &entries)?;
        Ok(())
    }

    fn merge_one(
        &self,
        definition: &IndexDefinition,
        key_name: &str,
        document: &Value,
        pending: &BTreeMap<String, Document>,
    ) -> MergeOutcome {
        let Some(key) = document.get(key_name).and_then(key_display) else {
            return MergeOutcome::Invalid(format!(
                "Document is missing the key field {key_name:?}."
            ));
        };
        // A key upserted earlier in the same batch merges against the pending
        // document, so in-batch upload-then-merge sees the upload.
        let existing = if let Some(doc) = pending.get(&key) {
            Some(doc.clone())
        } else {
            match self.storage.get_document(&definition.name, &key) {
                Ok(doc) => doc,
                Err(e) => return MergeOutcome::Invalid(e.to_string()),
            }
        };
        match existing {
            Some(existing) => match merge_fields(&existing, document) {
                Some(merged) => match validate_document(definition, &merged) {
                    Ok(doc) => MergeOutcome::Applied(doc),
                    Err(message) => MergeOutcome::Invalid(message),
                },
                None => MergeOutcome::Invalid("Merge document must be a JSON object.".to_owned()),
            },
            None => MergeOutcome::Missing,
        }
    }

    /// Parses and validates a search request body against the index schema.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist, the body is not a
    /// JSON object, an option is malformed, or a referenced field is missing
    /// or lacks the required attribute (`filterable`, `sortable`, `facetable`,
    /// `searchable`).
    pub fn parse_search(&self, index: &str, body: &Value) -> Result<SearchQuery, ApiError> {
        let definition = self.require_index(index)?;
        let obj = match body {
            Value::Object(map) => map,
            Value::Null => &Map::new(),
            _ => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    "Search request body must be a JSON object.",
                ))
            }
        };

        for key in UNSUPPORTED_SEARCH_OPTIONS {
            if obj.contains_key(*key) {
                return Err(ApiError::unsupported(
                    "UnsupportedQuery",
                    format!("The {key:?} search option is not supported by the emulator."),
                ));
            }
        }
        if let Some(query_type) = obj.get("queryType").and_then(Value::as_str) {
            if query_type != "simple" {
                return Err(ApiError::unsupported(
                    "UnsupportedQuery",
                    format!("queryType {query_type:?} is not supported by the emulator."),
                ));
            }
        }

        let search = obj.get("search").and_then(Value::as_str).map(str::to_owned);
        // The search text itself is parsed once, in `prepare_full_text` when
        // the search runs; malformed text surfaces there as `400 InvalidQuery`.
        // `searchMode`: `any` (OR, the default, matching Azure) or `all` (AND).
        let search_mode = parse_search_mode(obj)?;
        let (count, top, skip) = parse_paging_options(obj)?;

        let filter_raw = obj.get("filter").and_then(Value::as_str).map(str::to_owned);
        let filter = match &filter_raw {
            Some(raw) => Some(parse_filter_option(raw)?),
            None => None,
        };
        if let Some(expr) = &filter {
            filter::validate(expr, &definition).map_err(|e| {
                ApiError::bad_request("InvalidQuery", format!("Invalid filter: {e}"))
            })?;
        }

        let (orderby, orderby_raw) = match obj.get("orderby") {
            Some(value) => {
                let (parsed, raw) = parse_orderby(value, &definition)?;
                (parsed, Some(raw))
            }
            None => (Vec::new(), None),
        };

        let select = match obj.get("select") {
            Some(value) => parse_select(value, &definition)?,
            None => Vec::new(),
        };

        let facets = match obj.get("facets") {
            Some(value) => parse_facets(value, &definition)?,
            None => Vec::new(),
        };

        let search_fields = match obj.get("searchFields") {
            Some(value) => parse_search_fields(value, &definition)?,
            None => Vec::new(),
        };

        let (vector_queries, vector_queries_raw, vector_filter_mode) =
            parse_vector_options(obj, &definition)?;

        let continuation = obj
            .get("continuation")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let (highlight_fields, highlight_pre_tag, highlight_post_tag) =
            parse_highlight_options(obj, &definition)?;

        Ok(SearchQuery {
            search,
            search_mode,
            count,
            top,
            skip,
            filter,
            filter_raw,
            orderby,
            orderby_raw,
            select,
            facets,
            search_fields,
            highlight_fields,
            highlight_pre_tag,
            highlight_post_tag,
            continuation,
            vector_queries,
            vector_queries_raw,
            vector_filter_mode,
        })
    }

    /// Runs a search over an index: full-text match, vector similarity,
    /// hybrid union merge, filter, ordering, facets, and paging.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist, a continuation
    /// token is invalid or stale, or the vector queries changed mid-paging.
    pub fn search(&self, index: &str, query: &SearchQuery) -> Result<SearchOutcome, ApiError> {
        let definition = self.require_index(index)?;

        // The request's vector-query identity, bound into continuation tokens.
        let current_vector_hash: Option<u64> = query
            .vector_queries_raw
            .as_ref()
            .map(|raw| vector_query_hash(raw, query.vector_filter_mode.as_str()));

        // A continuation token is authoritative for skip/filter/orderby and
        // must reference the current document state.
        let (skip, filter, orderby) =
            Self::resolve_paging(query, &definition, current_vector_hash)?;

        let full_text = prepare_full_text(query)?;
        let vector_active = !query.vector_queries.is_empty();
        let full_text_active = !full_text.is_match_all();

        let documents = self
            .storage
            .get_documents(&definition.name)
            .map_err(|e| ApiError::not_found(e.to_string()))?;
        let doc_fields: BTreeMap<&str, &Map<String, Value>> = documents
            .iter()
            .map(|doc| (doc.key.as_str(), &doc.fields))
            .collect();

        let vector_scores = if vector_active {
            self.vector_side_scores(&definition.name, query, filter.as_ref(), &doc_fields)
        } else {
            BTreeMap::new()
        };

        // Full-text side: skipped only for vector-only searches (a match-all
        // query would otherwise drag every document into the union). The
        // top-level filter always applies here. Scores are BM25 relevance
        // scores from the query engine.
        let full_text_scores = if full_text_active || !vector_active {
            self.full_text_side_scores(&definition.name, &full_text, filter.as_ref(), &doc_fields)?
        } else {
            BTreeMap::new()
        };

        // Hybrid merge: union, best score wins.
        let mut merged: BTreeMap<String, f32> = vector_scores;
        for (key, score) in full_text_scores {
            merged
                .entry(key)
                .and_modify(|best| *best = best.max(score))
                .or_insert(score);
        }
        let doc_map: BTreeMap<&str, &Document> = documents
            .iter()
            .map(|doc| (doc.key.as_str(), doc))
            .collect();
        let mut scored: Vec<(Document, f32)> = merged
            .into_iter()
            .filter_map(|(key, score)| doc_map.get(key.as_str()).map(|doc| ((*doc).clone(), score)))
            .collect();
        order_scored(&mut scored, &orderby);

        let total = u64::try_from(scored.len()).unwrap_or(u64::MAX);
        let matched: Vec<Document> = scored.iter().map(|(doc, _)| doc.clone()).collect();
        let facets = if query.facets.is_empty() {
            None
        } else {
            Some(compute_facets(&matched, &query.facets))
        };

        let skip_usize = usize::try_from(skip).unwrap_or(usize::MAX);
        let take = query
            .top
            .and_then(|t| usize::try_from(t).ok())
            .unwrap_or(usize::MAX);
        let page: Vec<(Document, f32)> = scored.into_iter().skip(skip_usize).take(take).collect();
        // An empty page ends the sequence even when more documents exist
        // (e.g. `top=0`): otherwise the next token would encode the same skip
        // and a token-following client would loop forever on empty pages.
        let has_more = !page.is_empty()
            && u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX) < total;
        let next_skip = u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX);
        let page_scores: BTreeMap<String, f32> = page
            .iter()
            .map(|(doc, score)| (doc.key.clone(), *score))
            .collect();
        // Highlight fragments for the returned page, when `highlight`
        // fields were requested.
        let highlights = page_highlights(query, &full_text, &page);
        // Vectors with `retrievable: false` are searchable but omitted from
        // the response unless explicitly selected (same as Azure).
        let hidden: BTreeSet<&str> = definition
            .fields
            .iter()
            .filter(|f| f.is_vector_field() && !f.retrievable)
            .map(|f| f.name.as_str())
            .collect();
        let documents: Vec<Document> = page
            .into_iter()
            .map(|(mut doc, _)| {
                if !hidden.is_empty() {
                    doc.fields.retain(|name, _| {
                        !hidden.contains(name.as_str()) || query.select.iter().any(|s| s == name)
                    });
                }
                doc
            })
            .collect();
        Ok(SearchOutcome {
            total,
            documents,
            scores: page_scores,
            highlights,
            facets,
            has_more,
            next_skip,
        })
    }

    /// Resolves the effective `(skip, filter, orderby)` for a search. When a
    /// continuation token is present it encapsulates the result-set state, so
    /// its `skip`/`filter`/`orderby` win over the request parameters: paging
    /// can never mix states mid-sequence. Tokens remain valid across document
    /// mutations (like Azure; results may shift). The token must also match the
    /// request's vector-query identity.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`400 InvalidQuery`) when the token is invalid
    /// or the vector queries changed mid-paging.
    fn resolve_paging(
        query: &SearchQuery,
        definition: &IndexDefinition,
        current_vector_hash: Option<u64>,
    ) -> Result<(u64, Option<FilterExpr>, Vec<OrderBy>), ApiError> {
        let Some(raw) = &query.continuation else {
            return Ok((query.skip, query.filter.clone(), query.orderby.clone()));
        };
        let token = ContinuationToken::decode(raw).map_err(|e| {
            ApiError::bad_request("InvalidQuery", format!("Invalid continuation token: {e}"))
        })?;
        if (current_vector_hash.is_some() || token.vector_query_hash.is_some())
            && current_vector_hash != token.vector_query_hash
        {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                "Vector query changed during paging; restart the search.",
            ));
        }
        let filter = match &token.filter {
            Some(raw) => Some(parse_filter_option(raw)?),
            None => None,
        };
        let orderby = match &token.orderby {
            Some(raw) => parse_orderby(&Value::String(raw.clone()), definition)?.0,
            None => Vec::new(),
        };
        Ok((token.skip, filter, orderby))
    }

    /// Vector side of [`SearchService::search`]: the union of every
    /// (query × field) hit with the best score per document key.
    /// `preFilter` constrains candidates inside the scan; `postFilter` trims
    /// the retrieved top-k afterwards.
    fn vector_side_scores(
        &self,
        definition_name: &str,
        query: &SearchQuery,
        filter: Option<&FilterExpr>,
        doc_fields: &BTreeMap<&str, &Map<String, Value>>,
    ) -> BTreeMap<String, f32> {
        let pre_filter: Option<KeyPredicate<'_>> = match (&query.vector_filter_mode, filter) {
            (VectorFilterMode::PreFilter, Some(expr)) => Some(Box::new(|key: &str| {
                doc_fields
                    .get(key)
                    .is_some_and(|fields| expr.matches(fields))
            })),
            _ => None,
        };
        let mut scores: BTreeMap<String, f32> = BTreeMap::new();
        for vector_query in &query.vector_queries {
            for field in &vector_query.fields {
                let hits = self.vectors.search(
                    definition_name,
                    field,
                    &vector_query.vector,
                    vector_query.k,
                    vector_query.exhaustive,
                    pre_filter.as_deref(),
                );
                for (key, score) in hits {
                    scores
                        .entry(key)
                        .and_modify(|best| *best = best.max(score))
                        .or_insert(score);
                }
            }
        }
        if matches!(
            (&query.vector_filter_mode, filter),
            (VectorFilterMode::PostFilter, Some(_))
        ) {
            if let Some(expr) = filter {
                scores.retain(|key, _| {
                    doc_fields
                        .get(key.as_str())
                        .is_some_and(|fields| expr.matches(fields))
                });
            }
        }
        scores
    }

    /// Full-text side of [`SearchService::search`]: matching keys with BM25
    /// scores, with the top-level filter applied.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] when the query engine fails.
    fn full_text_side_scores(
        &self,
        index: &str,
        full_text: &FullTextQuery,
        filter: Option<&FilterExpr>,
        doc_fields: &BTreeMap<&str, &Map<String, Value>>,
    ) -> Result<BTreeMap<String, f32>, ApiError> {
        let matched_scores = self
            .engine
            .search(index, full_text)
            .map_err(|e| engine_error(index, e))?;
        let mut scored = BTreeMap::new();
        for (key, score) in matched_scores {
            let passes = filter.is_none_or(|expr| {
                doc_fields
                    .get(key.as_str())
                    .is_some_and(|fields| expr.matches(fields))
            });
            if passes {
                scored.insert(key, score);
            }
        }
        Ok(scored)
    }

    /// Builds the continuation token for the next page, if one exists. The
    /// token carries the effective paging state: when the request continued a
    /// previous token, that token's filter/orderby propagate forward (they
    /// are authoritative); otherwise the request's values start the sequence.
    /// This keeps clients that send only `{continuation}` on later pages on
    /// the same result set.
    #[must_use]
    pub fn next_continuation(
        &self,
        query: &SearchQuery,
        outcome: &SearchOutcome,
    ) -> Option<String> {
        if !outcome.has_more {
            return None;
        }
        let (filter, orderby) = match &query.continuation {
            Some(raw) => match ContinuationToken::decode(raw) {
                Ok(token) => (token.filter, token.orderby),
                Err(_) => (query.filter_raw.clone(), query.orderby_raw.clone()),
            },
            None => (query.filter_raw.clone(), query.orderby_raw.clone()),
        };
        let vector_query_hash = query
            .vector_queries_raw
            .as_ref()
            .map(|raw| vector_query_hash(raw, query.vector_filter_mode.as_str()));
        Some(
            ContinuationToken {
                filter,
                orderby,
                skip: outcome.next_skip,
                vector_query_hash,
            }
            .encode(),
        )
    }

    /// Runs an autocomplete query: case-insensitive prefix or infix matching
    /// of the search text against the whitespace-separated words of the
    /// suggester's search fields. Returns up to `top` distinct completions,
    /// ordered by first appearance (documents in key order, then field order).
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist, the suggester is
    /// not defined on the index, or the search text is empty.
    pub fn autocomplete(
        &self,
        index: &str,
        suggester_name: &str,
        search_text: &str,
        top: u64,
    ) -> Result<Vec<AutocompleteCompletion>, ApiError> {
        let (documents, suggester) = self.suggester_documents(index, suggester_name)?;
        let search = search_text.trim();
        let needle = search.to_lowercase();
        if needle.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                "The autocomplete search text must be a non-empty string.",
            ));
        }
        let limit = usize::try_from(top).unwrap_or(usize::MAX);
        let mut seen = std::collections::BTreeSet::new();
        let mut completions = Vec::new();
        for document in &documents {
            for field_name in &suggester.search_fields {
                for value in resolve_field_values(&document.fields, field_name) {
                    for word in field_words(value) {
                        if word.to_lowercase().contains(&needle) && seen.insert(word.clone()) {
                            completions.push(AutocompleteCompletion {
                                query_plus_text: format!("{search} {word}"),
                                text: word,
                            });
                            if completions.len() >= limit {
                                return Ok(completions);
                            }
                        }
                    }
                }
            }
        }
        Ok(completions)
    }

    /// Runs a suggest query: a document matches when any whitespace-separated
    /// word of the suggester's search fields contains the search text
    /// (case-insensitive prefix or infix match). Returns up to `top` matching
    /// documents in key order, each with the first matched word.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist, the suggester is
    /// not defined on the index, or the search text is empty.
    pub fn suggest(
        &self,
        index: &str,
        suggester_name: &str,
        search_text: &str,
        top: u64,
    ) -> Result<Vec<Suggestion>, ApiError> {
        let (documents, suggester) = self.suggester_documents(index, suggester_name)?;
        let needle = search_text.trim().to_lowercase();
        if needle.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                "The suggest search text must be a non-empty string.",
            ));
        }
        let limit = usize::try_from(top).unwrap_or(usize::MAX);
        let mut suggestions = Vec::new();
        for document in &documents {
            let mut matched = None;
            for field_name in &suggester.search_fields {
                for value in resolve_field_values(&document.fields, field_name) {
                    for word in field_words(value) {
                        if word.to_lowercase().contains(&needle) {
                            matched = Some(word);
                            break;
                        }
                    }
                    if matched.is_some() {
                        break;
                    }
                }
                if matched.is_some() {
                    break;
                }
            }
            if let Some(text) = matched {
                suggestions.push(Suggestion {
                    document: document.clone(),
                    text,
                });
                if suggestions.len() >= limit {
                    break;
                }
            }
        }
        Ok(suggestions)
    }

    /// Looks up the index and its suggester, returning the index's documents
    /// (in key order) and a clone of the suggester definition.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the index does not exist or the suggester
    /// is not defined on the index.
    fn suggester_documents(
        &self,
        index: &str,
        suggester_name: &str,
    ) -> Result<(Vec<Document>, Suggester), ApiError> {
        let definition = self.require_index(index)?;
        let suggester = definition
            .suggester(suggester_name)
            .cloned()
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    format!("Suggester {suggester_name:?} is not defined on index {index:?}."),
                )
            })?;
        let documents = self
            .storage
            .get_documents(&definition.name)
            .map_err(|e| ApiError::not_found(e.to_string()))?;
        Ok((documents, suggester))
    }

    pub fn reset(&self) {
        self.storage.reset();
        self.engine.reset();
        self.vectors.reset();
        let mut maps = self
            .synonym_maps
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        maps.clear();
        self.aliases.clear();
        self.knowledge_sources.clear();
        self.knowledge_bases.clear();
    }

    fn require_index(&self, name: &str) -> Result<IndexDefinition, ApiError> {
        let resolved = self.resolve_index_name(name);
        self.storage
            .get_index(&resolved)
            .ok_or_else(|| ApiError::not_found(format!("Index {name:?} was not found.")))
    }

    /// Resolves an index name through the alias table: when `name` is an
    /// alias, returns its target index (the first entry of the alias's
    /// `indexes` array); otherwise returns `name` unchanged. Data-plane
    /// routes (search, documents, suggest, autocomplete, analyze) accept an
    /// alias name anywhere an index name is accepted.
    fn resolve_index_name(&self, name: &str) -> String {
        let Ok(alias) = self.aliases.get(name, "Alias") else {
            return name.to_owned();
        };
        alias.target_index().unwrap_or_else(|| name.to_owned())
    }

    /// Analyzer names accepted by the analyze-text endpoint. `keyword` and
    /// `whitespace` tokenize as Azure documents them (single verbatim token;
    /// whitespace split without lowercasing); every other listed analyzer
    /// maps to the emulator's English analyzer (lowercasing, punctuation
    /// splitting, English stopword removal, English stemming). Unknown names
    /// are rejected explicitly rather than silently mapped.
    const KNOWN_ANALYZERS: &'static [&'static str] = &[
        "standard",
        "standard.lucene",
        "standard.asciiFolding",
        "keyword",
        "whitespace",
        "simple",
        "classic",
        "stop",
        "en.microsoft",
        "en.lucene",
    ];

    /// Validates analyze-text parameters against the index schema: `field`,
    /// when given, must exist in the schema; `analyzer`, when given, must be
    /// a known analyzer name (see [`SearchService::KNOWN_ANALYZERS`]).
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`404` for a missing index, `400
    /// InvalidRequest` for an unknown field or analyzer).
    pub fn validate_analyze(
        &self,
        index: &str,
        analyzer: Option<&str>,
        field: Option<&str>,
    ) -> Result<(), ApiError> {
        let definition = self.require_index(index)?;
        if let Some(name) = analyzer {
            if !Self::KNOWN_ANALYZERS.contains(&name) {
                return Err(ApiError::bad_request(
                    "InvalidRequest",
                    format!(
                        "Unknown analyzer {name:?}; supported analyzers: {}.",
                        Self::KNOWN_ANALYZERS.join(", ")
                    ),
                ));
            }
        }
        if let Some(name) = field {
            if definition.field_path(name).is_none() {
                return Err(ApiError::bad_request(
                    "InvalidRequest",
                    format!("Analyze field {name:?} does not exist in index {index:?}."),
                ));
            }
        }
        Ok(())
    }
}

/// The outcome of a single merge operation.
enum MergeOutcome {
    Applied(Document),
    Missing,
    Invalid(String),
}

/// Working state for one document batch: the per-key final outcome (a key
/// appears in at most one of `upserts` / `deletes` — its last action in the
/// batch wins) plus the per-action results in request order.
struct DocumentBatch {
    upserts: BTreeMap<String, Document>,
    deletes: BTreeSet<String>,
    results: Vec<IndexingResultItem>,
}

impl DocumentBatch {
    fn new(capacity: usize) -> Self {
        Self {
            upserts: BTreeMap::new(),
            deletes: BTreeSet::new(),
            results: Vec::with_capacity(capacity),
        }
    }

    /// Records an upsert in the batch's final per-key state: the document
    /// lands in `upserts` and any earlier delete for the same key is
    /// superseded.
    fn record_upsert(&mut self, doc: Document) {
        self.deletes.remove(&doc.key);
        self.upserts.insert(doc.key.clone(), doc);
    }
}

fn ok_result(key: String, status_code: u16) -> IndexingResultItem {
    IndexingResultItem {
        key,
        succeeded: true,
        status_code,
        error_message: None,
    }
}

fn fail_result(key: String, status_code: u16, message: String) -> IndexingResultItem {
    IndexingResultItem {
        key,
        succeeded: false,
        status_code,
        error_message: Some(message),
    }
}

/// Merges `update` into `existing`: scalar fields are overwritten, collection
/// fields are replaced wholesale (Azure merge semantics).
fn merge_fields(existing: &Document, update: &Value) -> Option<Value> {
    let update_obj = update.as_object()?;
    let mut merged = existing.fields.clone();
    for (key, value) in update_obj {
        merged.insert(key.clone(), value.clone());
    }
    Some(Value::Object(merged))
}

/// Orders scored `(document, score)` pairs: by `orderby` when given,
/// otherwise by score descending with the key field as tie-breaker so ranking
/// is deterministic. Match-all (unscored) queries carry equal scores, so they
/// stay in key order via the tie-breaker.
fn order_scored(scored: &mut [(Document, f32)], orderby: &[OrderBy]) {
    if orderby.is_empty() {
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.key.cmp(&b.0.key))
        });
    } else {
        scored.sort_by(|a, b| compare_scored(a, b, orderby));
    }
}

/// Compares two scored `(document, score)` pairs by the `orderby` clauses,
/// with the key field as the final tie-breaker so ordering is deterministic.
/// Missing values sort first in ascending order and last in descending order,
/// matching Azure's null ordering.
fn compare_scored(
    a: &(Document, f32),
    b: &(Document, f32),
    orderby: &[OrderBy],
) -> std::cmp::Ordering {
    for clause in orderby {
        let ordering = if clause.field == "@search.score" {
            let ordering = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
            if clause.descending {
                ordering.reverse()
            } else {
                ordering
            }
        } else {
            compare_field(&a.0, &b.0, &clause.field, clause.descending)
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    a.0.key.cmp(&b.0.key)
}

fn compare_field(a: &Document, b: &Document, field: &str, descending: bool) -> std::cmp::Ordering {
    let (av, bv) = (a.fields.get(field), b.fields.get(field));
    match (av, bv) {
        (None, None) => std::cmp::Ordering::Equal,
        // Azure sorts nulls first in ascending order (last in descending).
        (None, Some(_)) => {
            if descending {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (Some(_), None) => {
            if descending {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (Some(av), Some(bv)) => {
            let ordering = compare_values(av, bv);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
    }
}

fn compare_values(av: &Value, bv: &Value) -> std::cmp::Ordering {
    match (av, bv) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        // Mixed types: order by type tag so the result is deterministic.
        _ => type_tag(av).cmp(&type_tag(bv)),
    }
}

fn type_tag(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}

/// Computes facet counts over the full (filtered, ordered) result set: one
/// entry per distinct value, ordered by count descending then value ascending,
/// truncated to the facet's limit when one is given. The special `$count`
/// facet reports the total number of documents in the result set.
fn compute_facets(documents: &[Document], facets: &[Facet]) -> Value {
    let mut map = Map::new();
    for facet in facets {
        if facet.field == "$count" {
            map.insert(
                facet.field.clone(),
                Value::from(u64::try_from(documents.len()).unwrap_or(u64::MAX)),
            );
            continue;
        }
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for document in documents {
            let Some(value) = document.fields.get(&facet.field) else {
                continue;
            };
            let values = if value.is_array() {
                value.as_array().cloned().unwrap_or_default()
            } else {
                vec![value.clone()]
            };
            for value in values {
                if value.is_null() {
                    continue;
                }
                let key = facet_key(&value);
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        let mut entries: Vec<(String, u64)> = counts.into_iter().collect();
        entries.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        if let Some(limit) = facet.limit {
            entries.truncate(limit);
        }
        let items = entries
            .into_iter()
            .map(|(key, count)| {
                Value::Object({
                    let mut entry = Map::new();
                    entry.insert("value".to_owned(), facet_value(&key));
                    entry.insert("count".to_owned(), Value::from(count));
                    entry
                })
            })
            .collect();
        map.insert(facet.field.clone(), Value::Array(items));
    }
    Value::Object(map)
}

fn facet_key(value: &Value) -> String {
    match value {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        other => format!("o:{other}"),
    }
}

fn facet_value(key: &str) -> Value {
    match key.split_once(':') {
        Some(("s", rest)) => Value::String(rest.to_owned()),
        Some(("n", rest)) => rest
            .parse::<i64>()
            .ok()
            .map(Value::from)
            .or_else(|| rest.parse::<f64>().ok().map(Value::from))
            .unwrap_or_else(|| Value::String(rest.to_owned())),
        Some(("b", rest)) => Value::Bool(rest == "true"),
        _ => Value::String(key.to_owned()),
    }
}

fn parse_filter_option(raw: &str) -> Result<FilterExpr, ApiError> {
    filter::parse_filter(raw)
        .map_err(|e| ApiError::bad_request("InvalidQuery", format!("Invalid filter: {e}")))
}

/// Extracts a list of comma-separated items from a search option value. The
/// Azure REST API documents these as comma-separated strings, but SDKs may
/// send JSON arrays; both shapes are accepted.
fn string_items(value: &Value, name: &str) -> Result<Vec<String>, ApiError> {
    match value {
        Value::String(text) => Ok(text
            .split(',')
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(text) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_owned());
                        }
                    }
                    None => {
                        return Err(ApiError::bad_request(
                            "InvalidQuery",
                            format!("{name} must be a string or an array of strings."),
                        ))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(ApiError::bad_request(
            "InvalidQuery",
            format!("{name} must be a string or an array of strings."),
        )),
    }
}

/// Parses an `orderby` value: comma-separated `field [asc|desc]` clauses (or
/// a JSON array of clauses). Every field must exist and be marked `sortable`,
/// except the pseudo-field `@search.score`, which orders by relevance score.
/// Returns the parsed clauses and a canonical raw string for continuation
/// tokens.
fn parse_orderby(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<(Vec<OrderBy>, String), ApiError> {
    let parts = string_items(value, "orderby")?;
    let mut clauses = Vec::new();
    for part in &parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = part.split_whitespace().collect();
        let (field, direction) = match tokens.as_slice() {
            [field] => (*field, "asc"),
            [field, direction] => (*field, *direction),
            _ => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Invalid orderby clause {part:?}: expected 'field' or 'field asc|desc'."
                    ),
                ))
            }
        };
        if field.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Invalid orderby clause {part:?}: missing field name."),
            ));
        }
        let descending = match direction.to_ascii_lowercase().as_str() {
            "asc" => false,
            "desc" => true,
            other => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Invalid orderby clause {part:?}: expected direction 'asc' or 'desc', \
                         found {other:?}."
                    ),
                ))
            }
        };
        // `@search.score` is not a schema field: it orders by relevance score
        // (the default ranking when no `orderby` is given).
        if field == "@search.score" {
            clauses.push(OrderBy {
                field: field.to_owned(),
                descending,
            });
            continue;
        }
        let field_def = definition.field(field).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("orderby references unknown field {field:?}."),
            )
        })?;
        if !field_def.sortable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {field:?} is not sortable; mark it \"sortable\": true in the index schema."
                ),
            ));
        }
        clauses.push(OrderBy {
            field: field.to_owned(),
            descending,
        });
    }
    if clauses.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "orderby is empty."));
    }
    Ok((clauses, parts.join(", ")))
}

/// Parses a `select` value: comma-separated field names (or a JSON array),
/// each of which must exist in the schema. The special `*` selects every
/// field, exactly like omitting `select`.
fn parse_select(value: &Value, definition: &IndexDefinition) -> Result<Vec<String>, ApiError> {
    let items = string_items(value, "select")?;
    if items.iter().any(|item| item == "*") {
        return Ok(Vec::new());
    }
    let mut fields = Vec::new();
    for part in items {
        if definition.field(&part).is_none() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("select references unknown field {part:?}."),
            ));
        }
        fields.push(part);
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "select is empty."));
    }
    Ok(fields)
}

/// Parses a `facets` value: comma-separated entries (or a JSON array). Each
/// entry is a field name, the special `$count`, or `*` (all facetable fields),
/// optionally followed by `,count:N` (or `,top:N`) to limit the number of
/// returned facet values. Named fields must exist and be marked `facetable`.
fn parse_facets(value: &Value, definition: &IndexDefinition) -> Result<Vec<Facet>, ApiError> {
    // A single string is split on commas, with `count:N` / `top:N` fragments
    // re-attached to the preceding facet (so `"tags,count:1"` limits the
    // `tags` facet); array entries keep their inner commas intact.
    let entries: Vec<String> = match value {
        Value::String(text) => split_facet_string(text),
        _ => string_items(value, "facets")?,
    };
    let mut facets = Vec::new();
    for part in entries {
        parse_facet_entry(&part, definition, &mut facets)?;
    }
    if facets.is_empty() {
        return Err(ApiError::bad_request("InvalidQuery", "facets is empty."));
    }
    Ok(facets)
}

/// Splits a single-string `facets` value on commas, re-attaching `count:N` /
/// `top:N` fragments to the preceding facet entry. A leading option with no
/// facet passes through and is rejected as an unknown field downstream.
fn split_facet_string(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let is_option = part
            .split_once(':')
            .is_some_and(|(key, _)| matches!(key.trim(), "count" | "top"));
        if is_option {
            if let Some(last) = entries.last_mut() {
                last.push_str(", ");
                last.push_str(part);
                continue;
            }
        }
        entries.push(part.to_owned());
    }
    entries
}

/// Parses one facet entry (a field name, `$count`, or `*`, optionally with
/// `,count:N` / `,top:N` limits) against the index schema.
fn parse_facet_entry(
    part: &str,
    definition: &IndexDefinition,
    facets: &mut Vec<Facet>,
) -> Result<(), ApiError> {
    let mut pieces = part.split(',');
    let name = pieces.next().unwrap_or("").trim();
    let mut limit = None;
    for option in pieces {
        let option = option.trim();
        let Some((key, arg)) = option.split_once(':') else {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid facet option {option:?} in {part:?}; expected 'count:N' or 'top:N'."
                ),
            ));
        };
        let count: u64 = arg.trim().parse().map_err(|_| {
            ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid facet option {option:?} in {part:?}; the count must be a non-negative integer."
                ),
            )
        })?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        match key {
            "count" | "top" => limit = Some(n),
            other => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Unsupported facet option {other:?} in {part:?}; supported options: count:N, top:N."
                    ),
                ))
            }
        }
    }
    if name == "*" {
        for field in &definition.fields {
            if field.facetable {
                facets.push(Facet {
                    field: field.name.clone(),
                    limit,
                });
            }
        }
    } else if name == "$count" {
        if limit.is_some() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("The $count facet does not take options (got {part:?})."),
            ));
        }
        facets.push(Facet {
            field: "$count".to_owned(),
            limit: None,
        });
    } else {
        let field_def = definition.field(name).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("facets references unknown field {name:?}."),
            )
        })?;
        if !field_def.facetable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {name:?} is not facetable; mark it \"facetable\": true in the index schema."
                ),
            ));
        }
        facets.push(Facet {
            field: name.to_owned(),
            limit,
        });
    }
    Ok(())
}

/// Parses the `searchMode` option (`search_mode` SDK alias accepted): `any`
/// (OR, the default, matching Azure) or `all` (AND).
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the value is present but
/// not one of the two supported modes.
fn parse_search_mode(obj: &Map<String, Value>) -> Result<SearchMode, ApiError> {
    match obj.get("searchMode").or_else(|| obj.get("search_mode")) {
        None | Some(Value::Null) => Ok(SearchMode::default()),
        Some(Value::String(mode)) => {
            SearchMode::parse(mode).map_err(|e| ApiError::bad_request("InvalidQuery", e))
        }
        Some(_) => Err(ApiError::bad_request(
            "InvalidQuery",
            "searchMode must be 'all' or 'any'.",
        )),
    }
}

/// Parses the `count`/`top`/`skip` paging options: `top`/`skip` must be
/// non-negative integers when present.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when `top`/`skip` is
/// present but not a non-negative integer.
fn parse_paging_options(obj: &Map<String, Value>) -> Result<(bool, Option<u64>, u64), ApiError> {
    let count = obj.get("count").and_then(Value::as_bool).unwrap_or(false);
    let top = match obj.get("top") {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"top\" must be a non-negative integer.")
        })?),
    };
    let skip = match obj.get("skip") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| {
            ApiError::bad_request("InvalidQuery", "\"skip\" must be a non-negative integer.")
        })?,
    };
    Ok((count, top, skip))
}

/// Parses the top-level vector options: `vectorQueries`
/// (`vector_queries` SDK alias accepted) plus `vectorFilterMode`
/// (`vector_filter_mode` SDK alias accepted).
///
/// # Errors
///
/// Returns an [`ApiError`] when the filter mode or any vector query is
/// malformed (see [`parse_vector_filter_mode`], [`parse_vector_queries`]).
fn parse_vector_options(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<VectorQuery>, Option<Value>, VectorFilterMode), ApiError> {
    let vector_filter_mode = parse_vector_filter_mode(
        obj.get("vectorFilterMode")
            .or_else(|| obj.get("vector_filter_mode")),
    )?;
    let (vector_queries, vector_queries_raw) = parse_vector_queries(
        obj.get("vectorQueries")
            .or_else(|| obj.get("vector_queries")),
        definition,
    )?;
    Ok((vector_queries, vector_queries_raw, vector_filter_mode))
}

/// Maximum `vectorQueries` entries per search (matches Azure).
const MAX_VECTOR_QUERIES: usize = 5;
/// Maximum `k` per vector query (matches Azure).
const MAX_VECTOR_K: usize = 1000;
/// Default `k` when omitted (matches the SDK default).
const DEFAULT_VECTOR_K: usize = 3;

/// Parses the top-level `vectorFilterMode` (`vector_filter_mode` SDK alias
/// accepted): `postFilter` (default) or `preFilter`.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the value is present
/// but not one of the two supported modes.
fn parse_vector_filter_mode(value: Option<&Value>) -> Result<VectorFilterMode, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(VectorFilterMode::PostFilter),
        Some(Value::String(mode)) => match mode.as_str() {
            "postFilter" => Ok(VectorFilterMode::PostFilter),
            "preFilter" => Ok(VectorFilterMode::PreFilter),
            other => Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Invalid vectorFilterMode {other:?}; supported values: 'preFilter', 'postFilter'."
                ),
            )),
        },
        Some(_) => Err(ApiError::bad_request(
            "InvalidQuery",
            "vectorFilterMode must be 'preFilter' or 'postFilter'.",
        )),
    }
}

/// Parses the top-level `vectorQueries` array against the index schema,
/// returning the parsed queries plus the raw array (bound into continuation
/// tokens). SDK key aliases (`k_nearest_neighbors`, `vector_queries`) are
/// accepted; the service layer otherwise sees the wire shape only.
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`, or `400 UnsupportedQuery`
/// for `kind: "text"` vectorizer queries) when the array or any entry is
/// malformed.
fn parse_vector_queries(
    value: Option<&Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<VectorQuery>, Option<Value>), ApiError> {
    let Some(raw) = value else {
        return Ok((Vec::new(), None));
    };
    if raw.is_null() {
        return Ok((Vec::new(), None));
    }
    let entries = raw
        .as_array()
        .ok_or_else(|| ApiError::bad_request("InvalidQuery", "vectorQueries must be an array."))?;
    if entries.is_empty() {
        return Ok((Vec::new(), None));
    }
    if entries.len() > MAX_VECTOR_QUERIES {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            format!("At most {MAX_VECTOR_QUERIES} vector queries are supported."),
        ));
    }
    let mut queries = Vec::with_capacity(entries.len());
    for entry in entries {
        queries.push(parse_vector_query(entry, definition)?);
    }
    Ok((queries, Some(raw.clone())))
}

/// Parses one `vectorQueries[]` entry: `kind`, `vector`, `fields` (string or
/// array), `k` (`k_nearest_neighbors` SDK alias accepted), `exhaustive`.
/// `weight` is accepted but inert. A missing `kind` defaults to `"vector"`
/// (emulator-only leniency, documented in `known_differences.md`).
fn parse_vector_query(
    entry: &Value,
    definition: &IndexDefinition,
) -> Result<VectorQuery, ApiError> {
    let obj = entry.as_object().ok_or_else(|| {
        ApiError::bad_request(
            "InvalidQuery",
            "Each vectorQueries entry must be a JSON object.",
        )
    })?;
    match obj.get("kind").and_then(Value::as_str) {
        None | Some("vector") => {}
        Some("text") => {
            return Err(ApiError::unsupported(
                "UnsupportedQuery",
                "Vectorizer queries (kind 'text') are not supported; supply raw vectors with kind 'vector'.",
            ));
        }
        Some(other) => {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Invalid vector query kind {other:?}; supported kinds: 'vector'."),
            ));
        }
    }
    let (fields, expected) = parse_vector_query_fields(obj, definition)?;
    let vector = parse_vector_query_vector(obj, &fields, expected)?;
    let k = match obj
        .get("k")
        .or_else(|| obj.get("k_nearest_neighbors"))
        .or_else(|| obj.get("kNearestNeighbors"))
    {
        None | Some(Value::Null) => DEFAULT_VECTOR_K,
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n >= 1 && *n <= MAX_VECTOR_K)
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    "Vector query 'k' must be a positive integer (max 1000).",
                )
            })?,
    };
    let exhaustive = obj
        .get("exhaustive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `weight` is accepted but inert (no weighted fusion in the emulator).
    Ok(VectorQuery {
        fields,
        vector,
        k,
        exhaustive,
    })
}

/// Parses a vector query's `fields`: every entry must be a vector field in
/// the schema. Returns the field names plus the shared dimension the query
/// vector must match (fields with different dimensions cannot share one
/// query vector).
fn parse_vector_query_fields(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<String>, usize), ApiError> {
    let fields_value = obj.get("fields").ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "Each vector query must define \"fields\".")
    })?;
    let fields = string_items(fields_value, "vector query fields")?;
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            "Vector query \"fields\" is empty.",
        ));
    }
    let mut expected: Option<usize> = None;
    for name in &fields {
        let field_def = definition
            .field(name)
            .filter(|f| f.is_vector_field())
            .ok_or_else(|| {
                ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Vector query field {name:?} is not a vector field in index {:?}.",
                        definition.name
                    ),
                )
            })?;
        let dimensions = field_def.vector_dimensions.unwrap_or(0);
        match expected {
            None => expected = Some(dimensions),
            Some(d) if d == dimensions => {}
            Some(d) => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!(
                        "Vector query targets fields with different dimensions ({d} vs {dimensions})."
                    ),
                ));
            }
        }
    }
    Ok((fields, expected.unwrap_or(0)))
}

/// Parses a vector query's `vector`: an array of finite numbers whose length
/// matches every listed field's dimensions.
fn parse_vector_query_vector(
    obj: &Map<String, Value>,
    fields: &[String],
    expected: usize,
) -> Result<Vec<f32>, ApiError> {
    let raw_vector = obj.get("vector").ok_or_else(|| {
        ApiError::bad_request("InvalidQuery", "Each vector query must define \"vector\".")
    })?;
    let items = raw_vector.as_array().ok_or_else(|| {
        ApiError::bad_request(
            "InvalidQuery",
            "Vector query \"vector\" must be an array of numbers.",
        )
    })?;
    let first_field = fields.first().map_or("", String::as_str);
    if items.len() != expected {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            format!(
                "Vector query for {first_field:?} has dimension {}, expected {expected}.",
                items.len()
            ),
        ));
    }
    let mut vector = Vec::with_capacity(items.len());
    for item in items {
        match item.as_f64().and_then(finite_f32) {
            Some(narrowed) => vector.push(narrowed),
            None if item.is_number() => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!("Vector query for {first_field:?} contains non-finite values."),
                ));
            }
            None => {
                return Err(ApiError::bad_request(
                    "InvalidQuery",
                    format!("Vector query for {first_field:?} must contain only numeric values."),
                ));
            }
        }
    }
    Ok(vector)
}

/// Parses a `searchFields` value: comma-separated field names (or a JSON
/// array), optionally weighted (`field^2`). Fields must exist and be marked
/// `searchable`. Weights must be finite positive numbers and scale the
/// field's BM25 contribution to `@search.score`.
fn parse_search_fields(
    value: &Value,
    definition: &IndexDefinition,
) -> Result<Vec<SearchField>, ApiError> {
    let mut fields = Vec::new();
    for part in string_items(value, "searchFields")? {
        let (name_part, weight) = match part.split_once('^') {
            None => (part.as_str(), 1.0),
            Some((name, raw_weight)) => {
                let weight: f32 = raw_weight.trim().parse().map_err(|_| {
                    ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Invalid searchFields weight in {part:?}; \
                             expected a positive number (e.g. 'field^2')."
                        ),
                    )
                })?;
                if !weight.is_finite() || weight <= 0.0 {
                    return Err(ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Invalid searchFields weight in {part:?}; \
                             expected a finite positive number."
                        ),
                    ));
                }
                (name, weight)
            }
        };
        let name = name_part.trim();
        let field_def = definition.field_path(name).ok_or_else(|| {
            ApiError::bad_request(
                "InvalidQuery",
                format!("searchFields references unknown field {name:?}."),
            )
        })?;
        if !field_def.searchable {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!(
                    "Field {name:?} is not searchable; mark it \"searchable\": true in the index schema."
                ),
            ));
        }
        if field_def.is_vector_field() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                format!("Field {name:?} is a vector field and cannot be used in searchFields."),
            ));
        }
        fields.push(SearchField {
            name: name.to_owned(),
            boost: weight,
        });
    }
    if fields.is_empty() {
        return Err(ApiError::bad_request(
            "InvalidQuery",
            "searchFields is empty.",
        ));
    }
    Ok(fields)
}

/// Parses the `highlight` / `highlightPreTag` / `highlightPostTag` options.
/// Highlight fields must exist and be marked `searchable`. Tags default to
/// `<em>` / `</em>` (the Azure defaults).
fn parse_highlight_options(
    obj: &Map<String, Value>,
    definition: &IndexDefinition,
) -> Result<(Vec<String>, String, String), ApiError> {
    let highlight_value = obj.get("highlight");
    let fields = match highlight_value {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let mut fields = Vec::new();
            for part in string_items(value, "highlight")? {
                let field_def = definition.field_path(&part).ok_or_else(|| {
                    ApiError::bad_request(
                        "InvalidQuery",
                        format!("highlight references unknown field {part:?}."),
                    )
                })?;
                if !field_def.searchable {
                    return Err(ApiError::bad_request(
                        "InvalidQuery",
                        format!(
                            "Field {part:?} is not searchable; only searchable fields can be highlighted."
                        ),
                    ));
                }
                fields.push(part);
            }
            if fields.is_empty() {
                return Err(ApiError::bad_request("InvalidQuery", "highlight is empty."));
            }
            fields
        }
    };
    let tag = |key: &str, default: &str| -> Result<String, ApiError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(default.to_owned()),
            Some(Value::String(tag)) => Ok(tag.clone()),
            Some(_) => Err(ApiError::bad_request(
                "InvalidQuery",
                format!("{key} must be a string."),
            )),
        }
    };
    let pre_tag = tag("highlightPreTag", "<em>")?;
    let post_tag = tag("highlightPostTag", "</em>")?;
    Ok((fields, pre_tag, post_tag))
}

fn key_field_name(definition: &IndexDefinition) -> String {
    definition
        .key_field()
        .map(|f| f.name.clone())
        .unwrap_or_default()
}

fn key_display(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Resolves a field path (`Address/City`, or a plain field name) against a
/// document's field map, walking into complex-type objects. When a segment
/// resolves to a JSON array (a collection field or a collection-of-complex
/// field), the remaining path is resolved against every element, so a
/// collection-of-complex path yields one value per element. A plain
/// (non-collection) path yields at most one value.
fn resolve_field_values<'a>(fields: &'a Map<String, Value>, path: &str) -> Vec<&'a Value> {
    let mut segments = path.split('/');
    let Some(first) = segments.next() else {
        return Vec::new();
    };
    let mut current = match fields.get(first) {
        Some(value) => vec![value],
        None => return Vec::new(),
    };
    for segment in segments {
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

/// Builds the engine-level full-text query for a search: parses the search
/// text and applies the request's `searchMode` and `searchFields` (names plus
/// `field^N` boosts).
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the search text is
/// malformed.
fn prepare_full_text(query: &SearchQuery) -> Result<FullTextQuery, ApiError> {
    let mut full_text = match &query.search {
        Some(text) => parse_search_text(text).map_err(|e| {
            ApiError::bad_request("InvalidQuery", format!("Invalid search text: {e}"))
        })?,
        None => FullTextQuery::default(),
    };
    full_text.mode = query.search_mode;
    if !query.search_fields.is_empty() {
        full_text.fields = Some(query.search_fields.iter().map(|f| f.name.clone()).collect());
        full_text.boosts = query
            .search_fields
            .iter()
            .map(|f| (f.name.clone(), f.boost))
            .collect();
    }
    Ok(full_text)
}

/// Computes a page's `@search.highlights` value: one entry per document with
/// a query-term match in a requested highlight field. Empty when no
/// `highlight` fields were requested.
fn page_highlights(
    query: &SearchQuery,
    full_text: &FullTextQuery,
    page: &[(Document, f32)],
) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    if query.highlight_fields.is_empty() {
        return BTreeMap::new();
    }
    let terms = highlight_query_terms(full_text);
    page.iter()
        .filter_map(|(doc, _)| {
            let fields = highlight_document(
                doc,
                &query.highlight_fields,
                &terms,
                &query.highlight_pre_tag,
                &query.highlight_post_tag,
            );
            (!fields.is_empty()).then(|| (doc.key.clone(), fields))
        })
        .collect()
}

/// Extracts the whitespace-separated words of a string (or collection of
/// string) field value, for suggester matching.
fn field_words(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => text.split_whitespace().map(str::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|text| text.split_whitespace().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Collects the analyzed query terms from a full-text query's required
/// clauses (terms, fuzzy terms, and phrase tokens), for highlight matching.
/// Excluded clauses never highlight.
fn highlight_query_terms(query: &FullTextQuery) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for clause in &query.required {
        match clause {
            Clause::Term(term) | Clause::FuzzyTerm { term, .. } => {
                terms.extend(crate::query::analyze(term));
            }
            Clause::Phrase(phrase) => {
                terms.extend(crate::query::analyze(phrase));
            }
        }
    }
    terms
}

/// Wraps the words of `text` that match the analyzed query `terms` with the
/// highlight tags, preserving the original text (including spacing and
/// casing). Matching is analyzer-aware, so inflected forms highlight. Returns
/// `None` when no word matches.
fn highlight_text(
    text: &str,
    terms: &BTreeSet<String>,
    pre_tag: &str,
    post_tag: &str,
) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut matched = false;
    // `split_inclusive` keeps each word glued to its trailing whitespace so
    // the original spacing is preserved.
    for segment in text.split_inclusive(|c: char| c.is_whitespace()) {
        let split = segment
            .find(|c: char| c.is_whitespace())
            .unwrap_or(segment.len());
        let (word, rest) = segment.split_at(split);
        if !word.is_empty()
            && crate::query::analyze(word)
                .iter()
                .any(|token| terms.contains(token))
        {
            out.push_str(pre_tag);
            out.push_str(word);
            out.push_str(post_tag);
            matched = true;
        } else {
            out.push_str(word);
        }
        out.push_str(rest);
    }
    matched.then_some(out)
}

/// Computes a document's `@search.highlights` value: one entry per highlight
/// field that contains a query term, each with the highlighted fragments (one
/// per matching string value).
fn highlight_document(
    doc: &Document,
    fields: &[String],
    terms: &BTreeSet<String>,
    pre_tag: &str,
    post_tag: &str,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for field in fields {
        // A highlight path may resolve to several values (a collection field
        // or a path through a collection-of-complex field); every string
        // value with a query-term match contributes a fragment.
        let mut strings: Vec<&str> = Vec::new();
        for value in resolve_field_values(&doc.fields, field) {
            match value {
                Value::String(text) => strings.push(text.as_str()),
                Value::Array(items) => {
                    strings.extend(items.iter().filter_map(Value::as_str));
                }
                _ => {}
            }
        }
        let mut fragments = Vec::new();
        for text in strings {
            if let Some(fragment) = highlight_text(text, terms, pre_tag, post_tag) {
                fragments.push(fragment);
            }
        }
        if !fragments.is_empty() {
            out.insert(field.clone(), fragments);
        }
    }
    out
}

/// Maps a query-engine failure onto an Azure-compatible [`ApiError`].
fn engine_error(index: &str, error: QueryError) -> ApiError {
    match error {
        QueryError::IndexNotFound(name) => {
            ApiError::not_found(format!("Index {name:?} was not found."))
        }
        QueryError::Engine(message) => {
            ApiError::internal(format!("Search failed for index {index:?}: {message}"))
        }
    }
}

fn parse_index_definition(raw: &Value) -> Result<IndexDefinition, ApiError> {
    IndexDefinition::from_json(raw.clone())
        .map_err(|message| ApiError::bad_request("InvalidIndex", message))
}

/// Whether a field type is a complex type: a single `Edm.ComplexType` object
/// or a collection of them (`Edm.Collection(Edm.ComplexType)`). Both carry
/// subfields and share the same validation and indexing rules.
fn is_complex_type(field_type: &str) -> bool {
    field_type == "Edm.ComplexType" || field_type == "Edm.Collection(Edm.ComplexType)"
}

fn validate_schema(
    definition: &IndexDefinition,
    max_vector_dimension: usize,
) -> Result<(), ApiError> {
    let mut key_count = 0;
    let mut seen = std::collections::BTreeSet::new();
    for field in &definition.fields {
        validate_vector_field(field, max_vector_dimension)?;
        if !seen.insert(field.name.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!("Duplicate field name {:?} in index schema.", field.name),
            ));
        }
        if field.is_key {
            if is_complex_type(&field.field_type) {
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
        if is_complex_type(&field.field_type) {
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

/// Validates a single field's vector markers. A field is *attempting* to be
/// a vector field when it carries a `dimensions` property or a
/// `vectorSearchProfile`; such fields must be `Collection(Edm.Single)` with
/// valid dimensions, a profile, `searchable: true`, and none of
/// key/filterable/sortable/facetable. Plain `Collection(Edm.Single)` fields
/// without vector markers are ordinary collections and pass through.
fn validate_vector_field(field: &FieldDefinition, max_dimension: usize) -> Result<(), ApiError> {
    let attempts_vector = field.has_dimensions_property() || field.vector_search_profile.is_some();
    if !attempts_vector {
        return Ok(());
    }
    if field.field_type != "Edm.Collection(Edm.Single)" {
        return Err(ApiError::bad_request(
            "InvalidIndex",
            format!(
                "Unsupported vector field type {:?} for field {:?}; only 'Collection(Edm.Single)' is supported.",
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

/// Validates the index-level `vectorSearch` configuration against the
/// vector fields: required when vector fields are present (with at least one
/// profile), profiles must reference known algorithms, and every vector
/// field's profile must exist. At most 16 vector fields per index.
fn validate_vector_search_config(definition: &IndexDefinition) -> Result<(), ApiError> {
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

/// Validates the suggesters of an index definition: unique names, at least
/// one search field per suggester, and every search field must exist and be
/// marked `searchable`.
fn validate_suggesters(definition: &IndexDefinition) -> Result<(), ApiError> {
    let mut seen = std::collections::BTreeSet::new();
    for suggester in &definition.suggesters {
        if !seen.insert(suggester.name.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Duplicate suggester name {:?} in index schema.",
                    suggester.name
                ),
            ));
        }
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

/// Validates the subfields of an `Edm.ComplexType` field: non-empty, unique
/// names, supported scalar (or collection-of-scalar) types, no keys, and no
/// vector markers (vector fields cannot be complex-type subfields).
fn validate_subfields(field: &FieldDefinition) -> Result<(), ApiError> {
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
        if !seen.insert(subfield.name.as_str()) {
            return Err(ApiError::bad_request(
                "InvalidIndex",
                format!(
                    "Duplicate subfield name {:?} in complex type field {:?}.",
                    subfield.name, field.name
                ),
            ));
        }
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

/// Validates a synonym-map definition: a non-empty name, the `solr` format
/// (the only format Azure supports), and at least one non-blank synonym rule.
fn validate_synonym_map(name: &str, format: &str, synonyms: &str) -> Result<(), ApiError> {
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
    if synonyms.trim().is_empty() {
        return Err(ApiError::bad_request(
            "InvalidSynonymMap",
            "The synonym map must contain at least one synonym rule.",
        ));
    }
    Ok(())
}

fn validate_document(definition: &IndexDefinition, document: &Value) -> Result<Document, String> {
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
    let key = match key_value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => {
            return Err(format!(
                "Key field {key_name:?} must be a string or number, got {other:?}."
            ))
        }
    };
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

fn check_field_type(field: &FieldDefinition, value: &Value) -> Result<(), String> {
    if field.field_type == "Edm.ComplexType" {
        return check_complex_value(field, value);
    }
    if field.field_type == "Edm.Collection(Edm.ComplexType)" {
        return check_complex_collection_value(field, value);
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
        match field_type.as_str() {
            "Edm.String" | "Edm.DateTimeOffset" | "Edm.Guid" => value.is_string(),
            "Edm.GeographyPoint" => is_geography_point(value),
            "Edm.Int32" | "Edm.Int64" => value.is_i64() || value.is_u64(),
            "Edm.Single" | "Edm.Double" => value.is_number(),
            "Edm.Boolean" => value.is_boolean(),
            _ => true,
        }
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
fn finite_f32(n: f64) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = n as f32;
    (n.is_finite() && narrowed.is_finite()).then_some(narrowed)
}

/// Validates a vector field value: a JSON array of exactly the declared
/// number of finite numbers. Integers are accepted (widened to `f32` at
/// index time).
fn check_vector_value(field: &FieldDefinition, value: &Value) -> Result<(), String> {
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
    for item in items {
        match item.as_f64() {
            // The vector index stores `f32` (`Edm.Single`); wider `f64`
            // values that overflow `f32` would poison distance math, so they
            // are rejected here.
            Some(n) if finite_f32(n).is_some() => {}
            Some(_) => {
                return Err(format!(
                    "Field {:?} must contain only finite numeric values.",
                    field.name
                ));
            }
            None => {
                return Err(format!(
                    "Field {:?} must contain only numeric values.",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

/// Validates a complex-type value: a JSON object whose members are known
/// subfields with type-compatible values. Missing subfields are allowed.
fn check_complex_value(field: &FieldDefinition, value: &Value) -> Result<(), String> {
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
fn check_complex_collection_value(field: &FieldDefinition, value: &Value) -> Result<(), String> {
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
fn is_geography_point(value: &Value) -> bool {
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

fn type_ok(inner: &str, value: &Value) -> bool {
    match inner {
        "Edm.String" | "Edm.DateTimeOffset" | "Edm.Guid" => value.is_string(),
        "Edm.GeographyPoint" => is_geography_point(value),
        "Edm.Int32" | "Edm.Int64" => value.is_i64() || value.is_u64(),
        "Edm.Single" | "Edm.Double" => value.is_number(),
        "Edm.Boolean" => value.is_boolean(),
        _ => true,
    }
}

/// Search request options that are not implemented and must be rejected with
/// an explicit error.
const UNSUPPORTED_SEARCH_OPTIONS: &[&str] = &[
    "scoringProfile",
    "scoringParameters",
    "scoringStatistics",
    "minimumCoverage",
    "answers",
    "captions",
    "semantic",
    "semanticConfiguration",
    "semanticQuery",
    "semanticErrorHandling",
    "semanticMaxWaitInMilliseconds",
    "debug",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
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

    fn service() -> SearchService {
        SearchService::new(
            Arc::new(InMemoryStorage::new()),
            Arc::new(SearchEngine::new()),
            Arc::new(VectorEngine::new()),
            3072,
        )
    }

    fn index_body() -> Value {
        json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
                {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                {"name": "price", "type": "Edm.Double", "filterable": true, "sortable": true},
                {"name": "tags", "type": "Edm.Collection(Edm.String)", "filterable": true, "facetable": true}
            ]
        })
    }

    fn upload(service: &SearchService, docs: Vec<Value>) {
        let actions = docs
            .into_iter()
            .map(|document| DocumentAction {
                kind: ActionKind::Upload,
                document,
            })
            .collect();
        let results = ok(service.index_documents("items", actions));
        assert!(
            results.iter().all(|r| r.succeeded),
            "upload failed: {results:?}"
        );
    }

    #[test]
    fn create_and_delete_index() {
        let service = service();
        let created = ok(service.create_index(&index_body()));
        assert_eq!(created["name"], "items");
        assert!(service.delete_index("items").is_ok());
        assert!(service.delete_index("items").is_err());
    }

    #[test]
    fn duplicate_index_conflicts() {
        let service = service();
        ok(service.create_index(&index_body()));
        let api_error = err(service.create_index(&index_body()));
        assert_eq!(api_error.status, axum::http::StatusCode::CONFLICT);
    }

    #[test]
    fn schema_validation_rejects_bad_types_and_key_counts() {
        let service = service();
        let no_key = json!({
            "name": "x",
            "fields": [{"name": "id", "type": "Edm.String"}]
        });
        assert!(service.create_index(&no_key).is_err());
        let bad_type = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "v", "type": "Edm.Vector(Edm.Single, 3)"}
            ]
        });
        assert!(service.create_index(&bad_type).is_err());
    }

    #[test]
    fn schema_validation_complex_type() {
        let service = service();
        let valid = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {
                    "name": "Address",
                    "type": "Edm.ComplexType",
                    "fields": [
                        {"name": "City", "type": "Edm.String", "searchable": true, "filterable": true},
                        {"name": "Zip", "type": "Edm.Int32", "filterable": true}
                    ]
                }
            ]
        });
        assert!(service.create_index(&valid).is_ok());

        // Failed creations are rejected at validation, before storage, so one
        // service can check every invalid shape.
        let invalid_bodies = [
            // Complex type cannot be the key.
            json!({
                "name": "y",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "Address", "type": "Edm.ComplexType", "key": true,
                     "fields": [{"name": "City", "type": "Edm.String"}]}
                ]
            }),
            // Complex type cannot be searchable/sortable/facetable.
            json!({
                "name": "y",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "Address", "type": "Edm.ComplexType", "searchable": true,
                     "fields": [{"name": "City", "type": "Edm.String"}]}
                ]
            }),
            // Complex type must define subfields.
            json!({
                "name": "y",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "Address", "type": "Edm.ComplexType", "fields": []}
                ]
            }),
            // Subfields must be scalar (or collection-of-scalar) types.
            json!({
                "name": "y",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "Address", "type": "Edm.ComplexType",
                     "fields": [{"name": "Inner", "type": "Edm.ComplexType",
                                 "fields": [{"name": "City", "type": "Edm.String"}]}]}
                ]
            }),
            // Subfield names must be unique.
            json!({
                "name": "y",
                "fields": [
                    {"name": "id", "type": "Edm.String", "key": true},
                    {"name": "Address", "type": "Edm.ComplexType",
                     "fields": [
                         {"name": "City", "type": "Edm.String"},
                         {"name": "City", "type": "Edm.Int32"}
                     ]}
                ]
            }),
        ];
        for body in &invalid_bodies {
            assert!(
                service.create_index(body).is_err(),
                "expected rejection: {body}"
            );
        }
    }

    #[test]
    fn upload_validates_documents_against_schema() {
        let service = service();
        ok(service.create_index(&index_body()));
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "1", "title": "one", "price": 1.5}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"title": "missing key"}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "3", "unknown": true}),
                },
            ],
        ));
        assert_eq!(results.len(), 3);
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 201);
        assert!(!results[1].succeeded);
        assert_eq!(results[1].status_code, 400);
        assert!(!results[2].succeeded);
    }

    #[test]
    fn upload_validates_geography_point_and_complex_values() {
        let service = service();
        ok(service.create_index(&json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "Location", "type": "Edm.GeographyPoint"},
                {
                    "name": "Address",
                    "type": "Edm.ComplexType",
                    "fields": [
                        {"name": "City", "type": "Edm.String"},
                        {"name": "Zip", "type": "Edm.Int32"}
                    ]
                }
            ]
        })));
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({
                        "id": "1",
                        "Location": {"type": "Point", "coordinates": [-122.13, 47.67]},
                        "Address": {"City": "Seattle", "Zip": 98101}
                    }),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "2", "Location": {"type": "LineString", "coordinates": []}}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "3", "Location": {"type": "Point", "coordinates": [-122.13]}}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "4", "Address": {"City": "Seattle", "Unknown": 1}}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "5", "Address": {"Zip": "not a number"}}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "6", "Address": "not an object"}),
                },
            ],
        ));
        assert_eq!(results.len(), 6);
        assert!(
            results[0].succeeded,
            "{}",
            results[0].error_message.clone().unwrap_or_default()
        );
        for (index, result) in results.iter().skip(1).enumerate() {
            assert!(!result.succeeded, "document {} should fail", index + 2);
            assert_eq!(result.status_code, 400);
        }
    }

    #[test]
    fn merge_updates_existing_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.0, "tags": ["a"]})],
        );
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "1", "price": 2.0}),
            }],
        ));
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 200);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs[0].fields["title"], "one");
        assert_eq!(docs[0].fields["price"], 2.0);
        // Collections are replaced, not appended.
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "1", "tags": ["b"]}),
            }],
        ));
        assert!(results[0].succeeded);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs[0].fields["tags"], json!(["b"]));
    }

    #[test]
    fn merge_missing_document_fails_per_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        let results = ok(service.index_documents(
            "items",
            vec![DocumentAction {
                kind: ActionKind::Merge,
                document: json!({"id": "missing", "price": 2.0}),
            }],
        ));
        assert!(!results[0].succeeded);
        assert_eq!(results[0].status_code, 404);
    }

    #[test]
    fn merge_or_upload_merges_or_uploads() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.0})],
        );
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::MergeOrUpload,
                    document: json!({"id": "1", "price": 9.0}),
                },
                DocumentAction {
                    kind: ActionKind::MergeOrUpload,
                    document: json!({"id": "2", "title": "two", "price": 3.0}),
                },
            ],
        ));
        assert_eq!(results[0].status_code, 200);
        assert_eq!(results[1].status_code, 201);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].fields["title"], "one");
        assert_eq!(docs[0].fields["price"], 9.0);
    }

    #[test]
    fn delete_removes_document() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "one"}),
                json!({"id": "2", "title": "two"}),
            ],
        );
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "1"}),
                },
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "missing"}),
                },
            ],
        ));
        assert!(results[0].succeeded);
        assert_eq!(results[0].status_code, 200);
        assert!(!results[1].succeeded);
        assert_eq!(results[1].status_code, 404);
        let docs = ok(service.storage().get_documents("items"));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].key, "2");
        // Deleted documents are no longer searchable.
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 1);
    }

    #[test]
    fn upsert_index_rebuilds_search_state() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "hello", "price": 1.0})],
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("hello".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 1);

        // Replacing the index discards its documents from both storage and
        // the search engine.
        let replacement = json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "other", "type": "Edm.String", "searchable": true}
            ]
        });
        ok(service.create_or_update_index(&replacement));
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("hello".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 0);
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 0);
    }

    #[test]
    fn search_matches_and_orders_by_key() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "b", "title": "azure search"}),
                json!({"id": "a", "title": "azure emulators"}),
                json!({"id": "c", "title": "other"}),
            ],
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("azure".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 2);
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn search_count_and_paging() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            (0..5)
                .map(|i| json!({"id": i.to_string(), "title": "same"}))
                .collect(),
        );
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                count: true,
                top: Some(2),
                skip: 1,
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 5);
        assert_eq!(outcome.documents.len(), 2);
        assert_eq!(outcome.documents[0].key, "1");
        assert!(outcome.has_more);
        assert_eq!(outcome.next_skip, 3);
    }

    #[test]
    fn search_filter_narrows_results() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "cheap", "price": 5.0}),
                json!({"id": "2", "title": "mid", "price": 50.0}),
                json!({"id": "3", "title": "expensive", "price": 500.0}),
            ],
        );
        let query =
            ok(service.parse_search("items", &json!({"search": "*", "filter": "price ge 50"})));
        let outcome = ok(service.search("items", &query));
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["2", "3"]);
    }

    #[test]
    fn search_orderby_sorts_results() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "price": 30.0}),
                json!({"id": "2", "title": "b", "price": 10.0}),
                json!({"id": "3", "title": "c", "price": 20.0}),
            ],
        );
        let query =
            ok(service.parse_search("items", &json!({"search": "*", "orderby": "price desc"})));
        let outcome = ok(service.search("items", &query));
        let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
        assert_eq!(keys, vec!["1", "3", "2"]);
    }

    #[test]
    fn search_facets_count_values() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "tags": ["red", "blue"]}),
                json!({"id": "2", "title": "b", "tags": ["red"]}),
                json!({"id": "3", "title": "c", "tags": ["green"]}),
            ],
        );
        let query = ok(service.parse_search("items", &json!({"search": "*", "facets": "tags"})));
        let outcome = ok(service.search("items", &query));
        let Some(facets) = outcome.facets else {
            panic!("expected facets in search outcome");
        };
        let Some(entries) = facets["tags"].as_array() else {
            panic!("expected tags facet array");
        };
        assert_eq!(entries[0]["value"], "red");
        assert_eq!(entries[0]["count"], 2);
        assert_eq!(entries[1]["value"], "blue");
        assert_eq!(entries[1]["count"], 1);
        assert_eq!(entries[2]["value"], "green");
        assert_eq!(entries[2]["count"], 1);
    }

    #[test]
    fn search_facets_options_limit_and_total_count() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "tags": ["red", "blue"]}),
                json!({"id": "2", "title": "b", "tags": ["red"]}),
                json!({"id": "3", "title": "c", "tags": ["green"]}),
            ],
        );
        let query = ok(service.parse_search(
            "items",
            &json!({"search": "*", "facets": ["tags,count:1", "$count"]}),
        ));
        let outcome = ok(service.search("items", &query));
        let Some(facets) = outcome.facets else {
            panic!("expected facets in search outcome");
        };
        let Some(entries) = facets["tags"].as_array() else {
            panic!("expected tags facet array");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["value"], "red");
        assert_eq!(entries[0]["count"], 2);
        assert_eq!(facets["$count"], 3);

        // Unknown facet options are rejected explicitly.
        let api_error = err(service.parse_search("items", &json!({"facets": ["tags,minimum:1"]})));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.parse_search("items", &json!({"facets": ["tags,count:many"]})));
        assert_eq!(api_error.code, "InvalidQuery");
    }

    #[test]
    fn parse_search_rejects_bad_options() {
        let service = service();
        ok(service.create_index(&index_body()));
        // Unknown filter field.
        let api_error = err(service.parse_search("items", &json!({"filter": "missing eq 1"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-sortable field in orderby.
        let api_error = err(service.parse_search("items", &json!({"orderby": "title"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Unknown field in select.
        let api_error = err(service.parse_search("items", &json!({"select": "nope"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-facetable field in facets.
        let api_error = err(service.parse_search("items", &json!({"facets": "price"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Non-searchable field in searchFields.
        let api_error = err(service.parse_search("items", &json!({"searchFields": "price"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Bad searchFields weight.
        let api_error = err(service.parse_search("items", &json!({"searchFields": "title^many"})));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.parse_search("items", &json!({"searchFields": "title^0"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Highlight on an unknown or non-searchable field is rejected.
        let api_error = err(service.parse_search("items", &json!({"highlight": "missing"})));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.parse_search("items", &json!({"highlight": "price"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Invalid searchMode.
        let api_error = err(service.parse_search("items", &json!({"searchMode": "both"})));
        assert_eq!(api_error.code, "InvalidQuery");
        // Bad filter syntax.
        let api_error = err(service.parse_search("items", &json!({"filter": "price eq"})));
        assert_eq!(api_error.code, "InvalidQuery");
    }

    #[test]
    fn parse_search_accepts_search_mode_highlight_and_weights() {
        let service = service();
        ok(service.create_index(&index_body()));
        let query = ok(service.parse_search(
            "items",
            &json!({
                "search": "hello",
                "searchMode": "any",
                "searchFields": "title^2",
                "highlight": "title",
                "highlightPreTag": "<b>",
                "highlightPostTag": "</b>",
            }),
        ));
        assert_eq!(query.search_mode, crate::query::SearchMode::Any);
        assert_eq!(query.search_fields.len(), 1);
        assert_eq!(query.search_fields[0].name, "title");
        assert!(
            (query.search_fields[0].boost - 2.0).abs() < f32::EPSILON,
            "expected boost 2.0, got {}",
            query.search_fields[0].boost
        );
        assert_eq!(query.highlight_fields, vec!["title".to_owned()]);
        assert_eq!(query.highlight_pre_tag, "<b>");
        assert_eq!(query.highlight_post_tag, "</b>");
        // Defaults: OR mode (matching Azure), unit boosts, <em> tags.
        let query = ok(service.parse_search("items", &json!({"highlight": "title"})));
        assert_eq!(query.search_mode, crate::query::SearchMode::Any);
        assert_eq!(query.highlight_pre_tag, "<em>");
        assert_eq!(query.highlight_post_tag, "</em>");
    }

    #[test]
    fn continuation_token_round_trips_and_survives_mutation() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            (0..5)
                .map(|i| json!({"id": i.to_string(), "title": "same"}))
                .collect(),
        );
        let query = ok(service.parse_search(
            "items",
            &json!({"search": "*", "top": 2, "filter": "title eq 'same'"}),
        ));
        let outcome = ok(service.search("items", &query));
        let token = service
            .next_continuation(&query, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));
        // The token decodes and carries the next skip.
        let decoded =
            ContinuationToken::decode(&token).unwrap_or_else(|e| panic!("token decodes: {e}"));
        assert_eq!(decoded.skip, 2);
        assert_eq!(decoded.filter.as_deref(), Some("title eq 'same'"));

        // A mutation does NOT invalidate the token (like Azure; results may
        // shift). The filter still matches ids 0-4, so skip=2 resumes at "2".
        upload(&service, vec![json!({"id": "5", "title": "new"})]);
        let resumed = SearchQuery {
            continuation: Some(token),
            ..query.clone()
        };
        let outcome = ok(service.search("items", &resumed));
        assert_eq!(outcome.documents.first().map(|d| d.key.as_str()), Some("2"));

        // A fresh token also works and resumes after the first page.
        let outcome = ok(service.search("items", &query));
        let token = service
            .next_continuation(&query, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));
        let next = SearchQuery {
            continuation: Some(token),
            ..query.clone()
        };
        let outcome = ok(service.search("items", &next));
        assert_eq!(outcome.documents.first().map(|d| d.key.as_str()), Some("2"));
    }

    #[test]
    fn search_unknown_index_not_found() {
        let service = service();
        let api_error = err(service.parse_search("missing", &Value::Null));
        assert_eq!(api_error.status, axum::http::StatusCode::NOT_FOUND);
    }

    fn suggester_index_body() -> Value {
        json!({
            "name": "items",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true, "filterable": true, "sortable": true},
                {"name": "title", "type": "Edm.String", "searchable": true, "filterable": true},
                {"name": "tags", "type": "Edm.Collection(Edm.String)", "searchable": true, "filterable": true, "facetable": true}
            ],
            "suggesters": [
                {"name": "sg", "searchFields": ["title", "tags"]}
            ]
        })
    }

    #[test]
    fn schema_validation_rejects_bad_suggesters() {
        let service = service();
        // Unknown search field.
        let unknown_field = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true}
            ],
            "suggesters": [{"name": "sg", "searchFields": ["missing"]}]
        });
        assert!(service.create_index(&unknown_field).is_err());
        // Non-searchable search field.
        let not_searchable = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "filterable": true}
            ],
            "suggesters": [{"name": "sg", "searchFields": ["title"]}]
        });
        assert!(service.create_index(&not_searchable).is_err());
        // Duplicate suggester names.
        let duplicate = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true}
            ],
            "suggesters": [
                {"name": "sg", "searchFields": ["title"]},
                {"name": "sg", "searchFields": ["title"]}
            ]
        });
        assert!(service.create_index(&duplicate).is_err());
        // Malformed suggester (missing searchFields).
        let malformed = json!({
            "name": "x",
            "fields": [
                {"name": "id", "type": "Edm.String", "key": true},
                {"name": "title", "type": "Edm.String", "searchable": true}
            ],
            "suggesters": [{"name": "sg"}]
        });
        assert!(service.create_index(&malformed).is_err());
    }

    #[test]
    fn autocomplete_prefix_matches_and_dedupes() {
        let service = service();
        ok(service.create_index(&suggester_index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "Boston Harbor Hotel", "tags": ["spa"]}),
                json!({"id": "2", "title": "Boston Airport Inn", "tags": ["boston", "wifi"]}),
                json!({"id": "3", "title": "Seattle Downtown", "tags": ["wifi"]}),
            ],
        );
        let completions = ok(service.autocomplete("items", "sg", "bos", 5));
        let texts: Vec<_> = completions.iter().map(|c| c.text.clone()).collect();
        assert_eq!(texts, vec!["Boston", "boston"]);
        assert_eq!(completions[0].query_plus_text, "bos Boston");
        // Case-insensitive: "BOS" matches the same words.
        let completions = ok(service.autocomplete("items", "sg", "BOS", 5));
        assert_eq!(completions.len(), 2);
        // No match.
        assert!(ok(service.autocomplete("items", "sg", "zzz", 5)).is_empty());
        // top limits the results.
        let completions = ok(service.autocomplete("items", "sg", "bos", 1));
        assert_eq!(completions.len(), 1);
    }

    #[test]
    fn suggest_returns_matching_documents_with_text() {
        let service = service();
        ok(service.create_index(&suggester_index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "Boston Harbor Hotel", "tags": ["spa"]}),
                json!({"id": "2", "title": "Seattle Downtown", "tags": ["boston", "wifi"]}),
                json!({"id": "3", "title": "Portland Lodge", "tags": ["wifi"]}),
            ],
        );
        let suggestions = ok(service.suggest("items", "sg", "bos", 5));
        assert_eq!(suggestions.len(), 2);
        // Documents come back in key order, with the matched word.
        assert_eq!(suggestions[0].document.key, "1");
        assert_eq!(suggestions[0].text, "Boston");
        assert_eq!(suggestions[1].document.key, "2");
        assert_eq!(suggestions[1].text, "boston");
        // The full document fields are preserved.
        assert_eq!(
            suggestions[0].document.fields["title"],
            "Boston Harbor Hotel"
        );
        // top limits the results.
        let suggestions = ok(service.suggest("items", "sg", "bos", 1));
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].document.key, "1");
    }

    #[test]
    fn autocomplete_and_suggest_reject_bad_requests() {
        let service = service();
        ok(service.create_index(&suggester_index_body()));
        // Missing index.
        let api_error = err(service.autocomplete("missing", "sg", "bos", 5));
        assert_eq!(api_error.status, axum::http::StatusCode::NOT_FOUND);
        // Unknown suggester.
        let api_error = err(service.autocomplete("items", "nope", "bos", 5));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.suggest("items", "nope", "bos", 5));
        assert_eq!(api_error.code, "InvalidQuery");
        // Empty search text.
        let api_error = err(service.autocomplete("items", "sg", "   ", 5));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.suggest("items", "sg", "", 5));
        assert_eq!(api_error.code, "InvalidQuery");
    }

    #[test]
    fn concurrent_writes_and_searches_do_not_corrupt_state() {
        use std::sync::Barrier;

        let service = Arc::new(service());
        ok(service.create_index(&index_body()));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for worker in 0..4 {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..25 {
                    let key = format!("w{worker}-{i}");
                    let actions = vec![DocumentAction {
                        kind: ActionKind::Upload,
                        document: json!({"id": key, "title": "concurrent", "price": 1.0}),
                    }];
                    ok(service.index_documents("items", actions));
                }
            }));
        }
        for _ in 0..4 {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..25 {
                    let outcome = ok(service.search(
                        "items",
                        &SearchQuery {
                            search: Some("concurrent".to_owned()),
                            ..Default::default()
                        },
                    ));
                    // Every returned document matches; ordering is by key.
                    let keys: Vec<_> = outcome.documents.iter().map(|d| d.key.clone()).collect();
                    let mut sorted = keys.clone();
                    sorted.sort();
                    assert_eq!(keys, sorted);
                }
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap_or_else(|e| panic!("worker panicked: {e:?}"));
        }
        // All 100 documents landed exactly once.
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 100);
    }

    #[test]
    fn batch_last_action_wins_for_repeated_keys() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.0})],
        );

        // Upload-then-delete of a new key in one batch: the document is gone
        // from storage and from the search engine alike.
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "2", "title": "two", "price": 2.0}),
                },
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "2"}),
                },
            ],
        ));
        assert_eq!(
            results.iter().map(|r| r.status_code).collect::<Vec<_>>(),
            vec![201, 200]
        );
        assert_eq!(ok(service.count_documents("items")), 1);
        let outcome = ok(service.search(
            "items",
            &SearchQuery {
                search: Some("*".to_owned()),
                ..Default::default()
            },
        ));
        assert_eq!(outcome.total, 1);

        // Delete-then-upload of an existing key: the upload wins.
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Delete,
                    document: json!({"id": "1"}),
                },
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "1", "title": "replaced", "price": 9.0}),
                },
            ],
        ));
        assert_eq!(
            results.iter().map(|r| r.status_code).collect::<Vec<_>>(),
            vec![200, 201]
        );
        assert_eq!(ok(service.get_document("items", "1"))["title"], "replaced");

        // Upload-then-merge in one batch merges against the pending upload.
        let results = ok(service.index_documents(
            "items",
            vec![
                DocumentAction {
                    kind: ActionKind::Upload,
                    document: json!({"id": "3", "title": "three", "price": 3.0}),
                },
                DocumentAction {
                    kind: ActionKind::Merge,
                    document: json!({"id": "3", "price": 30.0}),
                },
            ],
        ));
        assert!(results.iter().all(|r| r.succeeded));
        let merged = ok(service.get_document("items", "3"));
        assert_eq!(merged["title"], "three");
        assert_eq!(merged["price"], 30.0);
    }

    #[test]
    fn continuation_only_request_preserves_filter_and_orderby() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            (1..=4)
                .map(|i| json!({"id": i.to_string(), "title": "same", "price": f64::from(i)}))
                .collect(),
        );
        let query = ok(service.parse_search(
            "items",
            &json!({"search": "*", "top": 1, "filter": "price ge 2", "orderby": "price desc"}),
        ));
        let outcome = ok(service.search("items", &query));
        assert_eq!(outcome.documents[0].key, "4");
        let token = service
            .next_continuation(&query, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));

        // Later pages may send only the continuation (plus a page size): the
        // result set stays filtered and ordered.
        let next = SearchQuery {
            search: Some("*".to_owned()),
            top: Some(1),
            continuation: Some(token),
            ..Default::default()
        };
        let outcome = ok(service.search("items", &next));
        assert_eq!(outcome.documents.len(), 1);
        assert_eq!(outcome.documents[0].key, "3");
        // The forwarded token still carries the filter/orderby raws, so a
        // third continuation-only page stays on the same result set.
        let token = service
            .next_continuation(&next, &outcome)
            .unwrap_or_else(|| panic!("expected a continuation token"));
        let decoded =
            ContinuationToken::decode(&token).unwrap_or_else(|e| panic!("token decodes: {e}"));
        assert_eq!(decoded.filter.as_deref(), Some("price ge 2"));
        assert_eq!(decoded.orderby.as_deref(), Some("price desc"));
        let next = SearchQuery {
            search: Some("*".to_owned()),
            top: Some(1),
            continuation: Some(token),
            ..Default::default()
        };
        let outcome = ok(service.search("items", &next));
        assert_eq!(outcome.documents.len(), 1);
        assert_eq!(outcome.documents[0].key, "2");
        assert!(!outcome.has_more);
        assert!(service.next_continuation(&next, &outcome).is_none());
    }

    #[test]
    fn continuation_tokens_are_url_safe_and_legacy_tokens_decode() {
        let token = ContinuationToken {
            filter: Some("price ge 2 and title ne 'x/y+z'".to_owned()),
            orderby: Some("price desc".to_owned()),
            skip: 7,
            vector_query_hash: Some(u64::MAX),
        };
        // `+` and `/` would be mangled inside query strings; the URL-safe
        // alphabet avoids them (padding `=` is query-safe).
        let encoded = token.encode();
        assert!(
            !encoded.contains('+') && !encoded.contains('/'),
            "token must be URL-safe: {encoded}"
        );
        assert_eq!(
            ContinuationToken::decode(&encoded).unwrap_or_else(|e| panic!("token decodes: {e}")),
            token
        );
        // Tokens minted before the URL-safe switch still decode.
        let json = serde_json::to_string(&token).unwrap_or_else(|e| panic!("serializes: {e}"));
        let legacy = BASE64.encode(json);
        assert_eq!(
            ContinuationToken::decode(&legacy).unwrap_or_else(|e| panic!("legacy decodes: {e}")),
            token
        );
    }

    #[test]
    fn top_zero_returns_empty_page_without_continuation() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "one"}),
                json!({"id": "2", "title": "two"}),
            ],
        );
        let query = ok(service.parse_search("items", &json!({"search": "*", "top": 0})));
        let outcome = ok(service.search("items", &query));
        assert_eq!(outcome.total, 2);
        assert!(outcome.documents.is_empty());
        assert!(!outcome.has_more);
        assert!(service.next_continuation(&query, &outcome).is_none());
    }

    #[test]
    fn select_star_returns_all_fields() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![json!({"id": "1", "title": "one", "price": 1.5, "tags": ["a"]})],
        );
        let query = ok(service.parse_search("items", &json!({"search": "*", "select": "*"})));
        // `*` behaves like omitting `select`: no projection is recorded.
        assert!(query.select.is_empty());
        let outcome = ok(service.search("items", &query));
        assert_eq!(outcome.documents.len(), 1);
        for field in ["id", "title", "price", "tags"] {
            assert!(
                outcome.documents[0].fields.contains_key(field),
                "missing field {field}"
            );
        }
    }

    #[test]
    fn orderby_search_score_orders_by_relevance() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "azure azure azure"}),
                json!({"id": "2", "title": "azure"}),
            ],
        );
        let keys = |orderby: &str| {
            let query =
                ok(service.parse_search("items", &json!({"search": "azure", "orderby": orderby})));
            ok(service.search("items", &query))
                .documents
                .iter()
                .map(|d| d.key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("@search.score desc"), vec!["1", "2"]);
        assert_eq!(keys("@search.score asc"), vec!["2", "1"]);
    }

    #[test]
    fn facet_string_with_options_limits_values() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "a", "tags": ["red", "blue"]}),
                json!({"id": "2", "title": "b", "tags": ["red"]}),
                json!({"id": "3", "title": "c", "tags": ["green"]}),
            ],
        );
        // Options in the single-string form attach to the preceding facet.
        let query =
            ok(service.parse_search("items", &json!({"search": "*", "facets": "tags,count:1"})));
        let outcome = ok(service.search("items", &query));
        let Some(facets) = outcome.facets else {
            panic!("expected facets in search outcome");
        };
        let Some(entries) = facets["tags"].as_array() else {
            panic!("expected tags facet array");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["value"], "red");
        assert_eq!(entries[0]["count"], 2);
    }
}
