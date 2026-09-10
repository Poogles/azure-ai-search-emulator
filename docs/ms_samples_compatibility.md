---
status: complete
status_last_reviewed: 2026-09-10
---

# Microsoft Reference-Samples Compatibility Probe

The emulator is tested against Microsoft's own reference samples for
`azure-search-documents`
(`Azure/azure-sdk-for-python`, `sdk/search/azure-search-documents/samples`).
This document describes how the probe works, how to run it, and the current
state of completion. The probe itself lives in
`source/tests/python/tests/ms_samples/`; this document is the human-readable
summary, the `KNOWN_ISSUES` registry in `test_ms_samples.py` is the
machine-readable one.

## How it works

- The samples are **not vendored**. They are pulled from a sparse, shallow,
  blob-filtered submodule (`source/tests/python/ms_samples/upstream`) that
  checks out only `sdk/search/azure-search-documents/samples` (~13 MB instead
  of the ~1 GB full repo). The submodule pointer pins the tested upstream
  commit; currently `67b56be` on `main`.
- Each discovered sync sample (`sample_*.py`, excluding `sample_utils.py` and
  `*_async.py`) is run **unmodified** as a subprocess, with the
  `AZURE_SEARCH_*` environment variables pointed at the emulator.
- Two harness-only adaptations, both documented in
  `source/tests/python/tests/ms_samples/conftest.py`:
  - The samples do not pin an `api-version`, so they send the SDK default
    (`2026-04-01` for the pinned SDK 12.0.0). The probe container accepts
    `2024-07-01,2026-04-01`.
  - Subprocesses re-apply the plain-HTTP shims via
    `ms_samples/shims/sitecustomize.py` on `PYTHONPATH`.
- Samples that assume the portal `hotels-sample-index` get it from
  `ms_samples/setup_hotels.py` (schema subset the emulator supports,
  including an `Address` complex field and `GeoJSON` `Location` values on the
  seed documents).
- Async mirrors are excluded: the emulator is a plain HTTP service, so the
  sync and async SDK paths exercise the same wire contract.

## Running it

```sh
make ms-samples         # populate the submodule (sparse, shallow; idempotent)
make test-ms            # run the probe (implies ms-samples)
make ms-samples-update  # bump the submodule to latest upstream main (see below)
```

## Result model

Each sample is classified in `KNOWN_ISSUES` (`test_ms_samples.py`):

| Category | Meaning | Suite behaviour |
|----------|---------|-----------------|
| *(absent)* | Expected to pass | Fails if the sample errors (new upstream sample needing triage) |
| `gap` | Emulator does not implement the feature; pinned to the Azure error signature | Fails if the sample starts passing (gap closed — remove from registry) or the signature changes |
| `falsepass` | Exits 0 but the operation silently did not take effect | Fails if the side-effect starts happening |
| `skip` | Cannot run here for non-emulator reasons (SDK version, external service) | Skipped with the reason |

## Current state

32 sync samples discovered. `make test-ms`: **15 passed, 17 skipped, 0 failed.**
(All 15 passes are genuine; there are no gap pins remaining.)

### Passes (15)

| Sample | Notes |
|--------|-------|
| `sample_query_simple.py` | Genuine pass: simple text search returns the seeded hotel. |
| `sample_documents_buffered_sender.py` | Genuine pass since the GeographyPoint fix: the `HotelId: 100` document is stored (previously a `falsepass` — the emulator rejected the SDK's `{"type": "Point", "coordinates": [...]}` shape while `SearchIndexingBufferedSender` swallowed the per-document error). |
| `sample_documents_crud.py` | Genuine pass since the GeographyPoint fix (upload/merge of doc 100) plus the new `GET /indexes('{name}')/docs('{key}')` endpoint (get/delete of doc 100). |
| `sample_query_facets.py` | Genuine pass since the facet-options fix (`Category,count:3`). |
| `sample_query_filter.py` | Genuine pass since the complex-type fix (`Address/StateProvince` filter against the `Address` complex field in the seeded hotels index). |
| `sample_index_analyze_text.py` | Genuine pass since the `POST /search.analyze` fix: tokenizes text and returns tokens with offsets. |
| `sample_query_session.py` | Genuine pass since the `sessionId` fix: the option is accepted and silently ignored (deterministic ordering makes session affinity irrelevant). |
| `sample_index_synonym_map_crud.py` | Genuine pass since the synonym-map CRUD fix: create (incl. from file), list, get, and delete of Solr-format maps all round-trip. Maps are stored but inert (see `known_differences.md`). |
| `sample_query_autocomplete.py` | Genuine pass since the autocomplete fix: `POST /docs/search.post.autocomplete` returns prefix-matched completions (`text` + `queryPlusText`). The seeded hotels yield no match for `"bo"`, so the sample passes on the empty `value` array. |
| `sample_query_suggestions.py` | Genuine pass since the suggest fix: `POST /docs/search.post.suggest` returns matching documents plus `@search.text`. The seeded hotels yield no match for `"coffee"`, so the sample passes on the empty `value` array. |
| `sample_query_vector.py` | Genuine pass since the SDK 12 bump plus the OData lambda-filter fix: the sample creates a vector index (`Collection(Edm.Single)` + `vectorSearch` profiles), uploads 7 pre-embedded hotel docs, and runs single-vector, filtered-vector (`Tags/any(tag: tag eq 'free wifi')`), and hybrid searches. |
| `sample_index_client_custom_request.py` | Genuine pass since the SDK 12 bump: `SearchIndexClient.send_request` GETs the seeded hotels index and prints the echoed definition. |
| `sample_search_client_custom_request.py` | Genuine pass since the SDK 12 bump: `SearchClient.send_request` GETs `/docs/$count` and prints the document count (4). |
| `sample_index_alias_crud.py` | Genuine pass since the alias CRUD fix: create, get, update (re-point to the v2 index, which exercises collection-of-complex), and delete of the `hotels-sample-alias` all round-trip. |
| `sample_agentic_retrieval.py` | Genuine pass since the knowledge CRUD + retrieval fix: creates a knowledge source and knowledge base over the seeded hotels index, `POST /knowledgebases('{name}')/retrieve` returns an empty response (no model inference — see `known_differences.md`), and cleanup deletes both. Passes on the empty `response` array. |

### Documented emulator gaps (0)

All emulator gaps are closed. Gaps 11–13 landed (see the tracker and plans below); their `KNOWN_ISSUES` pins have been removed.

Historical plans for all gaps are in [Gap implementation plans](#gap-implementation-plans) below. The remaining 17 skips are tracked as Gaps 9–10 (external-service, `☐ not started`) plus preview-SDK / SDK-bug skips.

### Gap implementation plans

Detailed requirements for closing each gap, in recommended implementation
order (each builds on prior auth-guard / route changes).

#### Completion tracker

| # | Gap | Status | PR / commit |
|---|-----|--------|-------------|
| 6 | `sessionId` | ☑ done | |
| 1 | Document count (`$count`) | ☑ done | |
| 7 | Service stats (`/servicestats`) | ☑ done | |
| 2 | Analyze text (`/search.analyze`) | ☑ done | |
| 3 | Synonym maps CRUD | ☑ done | |
| 4 | Autocomplete | ☑ done | |
| 5 | Suggest | ☑ done | |
| 8 | SDK 12 harness bump (18 samples) | ☑ done | |
| 9 | Indexers + data sources (3 samples) | ☐ not started | |
| 10 | AAD bearer auth (1 sample half) | ☐ not started | |
| 11 | Index aliases (1 sample) | ☑ done | |
| 12 | Collection-of-complex field types (1 sample) | ☑ done | |
| 13 | Knowledge sources/bases + agentic retrieval (2 samples) | ☑ done | |

---

#### Gap 6 — `sessionId` (trivial, ~5 min) ✅

**Sample:** `sample_query_session.py`
**Route:** N/A (field in search POST body)
**Was:** 400 `UnsupportedQuery`

**What was done:**

- [x] Removed `"sessionId"` from `UNSUPPORTED_SEARCH_OPTIONS` in
      `source/rust/src/service/mod.rs`. The field is now silently ignored
      (the emulator uses deterministic ordering and constant scoring, so
      session affinity is irrelevant).
- [x] Updated `docs/supported_operations.md`: moved `sessionId` to "accepted
      but inert".
- [x] Removed the `KNOWN_ISSUES` entry in
      `source/tests/python/tests/ms_samples/test_ms_samples.py`.
- [x] `make test-ms` confirms the sample passes.

---

#### Gap 1 — Document count `GET /indexes('{name}')/docs/$count` (easy, ~30 min) ✅

**Sample:** `sample_authentication.py`
**Route:** `GET /indexes('{indexName}')/docs/$count`
**Was:** 404 `Not Found`

**What was done:**

- [x] Added route `GET /{index}/docs/$count` to the azure sub-router in
      `source/rust/src/api/mod.rs`. The literal three-segment path takes
      precedence over the existing `/{index}/{key}` catch-all.
- [x] Added a handler that parses the index name, calls
      `service.count_documents(index)`, and returns the count as a bare
      integer body with `Content-Type: application/json`.
- [x] Added contract tests in `source/rust/tests/contract/document_management.rs`
      (count returns correct value, empty index returns 0, missing index
      returns 404).
- [x] Removed the `KNOWN_ISSUES` entry.
- [x] `make test-ms` confirms the API-key half of the sample passes.

**Notes:** The AAD half of `sample_authentication.py` requires `azure-identity`
and a real Azure AD environment; the sample is classified as `skip` for that
reason. The `GET /docs/$count` endpoint itself is fully implemented and
tested.

---

#### Gap 7 — Service stats `GET /servicestats` (easy, ~30 min) ✅

**Sample:** `sample_knowledge_service_stats_preview.py`
**Route:** `GET /servicestats`
**Was:** 400 `InvalidIndexName` (fell through to index-path parsing)

**What was done:**

- [x] Added route `GET /servicestats` to the azure sub-router in
      `source/rust/src/api/mod.rs` (literal path, takes precedence over
      `/{index}`).
- [x] Handler returns a static JSON response:
      ```json
      {
        "counters": {
          "knowledgeBaseCounter": {"usage": 0},
          "knowledgeSourceCounter": {"usage": 0}
        },
        "limits": {
          "maxVectorIndexSizePerIndexInBytes": 1073741824
        }
      }
      ```
- [x] Extended `azure_guard` middleware to cover the `/servicestats` path.
- [x] Added contract tests (correct response shape, requires auth).
- [x] Removed the `KNOWN_ISSUES` entry.

**Notes:** The endpoint is fully implemented and tested. The sample is
classified as `skip` because SDK 11.6.0's `get_service_statistics()` returns
a plain dict (not the typed model the sample's attribute access expects);
SDK ≥12 is needed for the sample to pass end-to-end.

---

#### Gap 2 — Analyze text `POST /indexes('{name}')/search.analyze` (easy-medium, ~1 hr) ✅

**Sample:** `sample_index_analyze_text.py`
**Route:** `POST /indexes('{indexName}')/search.analyze`
**Was:** 404 `Not Found`

**What was done:**

- [x] Added `AnalyzeToken` struct and `analyze_with_offsets()` function in
      `source/rust/src/query/mod.rs`, using Tantivy's `TokenStream` to
      provide `token.text`, `token.offset_from`, `token.offset_to`, and
      `token.position`.
- [x] Added route `POST /{index}/search.analyze` to the azure sub-router.
      Axum gives precedence to literal routes over the `/{index}/{key}`
      parameterized route.
- [x] Handler: parses the index name, validates the index exists, parses the
      JSON body (extracts `text`; accepts but ignores `analyzerName` and
      `field`), calls `analyze_with_offsets`, returns:
      ```json
      {"tokens": [{"token": "...", "startOffset": 0, "endOffset": 4, "position": 0}]}
      ```
- [x] Added contract tests (tokens with offsets, missing text field returns
      400, missing index returns 404).
- [x] Removed the `KNOWN_ISSUES` entry.
- [x] `make test-ms` confirms the sample passes.

**Notes:** The sample uses `analyzer_name="standard.lucene"`. The emulator
always uses Tantivy's default English analyzer; this is an acceptable
simplification (documented in `known_differences.md`).

---

#### Gap 3 — Synonym maps CRUD (medium, ~2-3 hrs) ✅

**Sample:** `sample_index_synonym_map_crud.py`
**Routes:**
- `POST /synonymmaps` (create)
- `GET /synonymmaps` (list)
- `GET /synonymmaps('{name}')` (get by name)
- `PUT /synonymmaps('{name}')` (create or update)
- `DELETE /synonymmaps('{name}')` (delete)

**Was:** 405 `Method Not Allowed`

**What was done:**

- [x] Defined a `SynonymMap` struct (`name`, `format`, `synonyms`, `etag`)
      in `source/rust/src/service/mod.rs`. Note: the pinned SDK sends
      `synonyms` as a single newline-joined **string** (not an array), so it
      is stored and echoed in that wire form.
- [x] Added storage: a `BTreeMap<String, SynonymMap>` in `SearchService`
      (service-level, not per-index), with an incrementing counter for etags.
      `reset()` clears the maps.
- [x] Added routes. `POST /synonymmaps` and `GET /synonymmaps` are literal
      routes; `GET/PUT/DELETE /synonymmaps('{name}')` dispatch in the existing
      `/{index}` handlers (the raw segment starts with `synonymmaps(`).
- [x] Extended `azure_guard` to cover `/synonymmaps` and `/synonymmaps(` paths.
- [x] Response format:
      ```json
      {"name": "...", "format": "solr", "synonyms": "a, b\nc, d", "@odata.etag": "1"}
      ```
      List response: `{"value": [...]}` (sorted by name). Create/update return
      `201`; delete returns `204`; missing map returns `404 ResourceNotFound`;
      duplicate create returns `409 SynonymMapAlreadyExists`; invalid
      definition (missing name, non-`solr` format, empty synonyms, path/body
      name mismatch) returns `400 InvalidSynonymMap`.
- [x] Added contract tests in `source/rust/tests/contract/synonym_maps.rs`
      (create, duplicate, list, get, update, delete, auth, validation, reset).
- [x] Documented in `known_differences.md`: synonym maps are stored but inert
      (do not affect search results).
- [x] Removed the `KNOWN_ISSUES` entry in
      `source/tests/python/tests/ms_samples/test_ms_samples.py`.
- [x] `make test-ms` confirms the sample passes.

---

#### Gap 4 — Autocomplete `POST /indexes('{name}')/docs/search.post.autocomplete` (medium-hard, ~3-4 hrs) ✅

**Sample:** `sample_query_autocomplete.py`
**Route:** `POST /indexes('{indexName}')/docs/search.post.autocomplete?api-version=...`
**Current failure:** none (closed)

**What was required:**

- [x] **Suggester schema:** Add a `Suggester` struct (`name: String`,
      `search_fields: Vec<String>`) and a `suggesters: Vec<Suggester>` field
      to `IndexDefinition`.
- [x] Parse the `suggesters` array in `parse_index_definition`.
- [x] Validate in `validate_schema` that suggester search fields exist and
      are `searchable`.
- [x] Update `setup_hotels.py` to include a suggester:
      ```python
      from azure.search.documents.indexes.models import SearchSuggester
      # In the SearchIndex definition:
      suggesters=[SearchSuggester(name="sg", source_fields=["HotelName"])]
      ```
- [x] Add route `POST /{index}/docs/search.post.autocomplete` to the azure
      sub-router.
- [x] Handler: parse index name, extract `suggesterName` (body first, query
      fallback), parse body (extract `search`), look up the suggester in the
      index definition, perform case-insensitive prefix matching on the
      suggester's search fields across all documents. Return:
      ```json
      {"value": [{"text": "Boston", "queryPlusText": "bo Boston"}]}
      ```
      Limit to `top` results (default 5, from body or query param).

Wire notes (correcting the original plan): the pinned SDK 11.6.0 sends
`search` (not `searchText`) and `suggesterName` in the POST body (only
`api-version` is a query param), and serializes suggester fields as
`sourceFields` (not `searchFields`). The emulator accepts `search` (plus a
`searchText` alias), body-first `suggesterName` with query fallback, and
both `searchFields` / `sourceFields`.

- [x] Add contract tests.
- [x] Document in `known_differences.md`: autocomplete uses simple prefix
      matching (not Azure's full suggester algorithm with scoring).
- [x] Remove the `KNOWN_ISSUES` entry.
- [x] Run `make test-ms` to confirm.

---

#### Gap 5 — Suggest `POST /indexes('{name}')/docs/search.post.suggest` (medium-hard, ~2-3 hrs) ✅

**Sample:** `sample_query_suggestions.py`
**Route:** `POST /indexes('{indexName}')/docs/search.post.suggest?api-version=...`
**Current failure:** none (closed; landed with Gap 4 — shared suggester infrastructure)

**What was required:**

- [x] Reuse the suggester infrastructure from Gap 4 (schema, parsing,
      validation, `setup_hotels.py`).
- [x] Add route `POST /{index}/docs/search.post.suggest` to the azure
      sub-router.
- [x] Handler: same pattern as autocomplete, but return full documents (all
      retrievable fields) with an additional `@search.text` field containing
      the matched text:
      ```json
      {"value": [{"@search.text": "coffee", "HotelId": "1", "HotelName": "...", ...}]}
      ```
      The sample calls `get_document(key=result["HotelId"])` for each
      suggestion, so the key field must be present.
- [x] Add contract tests.
- [x] Remove the `KNOWN_ISSUES` entry.
- [x] Run `make test-ms` to confirm.

---

#### Gap 8 — SDK 12 harness bump (medium) ✅

**Samples (18):** `sample_agentic_retrieval.py`, `sample_knowledge_service_stats_preview.py`, `sample_index_alias_crud.py`, `sample_index_client_custom_request.py`, `sample_index_crud.py`, `sample_query_semantic.py`, `sample_query_vector.py`, `sample_search_client_custom_request.py`, `sample_knowledge_base_configuration_preview.py`, `sample_knowledge_base_crud.py`, `sample_knowledge_retrieval_response_preview.py`, `sample_knowledge_source_crud.py`, `sample_knowledge_source_fabric_data_agent_preview.py`, `sample_knowledge_source_fabric_ontology_preview.py`, `sample_knowledge_source_file_preview.py`, `sample_knowledge_source_freshness_preview.py`, `sample_knowledge_source_mcp_server_preview.py`, `sample_knowledge_source_workiq_preview.py`
**Route:** N/A (harness dependency)
**Was:** samples failed at import / SDK internals under the pinned 11.6.0; classified as `skip`.

**What was done:**

- [x] Bumped `azure-search-documents==11.6.0` to `==12.0.0` in
      `source/tests/python/pyproject.toml`, regenerated `poetry.lock`
      (drops `azure-common`; `azure-core>=1.37`, `isodate>=0.6.1`), rebuilt
      the venv (`requires-python >=3.12` unaffected; SDK 12 needs `>=3.9`).
- [x] Re-validated the wire contract against 12.0.0 **before** triaging.
      Actual 11.6.0 → 12.0.0 deltas (correcting the pre-bump predictions):
      the SDK default `api-version` is now `2026-04-01` (both `SearchClient`
      and `SearchIndexClient`); the probe container now accepts
      `2024-07-01,2026-04-01`. `SearchFieldDataType` gained uppercase members
      (`.STRING`, …) but keeps the old lowercase names as aliases, so the
      harness seed files are unchanged. `SearchSuggester` is still the model
      name (the predicted `SearchSuggester` → `Suggester` rename did not
      happen) and the wire key is still `sourceFields`; the emulator accepts
      both `sourceFields` / `searchFields` as before. `SearchAlias` and the
      `knowledgebases` module now exist; `DEFAULT_VERSION` is exported from
      `azure.search.documents` (not from `indexes`). The plain-HTTP shims
      were updated: 12.0.0 removed `indexes._search_index_client`
      (`normalize_endpoint` is gone — the endpoint URL is used verbatim, so
      `http://` works for api-key auth with no shim) and `_enforce_https`
      remains bearer-token-only, so the shim now patches just that. All 10
      previously-passing samples still pass — no regressions.
- [x] Triaged the newly-runnable samples: 3 genuine passes
      (`sample_query_vector.py` — after the OData lambda-filter fix below —
      plus both `*_custom_request.py` samples), 4 new emulator gaps (Gaps
      11–13), 11 new skips (preview-SDK models, one SDK transport bug, and
      external-data dependencies). `sample_query_semantic.py` did not reach
      the emulator's `400 UnsupportedQuery` semantic path as predicted: SDK
      12.0.0 leaks `query_language`/`query_speller` into the HTTP transport
      (`TypeError`) on the speller call, so it is a `skip` (SDK bug; semantic
      search remains out of scope).
- [x] Fixed one genuine divergence found during triage so the vector sample
      passes: the filter parser now accepts the standard OData lambda form
      `field/any(var: body)` / `field/all(var: body)` (e.g.
      `Tags/any(tag: tag eq 'free wifi')`) alongside the existing
      space-separated form. Tokenizer gains a `:` token; unit tests in
      `source/rust/src/filter/mod.rs` plus a contract test
      (`filter_odata_lambda_any_all`) cover both forms and the malformed
      cases. Documented in `supported_operations.md` / `known_differences.md`.
- [x] Updated `docs/supported_operations.md` (SDK version under test, filter
      syntax) and this document (state table, tracker, wire notes).
- [x] `make test-ms` confirms the new state: 13 genuine passes + 4 gap pins
      (17 passed), 15 skipped, 0 failed.

---

#### Gap 9 — Indexers + data sources (large)

**Samples (3):** `sample_indexer_crud.py`, `sample_indexer_datasource_crud.py`, `sample_indexer_workflow.py`
**Routes:** indexer, data-source (and skillset) CRUD + run/status — none registered
**Current failure:** samples require `AZURE_STORAGE_CONNECTION_STRING`; classified as `skip`.

**What is required:**

- [ ] Provision a storage backend for runs: Azurite (local storage emulator)
      or a real Azure Storage account exposed to the probe as
      `AZURE_STORAGE_CONNECTION_STRING`.
- [ ] Add indexer resource routes (create/get/list/delete, run, status) to
      the azure sub-router.
- [ ] Add data-source resource routes (Azure Blob/ADLS shapes the samples
      use).
- [ ] Add skillset routes if the workflow sample needs them, or classify that
      sample as permanently skipped with the reason.
- [ ] Implement the execution engine: pull blobs from the storage backend,
      parse documents, index into the target index (reuse the existing
      document pipeline).
- [ ] Add contract tests (CRUD, validation, run lifecycle) and document the
      surface in `docs/supported_operations.md` / `docs/known_differences.md`.
- [ ] Remove the `KNOWN_ISSUES` entries.
- [ ] Run `make test-ms` to confirm.

**Notes:** Indexers were explicitly out of scope in the initial design
(`known_differences.md`: "No indexers, data sources, skillsets"). This gap
is a feature project, not a route fix; keep the out-of-scope notice until it
lands.

---

#### Gap 10 — AAD bearer auth (small-medium)

**Sample:** `sample_authentication.py` (AAD half only; the API-key half passes via `GET /docs/$count`)
**Route:** N/A (auth mechanism)
**Current failure:** needs `azure-identity` and a real Azure AD environment; classified as `skip`.

**What is required:**

- [ ] Add `azure-identity` to the harness dependencies.
- [ ] Provision Entra: app registration, Search role assignments (Data Reader
      suffices for the sample's AAD half), tenant/client IDs as probe env.
- [ ] Accept `Authorization: Bearer` in `azure_guard`
      (`source/rust/src/api/mod.rs`) — at minimum permissively (any
      non-empty bearer, mirroring the `api-key` compatibility mechanism);
      document the choice in `docs/known_differences.md` (authentication is a
      compatibility mechanism, not a security boundary).
- [ ] Add contract tests (bearer accepted, missing/empty auth still 401).
- [ ] Remove the `KNOWN_ISSUES` entry (or narrow it if only part lands).
- [ ] Run `make test-ms` to confirm.

---

#### Gap 11 — Index aliases (medium/medium-plus) ✅

**Samples (1):** `sample_index_alias_crud.py`
**Routes:** alias CRUD — none registered (`POST /aliases`, `GET /aliases`, `GET /aliases('{name}')`, `PUT /aliases('{name}')`, `DELETE /aliases('{name}')`)
**Was:** `create_alias` fails with `Operation returned an invalid status 'Method Not Allowed'`.

**What was done:**

- [x] Added a reusable `NamedResource` store (`name`, `etag`, raw body) in
      `source/rust/src/service/mod.rs` with service-level storage (mirroring
      the synonym-map pattern) and `reset()` coverage. Aliases use this store.
- [x] Added the five alias routes to the azure sub-router, mirroring the
      synonym-map shapes: create returns `201`, duplicate create returns
      `409 AliasAlreadyExists`, missing alias returns `404`, delete returns
      `204`; validated (missing/empty name, empty `indexes`, path/body name
      mismatch) with `400 InvalidAlias`. Referenced-index existence is not
      checked (aliases are stored opaquely).
- [x] Extended `azure_guard` to cover `/aliases` and `/aliases(` paths.
- [x] Alias resolution: aliases are CRUD-only; search/document routes do not
      resolve an alias name to its index. Documented in `known_differences.md`.
- [x] Added contract tests in `source/rust/tests/contract/aliases_knowledge.rs`
      (CRUD, validation, auth, reset) and documented the surface in
      `docs/supported_operations.md` / `docs/known_differences.md`.
- [x] Removed the `KNOWN_ISSUES` entry.
- [x] `make test-ms` confirms the sample passes.

---

#### Gap 12 — Collection-of-complex field types (medium/medium-plus) ✅

**Samples (1):** `sample_index_crud.py`
**Route:** N/A (schema validation)
**Was:** `create_index` with a `ComplexField(..., collection=True)` fails with `400 InvalidIndex`: `Unsupported field type "Edm.Collection(Edm.ComplexType)"`.

**What was done:**

- [x] Accept `Edm.Collection(Edm.ComplexType)` in `validate_schema`
      (`source/rust/src/service/mod.rs`): same rules as `Edm.ComplexType`
      (cannot be key/searchable/sortable/facetable; non-empty `fields` array
      of scalar/collection-of-scalar subfields via shared `is_complex_type`
      helper and `validate_subfields`).
- [x] Accept arrays of objects in document validation (`check_complex_collection_value`:
      per-element subfield checking via `check_complex_value`) and in indexing
      (full-text indexing of searchable string subfields across all elements
      via `collect_searchable` recursion and `resolve_doc_paths` array expansion
      in `source/rust/src/query/mod.rs`).
- [x] Filter semantics: collection-of-complex subfields filter via the same
      `Address/State` path syntax as single complex types (lambda `any(...)`
      shapes are not specially handled); documented in `known_differences.md`.
- [x] Echo the type faithfully in `GET /indexes('{name}')` (raw definition
      round-trip) so SDK `create` → `get` agree.
- [x] Added contract tests in `source/rust/tests/contract/complex_fields.rs`
      (schema accept/echo, document validation, search across elements) and
      updated `docs/supported_operations.md` (supported field types).
- [x] Removed the `KNOWN_ISSUES` entry. Note: `sample_index_crud.py` itself is
      now `skip` — the collection-of-complex create/get steps pass, but the
      sample's `list_index_names` step imports preview-SDK `ListingSearchType`
      absent from pinned 12.0.0.
- [x] `make test-ms` confirms the new state (the sample is skipped for the
      preview-SDK reason, not the emulator gap).

**Notes:** `sample_index_crud.py` also exercises `CorsOptions` and (empty) `scoringProfiles` on the index definition; those were already accepted, so no extra work was needed there.

---

#### Gap 13 — Knowledge sources/bases + agentic retrieval (large) ✅

**Samples (2):** `sample_knowledge_source_crud.py`, `sample_agentic_retrieval.py`
**Routes:** knowledge-source, knowledge-base, and retrieval routes — none registered (`/knowledgesources…`, `/knowledgebases…`, agentic retrieval)
**Was:** `create_or_update_knowledge_source` fails with `400 InvalidIndexName`: `Invalid index path segment "knowledgesources('…')"; expected indexes('name')`.

**What was done:**

- [x] Reused the `NamedResource` store for knowledge sources and knowledge
      bases (raw body preserved and echoed with `@odata.etag`) with
      service-level storage and `reset()` coverage, mirroring the synonym-map
      pattern.
- [x] Added the CRUD routes to the azure sub-router (`POST/GET
      /knowledgesources`, `/knowledgebases` literal forms plus `GET/PUT/DELETE
      /knowledgesources('name')`, `/knowledgebases('name')` via the shared
      single-segment dispatch); extended `azure_guard` to cover them.
      Validation: non-empty name, path/body match; sources require a `kind`
      (searchIndex sources require `searchIndexParameters.searchIndexName`);
      bases require a non-empty `knowledgeSources` array.
- [x] Implemented `POST /knowledgebases('{name}')/retrieve` to return an empty
      retrieval response (`{"response": [], "activity": [], "references": []}`)
      when the base exists (`404` otherwise). Model inference is an
      initial-design non-goal; documented in `known_differences.md`.
- [x] Added contract tests in `source/rust/tests/contract/aliases_knowledge.rs`
      (CRUD, validation, auth, reset, retrieval empty/404) and documented the
      surface in `docs/supported_operations.md` / `docs/known_differences.md`.
- [x] Removed the `KNOWN_ISSUES` entries. Note: `sample_knowledge_source_crud.py`
      is now `skip` — its create/get/list/delete steps pass, but the update step
      imports preview-SDK `SearchIndexKnowledgeSourceFilterHint`/`QueryHints`
      absent from pinned 12.0.0. `sample_agentic_retrieval.py` passes end-to-end.
- [x] `make test-ms` confirms the new state.

**Notes:** The remaining knowledge samples stay `skip`: the configuration/retrieval-response previews and the Fabric/ontology/file/MCP/WorkIQ source samples import preview-SDK models absent from pinned 12.0.0 (`KnowledgeBaseRetrieveDefaults`, `KnowledgeBaseResponseCompletedEvent`, `FabricDataAgentKnowledgeSource`, `FabricOntologyKnowledgeSource`, `FileUploadMetadata`, `McpServerAutoOutputParsing`, `EntraAppAuthentication`), and the Fabric/ontology/file/freshness/MCP/WorkIQ variants point at live external data in any case. `sample_index_crud.py` and `sample_knowledge_source_crud.py` join the preview-SDK skips (`ListingSearchType` and `FilterHint`/`QueryHints` respectively).

---

#### Cross-cutting changes (apply as each gap lands)

- [x] `azure_guard` middleware (`source/rust/src/api/mod.rs`): extended to
      cover `/servicestats` (done with Gap 7) and `/synonymmaps` /
      `/synonymmaps(` (done with Gap 3).
- [x] `docs/supported_operations.md`: updated for `$count`, `/search.analyze`,
      `/servicestats`, `sessionId`, synonym maps (done with Gaps 1, 2, 3,
      6, 7), and autocomplete/suggest (done with Gaps 4, 5).
- [x] `docs/known_differences.md`: documented `sessionId` as inert,
      `/search.analyze` default-analyzer simplification (done with Gaps 2, 6),
      inert synonym maps (done with Gap 3), and prefix-match
      autocomplete/suggest (done with Gaps 4, 5).
- [x] `source/tests/python/tests/ms_samples/test_ms_samples.py`: removed
      `KNOWN_ISSUES` entries for Gaps 1, 2, 3, 4, 5, 6, 7.
- [x] `source/tests/python/ms_samples/setup_hotels.py`: added suggester
      definition (done with Gaps 4 and 5).
- [x] Harness SDK bump (done with Gap 8): `pyproject.toml` pins
      `azure-search-documents==12.0.0`, `poetry.lock` regenerated, venv
      rebuilt; probe `API_VERSIONS` is now `2024-07-01,2026-04-01`; the
      plain-HTTP shims (`source/tests/python/conftest.py`,
      `ms_samples/shims/sitecustomize.py`) were updated for the 12.0.0 module
      layout (`normalize_endpoint` gone; `_enforce_https` bearer-only).
- [x] OData lambda filters (done with Gap 8): `field/any(var: body)` /
      `field/all(var: body)` parse, validate, and evaluate; unit tests in
      `source/rust/src/filter/mod.rs`, contract test
      `filter_odata_lambda_any_all`, documented in `supported_operations.md` /
      `known_differences.md`.
- [x] `source/tests/python/tests/ms_samples/test_ms_samples.py`: retriaged
      the 18 Gap-8 samples — removed 3 entries that now pass
      (`sample_query_vector.py`, both `*_custom_request.py`), added 4 `gap`
      pins (Gaps 11–13), rewrote 11 entries as precise `skip` reasons.
- [x] `azure_guard` middleware: extended to cover `/aliases` / `/aliases(`,
      `/knowledgesources` / `/knowledgesources(`, `/knowledgebases` /
      `/knowledgebases(` (done with Gaps 11, 13).
- [x] `docs/supported_operations.md`: updated for aliases, collection-of-complex
      field types, knowledge sources/bases, and agentic retrieval (done with
      Gaps 11, 12, 13).
- [x] `docs/known_differences.md`: documented CRUD-only aliases (no resolution),
      collection-of-complex filter semantics, and empty retrieval responses
      (done with Gaps 11, 12, 13).
- [x] `source/tests/python/tests/ms_samples/test_ms_samples.py`: removed the
      4 `gap` pins (Gaps 11–13); `sample_index_alias_crud.py` and
      `sample_agentic_retrieval.py` now pass; `sample_index_crud.py` and
      `sample_knowledge_source_crud.py` reclassified as preview-SDK `skip`
      (`ListingSearchType` and `FilterHint`/`QueryHints` absent from 12.0.0).

### Skipped — needs a newer/preview SDK than the pinned `12.0.0` (13)

Upstream `main` already targets models that post-date stable `12.0.0` (latest
on PyPI and the version the emulator is validated against — see
`supported_operations.md`). These samples fail on import, model construction,
or SDK internals before emulator behaviour is reached:

- `sample_knowledge_service_stats_preview.py` — the sample reads
  `stats.counters.knowledge_base_counter.usage`, but 12.0.0's
  `SearchServiceCounters` has no `knowledge_base_counter` (preview feature).
- `sample_query_semantic.py` — SDK 12.0.0 leaks `query_language` /
  `query_speller` into the HTTP transport (`TypeError:
  Session.request() got an unexpected keyword argument 'query_language'`) on
  the speller call, before any request reaches the emulator. Semantic search
  itself is out of scope (see `known_differences.md`).
- `sample_knowledge_base_configuration_preview.py`
  (`KnowledgeBaseRetrieveDefaults`), `sample_knowledge_retrieval_response_preview.py`
  (`KnowledgeBaseResponseCompletedEvent`), `sample_knowledge_source_fabric_data_agent_preview.py`
  (`FabricDataAgentKnowledgeSource`), `sample_knowledge_source_fabric_ontology_preview.py`
  (`FabricOntologyKnowledgeSource`), `sample_knowledge_source_file_preview.py`
  (`FileUploadMetadata`), `sample_knowledge_source_mcp_server_preview.py`
  (`McpServerAutoOutputParsing`), `sample_knowledge_source_workiq_preview.py`
  (`EntraAppAuthentication`): each imports a model absent from 12.0.0.
- `sample_knowledge_base_crud.py` — `KnowledgeBase(...)` rejects the
  sample's `tags` kwarg in 12.0.0 (`TypeError` at construction).
- `sample_index_crud.py` — the emulator implements collection-of-complex (the
  sample's create/get steps pass), but its `list_index_names` step imports
  preview-SDK `ListingSearchType` absent from 12.0.0.
- `sample_knowledge_source_crud.py` — the emulator implements knowledge-source
  CRUD (the sample's create/get/list/delete steps pass), but its update step
  imports preview-SDK `SearchIndexKnowledgeSourceFilterHint` /
  `SearchIndexKnowledgeSourceQueryHints` (and `SearchIndexFieldReference`)
  absent from 12.0.0.
- `sample_knowledge_source_freshness_preview.py` — points at live external
  data for its freshness policy, so it stays `skip`.

The Fabric/ontology/file/MCP/WorkIQ variants additionally point at live
external data and are permanently unrunnable here regardless of SDK version.

### Skipped — external service (4)

`sample_indexer_crud.py`, `sample_indexer_datasource_crud.py`,
`sample_indexer_workflow.py` require `AZURE_STORAGE_CONNECTION_STRING`
(tracked as [Gap 9](#gap-9--indexers--data-sources-large)).
`sample_authentication.py` requires `azure-identity` and a real Azure AD
environment for its AAD half (the API-key half, which exercises
`GET /docs/$count`, passes; tracked as
[Gap 10](#gap-10--aad-bearer-auth-small-medium)).

## Updating the submodule

The submodule pointer pins the tested upstream commit (currently `67b56be`).
`make ms-samples` always checks out that pinned commit, so CI and fresh
clones are reproducible. To test against newer samples:

```sh
make ms-samples-update   # fetch latest upstream main, check it out (detached),
                         # re-apply the sparse-checkout
make test-ms             # triage the result:
                         # - new sample fails ("new upstream sample? triage it")
                         #   -> add it to KNOWN_ISSUES in test_ms_samples.py
                         # - a pinned gap now passes ("gap closed?")
                         #   -> remove it from KNOWN_ISSUES
git add source/tests/python/ms_samples/upstream   # record the new pointer
```

Then update this document: the state table above, the pinned commit SHA in
"How it works", and `status_last_reviewed`.

To pin a specific commit instead of latest `main` (e.g. to bisect a new
upstream failure):

```sh
git -C source/tests/python/ms_samples/upstream fetch origin <sha> --depth 1 --filter=blob:none
git -C source/tests/python/ms_samples/upstream checkout <sha>
git -C source/tests/python/ms_samples/upstream sparse-checkout set sdk/search/azure-search-documents/samples
```

Notes:

- The submodule is shallow (`depth 1`, `shallow = true` in `.gitmodules`),
  so `git log` inside it shows only the checked-out commit. Deepen with
  `git -C <path> fetch --deepen=<n>` if you need history.
- `git submodule update --init` alone is not enough on a fresh clone: it
  restores the working tree but not the sparse-checkout pattern. Always go
  through `make ms-samples`, which re-applies it.

## Relationship to other docs

- `supported_operations.md` remains the contract: it defines what the
  emulator implements. This probe checks that contract against Microsoft's
  own usage of the SDK.
- Gaps confirmed here that are accepted long-term differences belong in
  `known_differences.md`; gaps that are planned work belong in the
  `phase_2*.md` / `phase_3*.md` plans. When a gap is closed, remove its
  `KNOWN_ISSUES` entry (the suite forces this by going red) and update those
  docs.
