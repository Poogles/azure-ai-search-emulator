---
status: complete
status_last_reviewed: 2026-09-09
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
    (`2025-09-01` for the pinned SDK). The probe container accepts
    `2024-07-01,2025-09-01`.
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

32 sync samples discovered. `make test-ms`: **10 passed, 22 skipped, 0 failed.**
(All 10 passes are genuine; the 2 former gap pins now pass for real: 10
genuine passes + 0 gap pins.)

### Passes (10)

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

### Documented emulator gaps (0)

All error-signature gaps are closed. Historical plans for each gap are in [Gap implementation plans](#gap-implementation-plans) below. The remaining 22 skips are tracked as Gaps 8–10 (same format, `☐ not started`).

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
| 8 | SDK 12 harness bump (18 samples) | ☐ not started | |
| 9 | Indexers + data sources (3 samples) | ☐ not started | |
| 10 | AAD bearer auth (1 sample half) | ☐ not started | |

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

#### Gap 8 — SDK 12 harness bump (medium)

**Samples (18):** `sample_agentic_retrieval.py`, `sample_knowledge_service_stats_preview.py`, `sample_index_alias_crud.py`, `sample_index_client_custom_request.py`, `sample_index_crud.py`, `sample_query_semantic.py`, `sample_query_vector.py`, `sample_search_client_custom_request.py`, `sample_knowledge_base_configuration_preview.py`, `sample_knowledge_base_crud.py`, `sample_knowledge_retrieval_response_preview.py`, `sample_knowledge_source_crud.py`, `sample_knowledge_source_fabric_data_agent_preview.py`, `sample_knowledge_source_fabric_ontology_preview.py`, `sample_knowledge_source_file_preview.py`, `sample_knowledge_source_freshness_preview.py`, `sample_knowledge_source_mcp_server_preview.py`, `sample_knowledge_source_workiq_preview.py`
**Route:** N/A (harness dependency)
**Current failure:** samples fail at import / SDK internals (`SearchAlias`, `DEFAULT_VERSION`, `SearchFieldDataType.STRING`, semantic query kwargs, typed service-stats model, `knowledgebases` module) before any HTTP reaches the emulator; classified as `skip`.

**What is required:**

- [ ] Bump `azure-search-documents==11.6.0` to 12.x in
      `source/tests/python/pyproject.toml`, regenerate the lockfile, rebuild
      the venv (check `requires-python` compatibility: harness is `>=3.12`).
- [ ] Re-validate the emulator's wire contract against 12.x **before**
      triaging: 12 renames models and enums (precedent: `SearchSuggester` →
      `Suggester`, `sourceFields` → `searchFields` between 11.6.0 and the
      REST docs). Expect the currently-passing 10 to wobble; fix regressions
      first (new `KNOWN_ISSUES` `gap` pins only for genuine new divergences).
- [ ] Triage the newly-runnable samples into new emulator gaps vs new
      external-service skips. Expected new gaps: index aliases, semantic
      query, vector query (see `phase_2_1_vector_indexing.md`), knowledge
      base/source CRUD, agentic retrieval. Expected permanently unrunnable
      here: the Fabric/ontology/file/freshness/MCP/WorkIQ knowledge-source
      samples, which point at live external data.
- [ ] Update `docs/supported_operations.md` (SDK version under test) and this
      document (state table, pinned wire notes).
- [ ] Run `make test-ms` to confirm.

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

### Skipped — needs `azure-search-documents >= 12.0.0` (18)

Upstream `main` targets SDK 12 (`12.0.0` is latest on PyPI); the harness pins
`11.6.0`, the version the emulator is validated against (see
`supported_operations.md`). These samples fail on import or SDK internals
before emulator behaviour is reached:

- `sample_agentic_retrieval.py` (`knowledgebases` module), `sample_index_alias_crud.py` (`SearchAlias`), `sample_index_client_custom_request.py` / `sample_search_client_custom_request.py` (`DEFAULT_VERSION` export), `sample_index_crud.py` / `sample_query_vector.py` (`SearchFieldDataType.STRING`), `sample_query_semantic.py` (semantic query kwargs),
- `sample_knowledge_service_stats_preview.py` (typed service-stats model; SDK 11 returns a dict),
- all knowledge-base/source previews: `sample_knowledge_base_configuration_preview.py`, `sample_knowledge_base_crud.py`, `sample_knowledge_retrieval_response_preview.py`, `sample_knowledge_source_crud.py`, `sample_knowledge_source_fabric_data_agent_preview.py`, `sample_knowledge_source_fabric_ontology_preview.py`, `sample_knowledge_source_file_preview.py`, `sample_knowledge_source_freshness_preview.py`, `sample_knowledge_source_mcp_server_preview.py`, `sample_knowledge_source_workiq_preview.py`.

Bumping the harness SDK would unblock these and give a truer gap list, but the
emulator's wire format would need re-validation against 12.x first. Tracked
as [Gap 8](#gap-8--sdk-12-harness-bump-medium).

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
