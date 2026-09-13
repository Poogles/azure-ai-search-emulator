//! Named resources (`ResourceKind`) and the generic `ResourceStore`.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::error::ApiError;
use crate::sync_util::{read_unpoisoned, write_unpoisoned};

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

/// Builds a [`NamedResource`] from its name, etag, and stored request body.
pub(crate) fn named_resource(name: &str, etag: String, raw: &Value) -> NamedResource {
    NamedResource {
        name: name.to_owned(),
        etag,
        raw: raw.clone(),
    }
}

/// The four kinds of named resources managed at the service level: synonym
/// maps, index aliases, knowledge sources, and knowledge bases. Each kind is
/// modeled as data: its OData-style path prefix, the labels and error codes
/// used in its error messages, and (for the [`NamedResource`] kinds) its slot
/// in [`SearchService::named_resources`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    SynonymMap,
    Alias,
    KnowledgeSource,
    KnowledgeBase,
}

impl ResourceKind {
    /// All four kinds.
    pub const ALL: [ResourceKind; 4] = [
        ResourceKind::SynonymMap,
        ResourceKind::Alias,
        ResourceKind::KnowledgeSource,
        ResourceKind::KnowledgeBase,
    ];

    /// The kind's collection name, e.g. `aliases`.
    #[must_use]
    pub const fn collection_name(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "synonymmaps",
            ResourceKind::Alias => "aliases",
            ResourceKind::KnowledgeSource => "knowledgesources",
            ResourceKind::KnowledgeBase => "knowledgebases",
        }
    }

    /// The kind's collection path, e.g. `/aliases`.
    #[must_use]
    pub const fn collection_path(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "/synonymmaps",
            ResourceKind::Alias => "/aliases",
            ResourceKind::KnowledgeSource => "/knowledgesources",
            ResourceKind::KnowledgeBase => "/knowledgebases",
        }
    }

    /// The OData-style path prefix of the kind's named segments, e.g.
    /// `aliases(`.
    #[must_use]
    pub const fn path_prefix(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "synonymmaps(",
            ResourceKind::Alias => "aliases(",
            ResourceKind::KnowledgeSource => "knowledgesources(",
            ResourceKind::KnowledgeBase => "knowledgebases(",
        }
    }

    /// The kind's named path prefix with a leading slash, e.g. `/aliases(`.
    #[must_use]
    pub const fn named_path_prefix(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "/synonymmaps(",
            ResourceKind::Alias => "/aliases(",
            ResourceKind::KnowledgeSource => "/knowledgesources(",
            ResourceKind::KnowledgeBase => "/knowledgebases(",
        }
    }

    /// The lowercase label used in request-validation messages, e.g.
    /// `Invalid alias path segment ...`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "synonym map",
            ResourceKind::Alias => "alias",
            ResourceKind::KnowledgeSource => "knowledge source",
            ResourceKind::KnowledgeBase => "knowledge base",
        }
    }

    /// The capitalized label used in not-found messages, e.g.
    /// `Alias "x" was not found.`.
    #[must_use]
    pub const fn not_found_label(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "Synonym map",
            ResourceKind::Alias => "Alias",
            ResourceKind::KnowledgeSource => "Knowledge source",
            ResourceKind::KnowledgeBase => "Knowledge base",
        }
    }

    /// The label used in create-conflict messages (`A {kind} with name ...
    /// already exists.`). Only synonym maps name themselves; the other kinds
    /// share the generic `resource` label.
    #[must_use]
    pub const fn conflict_kind(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "synonym map",
            ResourceKind::Alias | ResourceKind::KnowledgeSource | ResourceKind::KnowledgeBase => {
                "resource"
            }
        }
    }

    /// The error code for a create conflict, e.g. `AliasAlreadyExists`.
    #[must_use]
    pub const fn conflict_code(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "SynonymMapAlreadyExists",
            ResourceKind::Alias => "AliasAlreadyExists",
            ResourceKind::KnowledgeSource => "KnowledgeSourceAlreadyExists",
            ResourceKind::KnowledgeBase => "KnowledgeBaseAlreadyExists",
        }
    }

    /// The error code for invalid request segments and bodies, e.g.
    /// `InvalidAlias`.
    #[must_use]
    pub const fn invalid_code(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "InvalidSynonymMap",
            ResourceKind::Alias => "InvalidAlias",
            ResourceKind::KnowledgeSource => "InvalidKnowledgeSource",
            ResourceKind::KnowledgeBase => "InvalidKnowledgeBase",
        }
    }

    /// The error for a malformed named path segment of this kind. `raw` is
    /// the full segment. Synonym maps report the segment with the prefix
    /// stripped (a pre-existing message quirk); the other kinds report the
    /// full segment.
    #[must_use]
    pub fn invalid_segment_error(self, raw: &str) -> ApiError {
        let shown = match self {
            ResourceKind::SynonymMap => raw
                .strip_prefix(self.path_prefix())
                .unwrap_or(raw)
                .to_owned(),
            _ => raw.to_owned(),
        };
        ApiError::bad_request(
            self.invalid_code(),
            format!(
                "Invalid {} path segment {shown:?}; expected {}'name').",
                self.label(),
                self.collection_name()
            ),
        )
    }

    /// The logging operation name for `POST` on the kind's collection, e.g.
    /// `createAlias`.
    #[must_use]
    pub const fn create_operation(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "createSynonymMap",
            ResourceKind::Alias => "createAlias",
            ResourceKind::KnowledgeSource => "createKnowledgeSource",
            ResourceKind::KnowledgeBase => "createKnowledgeBase",
        }
    }

    /// The logging operation name for `GET` on the kind's collection, e.g.
    /// `listAliases`.
    #[must_use]
    pub const fn list_operation(self) -> &'static str {
        match self {
            ResourceKind::SynonymMap => "listSynonymMaps",
            ResourceKind::Alias => "listAliases",
            ResourceKind::KnowledgeSource => "listKnowledgeSources",
            ResourceKind::KnowledgeBase => "listKnowledgeBases",
        }
    }

    /// The logging operation name for a method on the kind's named segments,
    /// e.g. `getAlias`; `unknown` for methods the route does not serve.
    #[must_use]
    pub fn named_operation(self, method: &str) -> &'static str {
        match self {
            ResourceKind::SynonymMap => match method {
                "GET" => "getSynonymMap",
                "PUT" => "createOrUpdateSynonymMap",
                "DELETE" => "deleteSynonymMap",
                _ => "unknown",
            },
            ResourceKind::Alias => match method {
                "GET" => "getAlias",
                "PUT" => "createOrUpdateAlias",
                "DELETE" => "deleteAlias",
                _ => "unknown",
            },
            ResourceKind::KnowledgeSource => match method {
                "GET" => "getKnowledgeSource",
                "PUT" => "createOrUpdateKnowledgeSource",
                "DELETE" => "deleteKnowledgeSource",
                _ => "unknown",
            },
            ResourceKind::KnowledgeBase => match method {
                "GET" => "getKnowledgeBase",
                "PUT" => "createOrUpdateKnowledgeBase",
                "POST" => "retrieveKnowledgeBase",
                "DELETE" => "deleteKnowledgeBase",
                _ => "unknown",
            },
        }
    }

    /// The slot of this kind in [`SearchService::named_resources`]. Only the
    /// [`NamedResource`] kinds (alias, knowledge source, knowledge base) have
    /// slots; synonym maps are stored separately.
    ///
    /// # Panics
    ///
    /// Panics when called for [`ResourceKind::SynonymMap`].
    #[must_use]
    pub fn named_index(self) -> usize {
        match self {
            ResourceKind::Alias => 0,
            ResourceKind::KnowledgeSource => 1,
            ResourceKind::KnowledgeBase => 2,
            ResourceKind::SynonymMap => panic!("synonym maps are not stored as NamedResource"),
        }
    }
}

/// Service-level storage for a collection of named resources, keyed by name
/// (sorted), with an incrementing etag counter. `T` is the stored resource
/// type (a [`NamedResource`] for aliases, knowledge sources, and knowledge
/// bases; a [`SynonymMap`] for synonym maps); `build` constructs it from its
/// name and a fresh etag.
#[derive(Debug)]
pub(crate) struct ResourceStore<T> {
    items: std::sync::RwLock<std::collections::BTreeMap<String, T>>,
    etags: AtomicU64,
}

impl<T> Default for ResourceStore<T> {
    fn default() -> Self {
        Self {
            items: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            etags: AtomicU64::new(0),
        }
    }
}

impl<T: Clone> ResourceStore<T> {
    pub(crate) fn lock_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<String, T>> {
        write_unpoisoned(&self.items)
    }

    pub(crate) fn lock_read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, std::collections::BTreeMap<String, T>> {
        read_unpoisoned(&self.items)
    }

    /// Creates a new resource, failing if one with the same name exists.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::conflict`] if the name is already taken.
    pub(crate) fn create<F>(&self, kind: ResourceKind, name: &str, build: F) -> Result<T, ApiError>
    where
        F: FnOnce(&str, String) -> T,
    {
        let mut items = self.lock_write();
        if items.contains_key(name) {
            return Err(ApiError::conflict(
                kind.conflict_code(),
                format!(
                    "A {} with name {name:?} already exists.",
                    kind.conflict_kind()
                ),
            ));
        }
        Ok(self.insert(&mut items, name, build))
    }

    /// Creates or replaces a resource. Replacing issues a new etag.
    pub(crate) fn create_or_update<F>(&self, name: &str, build: F) -> T
    where
        F: FnOnce(&str, String) -> T,
    {
        let mut items = self.lock_write();
        self.insert(&mut items, name, build)
    }

    /// Returns a clone of the resource with the given name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    pub(crate) fn get(&self, kind: ResourceKind, name: &str) -> Result<T, ApiError> {
        self.lock_read().get(name).cloned().ok_or_else(|| {
            ApiError::not_found(format!(
                "{} {name:?} was not found.",
                kind.not_found_label()
            ))
        })
    }

    /// Returns clones of all resources, sorted by name.
    #[must_use]
    pub(crate) fn list(&self) -> Vec<T> {
        self.lock_read().values().cloned().collect()
    }

    /// Returns `true` when a resource with the given name exists.
    #[must_use]
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lock_read().contains_key(name)
    }

    /// Deletes a resource by name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::not_found`] if the resource does not exist.
    pub(crate) fn delete(&self, kind: ResourceKind, name: &str) -> Result<(), ApiError> {
        let mut items = self.lock_write();
        if items.remove(name).is_some() {
            Ok(())
        } else {
            Err(ApiError::not_found(format!(
                "{} {name:?} was not found.",
                kind.not_found_label()
            )))
        }
    }

    pub(crate) fn clear(&self) {
        self.lock_write().clear();
    }

    fn insert<F>(
        &self,
        items: &mut std::collections::BTreeMap<String, T>,
        name: &str,
        build: F,
    ) -> T
    where
        F: FnOnce(&str, String) -> T,
    {
        // Azure etags are quoted hex strings (e.g. `"0x8D..."`); SDKs echo
        // them back in `If-Match`, so match the shape, not just uniqueness.
        // +1 so the first etag is non-zero, as Azure's (timestamp-derived) are.
        let etag = format!(
            "\"0x{:08X}\"",
            self.etags.fetch_add(1, Ordering::SeqCst) + 1
        );
        let resource = build(name, etag);
        items.insert(name.to_owned(), resource.clone());
        resource
    }
}
