//! Domain / service layer: index lifecycle, document indexing (upload, merge,
//! merge-or-upload, delete), and search (full-text, filter, ordering,
//! projection, facets, paging with continuation tokens).

pub mod continuation;
pub mod facets;
pub mod highlight;
pub mod ordering;
pub mod parsing;
pub mod resources;
pub mod synonyms;
pub mod types;
pub mod validation;

pub use continuation::ContinuationToken;
pub use resources::{NamedResource, ResourceKind};
pub use synonyms::SynonymMap;
pub use types::{
    ActionKind, AutocompleteCompletion, DocumentAction, Facet, IndexingResultItem, OrderBy,
    PagingState, SearchField, SearchOutcome, SearchQuery, Suggestion, VectorFilterMode,
    VectorQuery,
};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::ApiError;
use crate::filter::{self, FilterExpr};
use crate::query::{parse_search_text, Clause, FullTextQuery, QueryError, QueryType, SearchEngine};
use crate::storage::{
    Document, FieldDefinition, IndexDefinition, Storage, StorageError, Suggester,
};
use crate::vector::{vector_query_hash, VectorEngine};

use self::facets::compute_facets;
use self::highlight::{field_words, page_highlights};
use self::ordering::{order_scored, rrf_fuse_weighted};
use self::parsing::{
    parse_facets, parse_filter_option, parse_highlight_options, parse_orderby,
    parse_paging_options, parse_search_fields, parse_search_mode, parse_select,
    parse_vector_options, UNSUPPORTED_SEARCH_OPTIONS,
};
use self::resources::{named_resource, ResourceStore};
use self::synonyms::{parse_synonym_rules, validate_synonym_map};
use self::types::KeyPredicate;
use self::validation::{
    finite_f32, key_display, key_field_name, validate_document, validate_schema,
};

/// The resolved state of a search: the effective paging parameters plus the
/// per-side score lists, ready to merge, order, and project.
struct SearchPlan {
    skip: u64,
    orderby: Vec<OrderBy>,
    full_text: FullTextQuery,
    documents: Vec<Document>,
    full_text_scores: BTreeMap<String, f32>,
    vector_lists: Vec<(f32, BTreeMap<String, f32>)>,
}

/// A projected search page: the response documents, their per-document
/// scores, and their highlight fragments.
struct SearchPage {
    documents: Vec<Document>,
    scores: BTreeMap<String, f32>,
    highlights: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

pub struct SearchService {
    storage: Arc<dyn Storage>,
    engine: Arc<SearchEngine>,
    vectors: Arc<VectorEngine>,
    /// Cap on accepted vector dimensions (`EMULATOR_VECTOR__MAX_DIMENSION`,
    /// default 3072).
    max_vector_dimension: usize,
    /// Service-level synonym maps, keyed by name (sorted).
    synonym_maps: ResourceStore<SynonymMap>,
    /// Service-level named resources (aliases, knowledge sources, knowledge
    /// bases), one store per [`ResourceKind`] (see
    /// [`ResourceKind::named_index`]), keyed by name (sorted).
    named_resources: [ResourceStore<NamedResource>; 3],
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
            synonym_maps: ResourceStore::default(),
            named_resources: [
                ResourceStore::default(),
                ResourceStore::default(),
                ResourceStore::default(),
            ],
        }
    }

    /// The named-resource store for a [`NamedResource`] kind.
    ///
    /// # Panics
    ///
    /// Panics when called for [`ResourceKind::SynonymMap`].
    fn named_store(&self, kind: ResourceKind) -> &ResourceStore<NamedResource> {
        &self.named_resources[kind.named_index()]
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
        self.check_alias_collision(&definition.name)?;
        match self.storage.create_index(&definition) {
            Ok(()) => {
                if let Err(e) = self
                    .engine
                    .create_index(&definition.name, &definition.fields)
                {
                    // Roll back: this index did not exist before, so removing
                    // it restores the prior state and avoids divergence.
                    self.rollback_index(&definition.name);
                    return Err(engine_error(&definition.name, e));
                }
                if let Err(message) = self.create_vector_indexes(&definition) {
                    self.rollback_index(&definition.name);
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

    /// Creates or updates an index from a raw Azure `SearchIndex` definition,
    /// returning the echoed definition. When the index already exists and the
    /// schema change is compatible with its stored documents (every existing
    /// field is kept with the same name, type, and — for vector fields —
    /// dimensions), the documents are preserved and re-indexed, matching
    /// Azure's in-place update. Otherwise the index is replaced and its
    /// documents are discarded.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the definition is malformed or the schema
    /// uses unsupported field types.
    pub fn create_or_update_index(&self, raw: &Value) -> Result<Value, ApiError> {
        let definition = parse_index_definition(raw)?;
        validate_schema(&definition, self.max_vector_dimension)?;
        self.check_alias_collision(&definition.name)?;
        // In-place update: capture the stored documents when the index exists
        // and the schema change is compatible with them; otherwise the index
        // is replaced and its documents are discarded.
        let preserved = match self.storage.get_index(&definition.name) {
            Some(existing) if schema_compatible(&existing, &definition) => self
                .storage
                .get_documents(&definition.name)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.storage.upsert_index(&definition);
        // Rebuild the search index; preserved documents are re-indexed below.
        self.engine.delete_index(&definition.name);
        if let Err(e) = self
            .engine
            .create_index(&definition.name, &definition.fields)
        {
            // Engine rebuild failed; roll the index back to absent so storage,
            // engine, and vectors stay consistent. The caller can retry.
            self.rollback_index(&definition.name);
            return Err(engine_error(&definition.name, e));
        }
        // Rebuild the vector indexes; preserved vectors are re-inserted below.
        self.vectors.delete_index(&definition.name);
        if let Err(message) = self.create_vector_indexes(&definition) {
            self.rollback_index(&definition.name);
            return Err(ApiError::bad_request("InvalidIndex", message));
        }
        // Re-materialize preserved documents: engine, vectors, then storage
        // (the same order as a fresh upload). A failure rolls the index back
        // to absent so storage/engine/vectors never diverge.
        if !preserved.is_empty() {
            if let Err(e) = self.engine.index_documents(&definition.name, &preserved) {
                self.rollback_index(&definition.name);
                return Err(engine_error(&definition.name, e));
            }
            if let Err(message) = self.apply_vector_changes(&definition, &preserved, &[]) {
                self.rollback_index(&definition.name);
                return Err(ApiError::internal(format!(
                    "Vector re-indexing failed for index {:?}: {message}",
                    definition.name
                )));
            }
            if let Err(e) = self.storage.put_documents(&definition.name, preserved) {
                self.rollback_index(&definition.name);
                return Err(ApiError::not_found(e.to_string()));
            }
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

    /// Index and alias names share the data-plane namespace (aliases resolve
    /// where index names are accepted), so an index may not shadow an alias.
    fn check_alias_collision(&self, name: &str) -> Result<(), ApiError> {
        if self.named_store(ResourceKind::Alias).contains(name) {
            return Err(ApiError::conflict(
                "IndexAlreadyExists",
                format!("An alias with name {name:?} already exists."),
            ));
        }
        Ok(())
    }

    /// Removes an index from storage, the search engine, and the vector
    /// engine. Deleting a non-existent index is a no-op in all three, so this
    /// is safe to call after any partial creation to restore the prior state
    /// and avoid storage/engine/vectors divergence.
    fn rollback_index(&self, name: &str) {
        self.storage.delete_index(name);
        self.engine.delete_index(name);
        self.vectors.delete_index(name);
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
        let rules = parse_synonym_rules(synonyms).map_err(|e| {
            ApiError::bad_request("InvalidSynonymMap", format!("Invalid synonym rules: {e}"))
        })?;
        self.synonym_maps
            .create(ResourceKind::SynonymMap, name, |name, etag| SynonymMap {
                name: name.to_owned(),
                format: format.to_owned(),
                synonyms: synonyms.to_owned(),
                rules: rules.clone(),
                etag,
            })
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
        let rules = parse_synonym_rules(synonyms).map_err(|e| {
            ApiError::bad_request("InvalidSynonymMap", format!("Invalid synonym rules: {e}"))
        })?;
        Ok(self
            .synonym_maps
            .create_or_update(name, |name, etag| SynonymMap {
                name: name.to_owned(),
                format: format.to_owned(),
                synonyms: synonyms.to_owned(),
                rules,
                etag,
            }))
    }

    /// Returns a clone of the synonym map with the given name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the map does not exist.
    pub fn get_synonym_map(&self, name: &str) -> Result<SynonymMap, ApiError> {
        self.synonym_maps.get(ResourceKind::SynonymMap, name)
    }

    /// Returns the raw synonym outputs for a query word: the union of outputs
    /// from all rules (across all service-level maps) with a matching input
    /// (case-insensitive), excluding the word itself. All maps apply to every
    /// search (the emulator does not track per-field map associations).
    fn synonym_expansions(&self, word: &str) -> Vec<String> {
        let lowered = word.to_lowercase();
        let mut outputs = BTreeSet::new();
        for map in self.synonym_maps.list() {
            for rule in &map.rules {
                if rule.inputs.iter().any(|input| input == &lowered) {
                    outputs.extend(rule.outputs.iter().cloned());
                }
            }
        }
        outputs.remove(&lowered);
        outputs.into_iter().collect()
    }

    /// Returns clones of all synonym maps, sorted by name.
    #[must_use]
    pub fn list_synonym_maps(&self) -> Vec<SynonymMap> {
        self.synonym_maps.list()
    }

    /// Deletes a synonym map by name.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] if the map does not exist.
    pub fn delete_synonym_map(&self, name: &str) -> Result<(), ApiError> {
        self.synonym_maps.delete(ResourceKind::SynonymMap, name)
    }

    // ------------------------------------------------------------------
    // Named resources (aliases, knowledge sources, knowledge bases)
    // ------------------------------------------------------------------

    /// Creates a new named resource (alias, knowledge source, or knowledge
    /// base).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if a resource of the same kind and name
    /// exists, or — for aliases — if an index with the same name exists (the
    /// two share the data-plane namespace, so neither may shadow the other).
    pub fn create_named_resource(
        &self,
        kind: ResourceKind,
        name: &str,
        raw: &Value,
    ) -> Result<NamedResource, ApiError> {
        if kind == ResourceKind::Alias {
            self.reject_alias_index_collision(name)?;
        }
        self.named_store(kind)
            .create(kind, name, |name, etag| named_resource(name, etag, raw))
    }

    /// Creates or replaces a named resource (alias, knowledge source, or
    /// knowledge base). Replacing a resource issues a new etag.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] when `kind` is an alias and an index
    /// with the same name exists.
    pub fn create_or_update_named_resource(
        &self,
        kind: ResourceKind,
        name: &str,
        raw: &Value,
    ) -> Result<NamedResource, ApiError> {
        if kind == ResourceKind::Alias {
            self.reject_alias_index_collision(name)?;
        }
        Ok(self
            .named_store(kind)
            .create_or_update(name, |name, etag| named_resource(name, etag, raw)))
    }

    fn reject_alias_index_collision(&self, name: &str) -> Result<(), ApiError> {
        if self.storage.get_index(name).is_some() {
            return Err(ApiError::conflict(
                ResourceKind::Alias.conflict_code(),
                format!("An index with name {name:?} already exists."),
            ));
        }
        Ok(())
    }

    /// Returns a clone of the named resource with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    pub fn get_named_resource(
        &self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<NamedResource, ApiError> {
        self.named_store(kind).get(kind, name)
    }

    /// Returns clones of all named resources of the given kind, sorted by
    /// name.
    #[must_use]
    pub fn list_named_resources(&self, kind: ResourceKind) -> Vec<NamedResource> {
        self.named_store(kind).list()
    }

    /// Deletes a named resource by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    pub fn delete_named_resource(&self, kind: ResourceKind, name: &str) -> Result<(), ApiError> {
        self.named_store(kind).delete(kind, name)
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
                self.merge_or_fallback(definition, key_name, key, action, batch, |key, batch| {
                    batch.results.push(fail_result(
                        key.clone(),
                        404,
                        format!("Document with key {key:?} was not found in index {index:?}."),
                    ));
                });
            }
            ActionKind::MergeOrUpload => {
                self.merge_or_fallback(definition, key_name, key, action, batch, |key, batch| {
                    match validate_document(definition, &action.document) {
                        Ok(doc) => {
                            batch.record_upsert(doc);
                            batch.results.push(ok_result(key, 201));
                        }
                        Err(message) => batch.results.push(fail_result(key, 400, message)),
                    }
                });
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

    /// Applies a merge-family action (`merge` / `mergeOrUpload`): a successful
    /// merge records the merged document (200) and an invalid document fails
    /// (400); a missing document is delegated to `on_missing` (a 404 for
    /// `merge`, an upload attempt for `mergeOrUpload`).
    fn merge_or_fallback<F>(
        &self,
        definition: &IndexDefinition,
        key_name: &str,
        key: String,
        action: &DocumentAction,
        batch: &mut DocumentBatch,
        on_missing: F,
    ) where
        F: FnOnce(String, &mut DocumentBatch),
    {
        match self.merge_one(definition, key_name, &action.document, &batch.upserts) {
            MergeOutcome::Applied(doc) => {
                batch.record_upsert(doc);
                batch.results.push(ok_result(key, 200));
            }
            MergeOutcome::Missing => on_missing(key, batch),
            MergeOutcome::Invalid(message) => {
                batch.results.push(fail_result(key, 400, message));
            }
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
        // `queryType`: `simple` (the default) or `full` (Lucene syntax,
        // parsed by the engine's query parser).
        let query_type = match obj.get("queryType").and_then(Value::as_str) {
            Some(value) => QueryType::parse(value).map_err(|e| {
                ApiError::bad_request("InvalidQuery", format!("Invalid queryType: {e}"))
            })?,
            None => QueryType::Simple,
        };

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
            query_type,
            search_mode,
            count,
            top,
            skip,
            filter,
            orderby,
            select,
            facets,
            search_fields,
            highlight_fields,
            highlight_pre_tag,
            highlight_post_tag,
            continuation,
            paging: PagingState {
                filter_raw,
                orderby_raw,
                vector_queries_raw,
            },
            vector_queries,
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
        let SearchPlan {
            skip,
            orderby,
            full_text,
            documents,
            full_text_scores,
            vector_lists,
        } = self.resolve_plan(&definition, query)?;
        let (scored, total, facets) =
            Self::execute_plan(&orderby, &documents, full_text_scores, &vector_lists, query);
        let (page, has_more, next_skip) = Self::paginate(scored, skip, query.top, total);
        let page = Self::project_page(page, query, &full_text, &definition);
        Ok(SearchOutcome {
            total,
            documents: page.documents,
            scores: page.scores,
            highlights: page.highlights,
            facets,
            has_more,
            next_skip,
        })
    }

    /// Resolves a search request into a [`SearchPlan`]: the effective paging
    /// state (a continuation token is authoritative for skip/filter/orderby
    /// and must reference the current document state), the prepared full-text
    /// query, and the per-side score lists.
    ///
    /// # Errors
    ///
    /// Returns an [`ApiError`] (`400 InvalidQuery`) when a continuation token
    /// is invalid or the vector queries changed mid-paging, or the query
    /// engine fails.
    fn resolve_plan(
        &self,
        definition: &IndexDefinition,
        query: &SearchQuery,
    ) -> Result<SearchPlan, ApiError> {
        // The request's vector-query identity, bound into continuation tokens.
        let current_vector_hash: Option<u64> = query
            .paging
            .vector_queries_raw
            .as_ref()
            .map(|raw| vector_query_hash(raw, query.vector_filter_mode.as_str()));

        let (skip, filter, orderby) = Self::resolve_paging(query, definition, current_vector_hash)?;

        let full_text = prepare_full_text(self, query)?;
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

        let vector_lists = if vector_active {
            self.vector_side_score_lists(&definition.name, query, filter.as_ref(), &doc_fields)
        } else {
            Vec::new()
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

        Ok(SearchPlan {
            skip,
            orderby,
            full_text,
            documents,
            full_text_scores,
            vector_lists,
        })
    }

    /// Executes the scored half of a [`SearchPlan`]: merges the per-side score
    /// lists (RRF fusion when both sides produced hits, otherwise a plain
    /// union keeping native scores — best score wins), orders the hits, and
    /// computes facets over the full matched set. Returns the ordered hits,
    /// the match total, and the facets.
    fn execute_plan(
        orderby: &[OrderBy],
        documents: &[Document],
        full_text_scores: BTreeMap<String, f32>,
        vector_lists: &[(f32, BTreeMap<String, f32>)],
        query: &SearchQuery,
    ) -> (Vec<(Document, f32)>, u64, Option<Value>) {
        // Hybrid merge: when both sides produced hits, fuse them with weighted
        // Reciprocal Rank Fusion (RRF, k=60) — the full-text list at weight
        // 1.0 and each vector-query list at its own `weight` — so a document
        // ranked well on either side surfaces and a heavier query contributes
        // more. When only one side is active (vector-only or full-text-only)
        // keep the native scores via a plain union (best score wins).
        let merged = Self::merge_scores(full_text_scores, vector_lists);
        let doc_map: BTreeMap<&str, &Document> = documents
            .iter()
            .map(|doc| (doc.key.as_str(), doc))
            .collect();
        let mut scored: Vec<(Document, f32)> = merged
            .into_iter()
            .filter_map(|(key, score)| doc_map.get(key.as_str()).map(|doc| ((*doc).clone(), score)))
            .collect();
        order_scored(&mut scored, orderby);

        let total = u64::try_from(scored.len()).unwrap_or(u64::MAX);
        let matched: Vec<Document> = scored.iter().map(|(doc, _)| doc.clone()).collect();
        let facets = if query.facets.is_empty() {
            None
        } else {
            Some(compute_facets(&matched, &query.facets))
        };
        (scored, total, facets)
    }

    /// Merges full-text and vector score lists: RRF fusion when both sides
    /// produced hits, otherwise a plain union keeping native scores
    /// (best score wins).
    fn merge_scores(
        full_text_scores: BTreeMap<String, f32>,
        vector_lists: &[(f32, BTreeMap<String, f32>)],
    ) -> BTreeMap<String, f32> {
        if vector_lists.is_empty() || full_text_scores.is_empty() {
            let mut merged = full_text_scores;
            for (_, scores) in vector_lists {
                for (key, score) in scores {
                    merged
                        .entry(key.clone())
                        .and_modify(|best| *best = best.max(*score))
                        .or_insert(*score);
                }
            }
            merged
        } else {
            rrf_fuse_weighted(&full_text_scores, 1.0, vector_lists)
        }
    }

    /// Slices a scored result list into a page: skips `skip`, takes `top`
    /// (or everything when `None`). Returns the page plus `has_more` /
    /// `next_skip`. An empty page ends the sequence even when more documents
    /// exist (e.g. `top=0`): otherwise the next token would encode the same
    /// skip and a token-following client would loop forever on empty pages.
    fn paginate(
        scored: Vec<(Document, f32)>,
        skip: u64,
        top: Option<u64>,
        total: u64,
    ) -> (Vec<(Document, f32)>, bool, u64) {
        let skip_usize = usize::try_from(skip).unwrap_or(usize::MAX);
        let take = top
            .and_then(|t| usize::try_from(t).ok())
            .unwrap_or(usize::MAX);
        let page: Vec<(Document, f32)> = scored.into_iter().skip(skip_usize).take(take).collect();
        let has_more = !page.is_empty()
            && u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX) < total;
        let next_skip = u64::try_from(skip_usize.saturating_add(page.len())).unwrap_or(u64::MAX);
        (page, has_more, next_skip)
    }

    /// Projects a page into response documents: per-document scores,
    /// highlight fragments, and omission of non-retrievable vectors unless
    /// explicitly selected.
    fn project_page(
        page: Vec<(Document, f32)>,
        query: &SearchQuery,
        full_text: &FullTextQuery,
        definition: &IndexDefinition,
    ) -> SearchPage {
        let scores: BTreeMap<String, f32> = page
            .iter()
            .map(|(doc, score)| (doc.key.clone(), *score))
            .collect();
        // Highlight fragments for the returned page, when `highlight`
        // fields were requested.
        let highlights = page_highlights(query, full_text, definition, &page);
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
        SearchPage {
            documents,
            scores,
            highlights,
        }
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

    /// Vector side of [`SearchService::search`]: one `(weight, scores)` list
    /// per vector query, where `scores` is the union of that query's
    /// (field) hits with the best score per document key. Keeping the queries
    /// separate lets the hybrid merge weight each query's RRF contribution.
    /// `preFilter` constrains candidates inside the scan; `postFilter` trims
    /// the retrieved top-k afterwards.
    fn vector_side_score_lists(
        &self,
        definition_name: &str,
        query: &SearchQuery,
        filter: Option<&FilterExpr>,
        doc_fields: &BTreeMap<&str, &Map<String, Value>>,
    ) -> Vec<(f32, BTreeMap<String, f32>)> {
        let pre_filter: Option<KeyPredicate<'_>> = match (&query.vector_filter_mode, filter) {
            (VectorFilterMode::PreFilter, Some(expr)) => Some(Box::new(|key: &str| {
                doc_fields
                    .get(key)
                    .is_some_and(|fields| expr.matches(fields))
            })),
            _ => None,
        };
        let post_filter = matches!(
            (&query.vector_filter_mode, filter),
            (VectorFilterMode::PostFilter, Some(_))
        );
        let mut lists = Vec::with_capacity(query.vector_queries.len());
        for vector_query in &query.vector_queries {
            let mut scores: BTreeMap<String, f32> = BTreeMap::new();
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
            if post_filter {
                if let Some(expr) = filter {
                    scores.retain(|key, _| {
                        doc_fields
                            .get(key.as_str())
                            .is_some_and(|fields| expr.matches(fields))
                    });
                }
            }
            lists.push((vector_query.weight, scores));
        }
        lists
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
                Err(_) => (
                    query.paging.filter_raw.clone(),
                    query.paging.orderby_raw.clone(),
                ),
            },
            None => (
                query.paging.filter_raw.clone(),
                query.paging.orderby_raw.clone(),
            ),
        };
        let vector_query_hash = query
            .paging
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
        filter: Option<&str>,
    ) -> Result<Vec<AutocompleteCompletion>, ApiError> {
        let (documents, suggester) = self.suggester_documents(index, suggester_name)?;
        let filter_expr = filter.map(parse_filter_option).transpose()?;
        let search = search_text.trim();
        let needle = search.to_lowercase();
        if needle.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                "The autocomplete search text must be a non-empty string.",
            ));
        }
        let limit = usize::try_from(top).unwrap_or(usize::MAX);
        let mut seen = BTreeSet::new();
        let mut completions = Vec::new();
        for (_, words) in
            Self::suggester_candidates(&documents, &suggester, filter_expr.as_ref(), &needle)
        {
            for word in words {
                if seen.insert(word.clone()) {
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
        filter: Option<&str>,
    ) -> Result<Vec<Suggestion>, ApiError> {
        let (documents, suggester) = self.suggester_documents(index, suggester_name)?;
        let filter_expr = filter.map(parse_filter_option).transpose()?;
        let needle = search_text.trim().to_lowercase();
        if needle.is_empty() {
            return Err(ApiError::bad_request(
                "InvalidQuery",
                "The suggest search text must be a non-empty string.",
            ));
        }
        let limit = usize::try_from(top).unwrap_or(usize::MAX);
        let mut suggestions = Vec::new();
        for (document, words) in
            Self::suggester_candidates(&documents, &suggester, filter_expr.as_ref(), &needle)
        {
            let Some(text) = words.into_iter().next() else {
                continue;
            };
            suggestions.push(Suggestion {
                document: document.clone(),
                text,
            });
            if suggestions.len() >= limit {
                break;
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

    /// The words of a document's suggester search fields, in field order,
    /// then value order, then word order.
    fn suggester_words<'a>(
        document: &'a Document,
        suggester: &'a Suggester,
    ) -> impl Iterator<Item = String> + 'a {
        suggester.search_fields.iter().flat_map(|field_name| {
            document
                .resolve_path(field_name)
                .into_iter()
                .flat_map(field_words)
        })
    }

    /// The suggester's candidate documents with their matching words: each
    /// document passing `filter` (in key order) paired with the words of the
    /// suggester's search fields that contain `needle` (case-insensitive).
    /// Documents with no matching word are omitted.
    fn suggester_candidates<'a>(
        documents: &'a [Document],
        suggester: &'a Suggester,
        filter: Option<&'a FilterExpr>,
        needle: &'a str,
    ) -> impl Iterator<Item = (&'a Document, Vec<String>)> {
        documents.iter().filter_map(move |document| {
            if !filter.is_none_or(|expr| expr.matches(&document.fields)) {
                return None;
            }
            let words = Self::suggester_words(document, suggester)
                .filter(|word| word.to_lowercase().contains(needle))
                .collect::<Vec<_>>();
            (!words.is_empty()).then_some((document, words))
        })
    }

    pub fn reset(&self) {
        self.storage.reset();
        self.engine.reset();
        self.vectors.reset();
        self.synonym_maps.clear();
        for store in &self.named_resources {
            store.clear();
        }
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
        let Ok(alias) = self.get_named_resource(ResourceKind::Alias, name) else {
            return name.to_owned();
        };
        alias.target_index().unwrap_or_else(|| name.to_owned())
    }

    /// Analyzer names accepted by the analyze-text endpoint; the single
    /// source of truth is [`crate::query::KNOWN_ANALYZERS`]. Unknown names
    /// are rejected explicitly rather than silently mapped.
    const KNOWN_ANALYZERS: &'static [&'static str] = crate::query::KNOWN_ANALYZERS;

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

/// Whether an in-place index update is compatible with the stored documents:
/// every field in the old schema must still exist in the new schema with the
/// same name and type (and, for vector fields, the same dimensions). Adding
/// fields or changing field attributes (`filterable`, `facetable`, `searchable`,
/// analyzers, ...) is compatible; removing a field or changing its type or
/// vector dimensions is not, in which case the index is replaced and its
/// documents discarded.
fn schema_compatible(old: &IndexDefinition, new: &IndexDefinition) -> bool {
    let new_fields: BTreeMap<&str, &FieldDefinition> =
        new.fields.iter().map(|f| (f.name.as_str(), f)).collect();
    old.fields.iter().all(|old_field| {
        new_fields
            .get(old_field.name.as_str())
            .is_some_and(|new_field| {
                old_field.field_type == new_field.field_type
                    && old_field.vector_dimensions == new_field.vector_dimensions
            })
    })
}

/// Builds the engine-level full-text query for a search: parses the search
/// text (simple-query clauses, or raw Lucene text for `queryType=full`),
/// attaches synonym expansions for single-term clauses, and applies the
/// request's `searchMode` and `searchFields` (names plus `field^N` boosts).
///
/// # Errors
///
/// Returns an [`ApiError`] (`400 InvalidQuery`) when the search text is
/// malformed.
fn prepare_full_text(
    service: &SearchService,
    query: &SearchQuery,
) -> Result<FullTextQuery, ApiError> {
    let mut full_text = match (&query.search, query.query_type) {
        (Some(text), QueryType::Full) => FullTextQuery {
            lucene: Some(text.clone()),
            ..FullTextQuery::default()
        },
        (Some(text), QueryType::Simple) => parse_search_text(text).map_err(|e| {
            ApiError::bad_request("InvalidQuery", format!("Invalid search text: {e}"))
        })?,
        (None, _) => FullTextQuery::default(),
    };
    full_text.mode = query.search_mode;
    // Synonym expansions for single-term clauses (fuzzy terms and phrases do
    // not expand). Each pair is `(raw term, raw expansions)`; the engine
    // analyzes both sides per field.
    for clause in &full_text.required {
        if let Clause::Term(term) = clause {
            let expansions = service.synonym_expansions(term);
            if !expansions.is_empty() {
                full_text.synonyms.push((term.clone(), expansions));
            }
        }
    }
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

/// Maps a query-engine failure onto an Azure-compatible [`ApiError`].
fn engine_error(index: &str, error: QueryError) -> ApiError {
    match error {
        QueryError::IndexNotFound(name) => {
            ApiError::not_found(format!("Index {name:?} was not found."))
        }
        QueryError::InvalidQuery(message) => {
            ApiError::bad_request("InvalidQuery", format!("Invalid search text: {message}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::testutil::{err, ok};
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use serde_json::json;

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

    fn field(name: &str, field_type: &str) -> FieldDefinition {
        ok(FieldDefinition::from_json(
            json!({"name": name, "type": field_type}),
        ))
    }

    fn definition(fields: Vec<FieldDefinition>) -> IndexDefinition {
        IndexDefinition {
            name: "items".to_owned(),
            fields,
            suggesters: Vec::new(),
            vector_search: None,
            raw: json!({}),
        }
    }

    #[test]
    fn schema_compatible_allows_added_fields_and_attribute_changes() {
        let old = definition(vec![
            field("id", "Edm.String"),
            field("title", "Edm.String"),
        ]);
        // Adding a field and changing attributes (same name + type) is compatible.
        let new = definition(vec![
            field("id", "Edm.String"),
            field("title", "Edm.String"),
            field("price", "Edm.Double"),
        ]);
        assert!(schema_compatible(&old, &new));
    }

    #[test]
    fn schema_compatible_rejects_removed_or_retyped_fields() {
        let old = definition(vec![
            field("id", "Edm.String"),
            field("title", "Edm.String"),
        ]);
        // Removing a field is incompatible.
        let removed = definition(vec![field("id", "Edm.String")]);
        assert!(!schema_compatible(&old, &removed));
        // Changing a field's type is incompatible.
        let retyped = definition(vec![
            field("id", "Edm.String"),
            field("title", "Edm.Double"),
        ]);
        assert!(!schema_compatible(&old, &retyped));
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
        let completions = ok(service.autocomplete("items", "sg", "bos", 5, None));
        let texts: Vec<_> = completions.iter().map(|c| c.text.clone()).collect();
        assert_eq!(texts, vec!["Boston", "boston"]);
        assert_eq!(completions[0].query_plus_text, "bos Boston");
        // Case-insensitive: "BOS" matches the same words.
        let completions = ok(service.autocomplete("items", "sg", "BOS", 5, None));
        assert_eq!(completions.len(), 2);
        // No match.
        assert!(ok(service.autocomplete("items", "sg", "zzz", 5, None)).is_empty());
        // top limits the results.
        let completions = ok(service.autocomplete("items", "sg", "bos", 1, None));
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
        let suggestions = ok(service.suggest("items", "sg", "bos", 5, None));
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
        let suggestions = ok(service.suggest("items", "sg", "bos", 1, None));
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].document.key, "1");
    }

    #[test]
    fn suggest_and_autocomplete_filter_narrow_candidates() {
        let service = service();
        ok(service.create_index(&suggester_index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "Boston Harbor Hotel", "tags": ["spa"]}),
                json!({"id": "2", "title": "Seattle Downtown", "tags": ["boston", "wifi"]}),
            ],
        );
        // Without a filter, "bos" matches doc 1 (title) and doc 2 (tag).
        assert_eq!(ok(service.suggest("items", "sg", "bos", 5, None)).len(), 2);
        // A filter narrows the candidates: only the wifi-tagged doc (2) remains.
        let suggestions =
            ok(service.suggest("items", "sg", "bos", 5, Some("tags/any(t: t eq 'wifi')")));
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].document.key, "2");
        // Autocomplete narrows the same way: "bos" only completes from doc 2's
        // tag, not doc 1's title.
        let completions =
            ok(service.autocomplete("items", "sg", "bos", 5, Some("tags/any(t: t eq 'wifi')")));
        let texts: Vec<_> = completions.iter().map(|c| c.text.clone()).collect();
        assert_eq!(texts, vec!["boston"]);
        // An invalid filter is rejected.
        let api_error = err(service.suggest("items", "sg", "bos", 5, Some("tags/any(t:")));
        assert_eq!(api_error.code, "InvalidQuery");
    }

    #[test]
    fn autocomplete_and_suggest_reject_bad_requests() {
        let service = service();
        ok(service.create_index(&suggester_index_body()));
        // Missing index.
        let api_error = err(service.autocomplete("missing", "sg", "bos", 5, None));
        assert_eq!(api_error.status, axum::http::StatusCode::NOT_FOUND);
        // Unknown suggester.
        let api_error = err(service.autocomplete("items", "nope", "bos", 5, None));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.suggest("items", "nope", "bos", 5, None));
        assert_eq!(api_error.code, "InvalidQuery");
        // Empty search text.
        let api_error = err(service.autocomplete("items", "sg", "   ", 5, None));
        assert_eq!(api_error.code, "InvalidQuery");
        let api_error = err(service.suggest("items", "sg", "", 5, None));
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

    #[test]
    fn synonym_expansions_match_case_insensitively() {
        let service = service();
        ok(service.create_synonym_map("m", "solr", "USA, United States\nWA => Washington"));
        assert_eq!(service.synonym_expansions("usa"), vec!["states", "united"]);
        assert_eq!(service.synonym_expansions("USA"), vec!["states", "united"]);
        assert_eq!(service.synonym_expansions("states"), vec!["united", "usa"]);
        // Explicit mappings only expand from the left side.
        assert_eq!(service.synonym_expansions("wa"), vec!["washington"]);
        assert!(service.synonym_expansions("washington").is_empty());
        assert!(service.synonym_expansions("unknown").is_empty());
    }

    #[test]
    fn synonym_search_expands_query_terms() {
        let service = service();
        ok(service.create_index(&index_body()));
        upload(
            &service,
            vec![
                json!({"id": "1", "title": "hotels in Washington"}),
                json!({"id": "2", "title": "flights to Boston"}),
            ],
        );
        ok(service.create_synonym_map("m", "solr", "WA, Washington"));
        // "wa" matches document 1 via the "washington" expansion.
        let query = ok(service.parse_search("items", &json!({"search": "wa"})));
        let outcome = ok(service.search("items", &query));
        let keys: Vec<&str> = outcome.documents.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, vec!["1"]);
        // Without the map, "wa" matches nothing.
        service
            .delete_synonym_map("m")
            .unwrap_or_else(|e| panic!("delete failed: {e}"));
        let query = ok(service.parse_search("items", &json!({"search": "wa"})));
        let outcome = ok(service.search("items", &query));
        assert!(outcome.documents.is_empty());
    }
}
