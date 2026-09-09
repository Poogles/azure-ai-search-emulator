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

32 sync samples discovered. `make test-ms`: **12 passed, 20 skipped, 0 failed.**

### Passes (5)

| Sample | Notes |
|--------|-------|
| `sample_query_simple.py` | Genuine pass: simple text search returns the seeded hotel. |
| `sample_documents_buffered_sender.py` | Genuine pass since the GeographyPoint fix: the `HotelId: 100` document is stored (previously a `falsepass` — the emulator rejected the SDK's `{"type": "Point", "coordinates": [...]}` shape while `SearchIndexingBufferedSender` swallowed the per-document error). |
| `sample_documents_crud.py` | Genuine pass since the GeographyPoint fix (upload/merge of doc 100) plus the new `GET /indexes('{name}')/docs('{key}')` endpoint (get/delete of doc 100). |
| `sample_query_facets.py` | Genuine pass since the facet-options fix (`Category,count:3`). |
| `sample_query_filter.py` | Genuine pass since the complex-type fix (`Address/StateProvince` filter against the `Address` complex field in the seeded hotels index). |

### Documented emulator gaps (7)

| Sample | Fails with | Missing feature |
|--------|-----------|-----------------|
| `sample_authentication.py` | 404 `Not Found` | `GET /docs/$count` (`get_document_count`) not implemented. (AAD halves also need `azure-identity`.) |
| `sample_index_analyze_text.py` | 404 `Not Found` | `POST /indexes('{name}')/analyze` not implemented. |
| `sample_index_synonym_map_crud.py` | 405 `Method Not Allowed` | Synonym-map routes not implemented. |
| `sample_query_autocomplete.py` | 404 `Not Found` | Autocomplete route not implemented. |
| `sample_query_suggestions.py` | 404 `Not Found` | Suggest route not implemented. |
| `sample_query_session.py` | 400 `UnsupportedQuery` | `sessionId` rejected as an unsupported query option. |
| `sample_knowledge_service_stats_preview.py` | 400 `InvalidIndexName` | Service-stats route not implemented; the request falls through to index-path parsing. |

Implementation plans for each gap are in [Gap implementation plans](#gap-implementation-plans) below.

### Gap implementation plans

Detailed requirements for closing each gap, in recommended implementation
order (each builds on prior auth-guard / route changes).

#### Completion tracker

| # | Gap | Status | PR / commit |
|---|-----|--------|-------------|
| 6 | `sessionId` | ☐ not started | |
| 1 | Document count (`$count`) | ☐ not started | |
| 7 | Service stats (`/servicestats`) | ☐ not started | |
| 2 | Analyze text (`/search.analyze`) | ☐ not started | |
| 3 | Synonym maps CRUD | ☐ not started | |
| 4 | Autocomplete | ☐ not started | |
| 5 | Suggest | ☐ not started | |

---

#### Gap 6 — `sessionId` (trivial, ~5 min)

**Sample:** `sample_query_session.py`
**Route:** N/A (field in search POST body)
**Current failure:** 400 `UnsupportedQuery`

**What is required:**

- [ ] Remove `"sessionId"` from `UNSUPPORTED_SEARCH_OPTIONS` in
      `source/src/service/mod.rs` (~line 1418). The field will be silently
      ignored (the emulator uses deterministic ordering and constant scoring,
      so session affinity is irrelevant).
- [ ] Update `docs/supported_operations.md`: move `sessionId` from the
      rejected-options list to "accepted but inert".
- [ ] Remove the `KNOWN_ISSUES` entry in
      `source/tests/python/tests/ms_samples/test_ms_samples.py`.
- [ ] Run `make test-ms` to confirm the sample now passes.

---

#### Gap 1 — Document count `GET /indexes('{name}')/docs/$count` (easy, ~30 min)

**Sample:** `sample_authentication.py`
**Route:** `GET /indexes('{indexName}')/docs/$count`
**Current failure:** 404 `Not Found`

**What is required:**

- [ ] Add route `GET /{index}/docs/$count` to the azure sub-router in
      `source/src/api/mod.rs`. The literal three-segment path takes precedence
      over the existing `/{index}/{key}` catch-all.
- [ ] Add a handler that parses the index name, calls
      `storage.get_documents(index)`, and returns the `.len()` as a bare
      integer body (not JSON-wrapped) with `Content-Type: application/json`.
- [ ] Add a contract test in `source/tests/python/tests/` exercising
      `search_client.get_document_count()`.
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

**Notes:** The AAD halves of `sample_authentication.py` also require
`azure-identity` and a real token; those remain a `skip` even after this gap
is closed. The sample will pass if the `get_document_count` call succeeds
before the AAD section.

---

#### Gap 7 — Service stats `GET /servicestats` (easy, ~30 min)

**Sample:** `sample_knowledge_service_stats_preview.py`
**Route:** `GET /servicestats`
**Current failure:** 400 `InvalidIndexName` (falls through to index-path
parsing)

**What is required:**

- [ ] Add route `GET /servicestats` to the main router in
      `source/src/api/mod.rs` (literal path, takes precedence over
      `/{index}`).
- [ ] Handler returns a static JSON response:
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
- [ ] Extend `azure_guard` middleware to cover the `/servicestats` path
      (currently only covers `/indexes` and `/indexes(`).
- [ ] Add a contract test.
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

---

#### Gap 2 — Analyze text `POST /indexes('{name}')/search.analyze` (easy-medium, ~1 hr)

**Sample:** `sample_index_analyze_text.py`
**Route:** `POST /indexes('{indexName}')/search.analyze`
**Current failure:** 404 `Not Found`

**What is required:**

- [ ] Extend `analyze()` in `source/src/query/mod.rs` (~line 70) to return
      structured tokens with offsets. Tantivy's `TokenStream` already provides
      `token.text`, `token.offset_from`, `token.offset_to`, and
      `token.position`. Define:
      ```rust
      struct AnalyzeToken {
          token: String,
          start_offset: usize,
          end_offset: usize,
          position: usize,
      }
      ```
- [ ] Add route `POST /{index}/search.analyze` to the azure sub-router.
      Axum gives precedence to literal routes over the `/{index}/{key}`
      parameterized route.
- [ ] Handler: parse the index name, parse the JSON body (extract `text`;
      accept but ignore `analyzerName` and `field`), call the extended
      analyze function, return:
      ```json
      {"tokens": [{"token": "...", "startOffset": 0, "endOffset": 4, "position": 0}]}
      ```
- [ ] Add a contract test.
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

**Notes:** The sample uses `analyzer_name="standard.lucene"`. The emulator
always uses Tantivy's default English analyzer; this is an acceptable
simplification (document in `known_differences.md`).

---

#### Gap 3 — Synonym maps CRUD (medium, ~2-3 hrs)

**Sample:** `sample_index_synonym_map_crud.py`
**Routes:**
- `POST /synonymmaps` (create)
- `GET /synonymmaps` (list)
- `GET /synonymmaps('{name}')` (get by name)
- `PUT /synonymmaps('{name}')` (create or update)
- `DELETE /synonymmaps('{name}')` (delete)

**Current failure:** 405 `Method Not Allowed`

**What is required:**

- [ ] Define a `SynonymMap` struct: `name: String`, `format: String`
      (always `"solr"`), `synonyms: Vec<String>`, `etag: String`
      (generated UUID or incrementing counter).
- [ ] Add storage: a `BTreeMap<String, SynonymMap>` in `SearchService` (or a
      dedicated `SynonymMapStore`).
- [ ] Add routes. The OData-style `synonymmaps('name')` is a single path
      segment, so it conflicts with the existing `/{index}` route. Options:
      - Register `POST /synonymmaps` and `GET /synonymmaps` as literal routes
        (no conflict).
      - For `GET/PUT/DELETE /{segment}`: dispatch in the existing `/{index}`
        handler — if the raw segment starts with `synonymmaps(`, route to
        synonym-map logic; otherwise treat as an index name.
- [ ] Extend `azure_guard` to cover `/synonymmaps` and `/synonymmaps(` paths.
- [ ] Response format:
      ```json
      {"name": "...", "format": "solr", "synonyms": ["a, b, c"], "@odata.etag": "\"...\""}
      ```
      List response: `{"value": [...]}`.
- [ ] Add contract tests (create, list, get, update, delete).
- [ ] Document in `known_differences.md`: synonym maps are stored but inert
      (do not affect search results).
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

---

#### Gap 4 — Autocomplete `POST /indexes('{name}')/docs/search.post.autocomplete` (medium-hard, ~3-4 hrs)

**Sample:** `sample_query_autocomplete.py`
**Route:** `POST /indexes('{indexName}')/docs/search.post.autocomplete?suggesterName=sg`
**Current failure:** 404 `Not Found`

**What is required:**

- [ ] **Suggester schema:** Add a `Suggester` struct (`name: String`,
      `search_fields: Vec<String>`) and a `suggesters: Vec<Suggester>` field
      to `IndexDefinition`.
- [ ] Parse the `suggesters` array in `parse_index_definition`.
- [ ] Validate in `validate_schema` that suggester search fields exist and
      are `searchable`.
- [ ] Update `setup_hotels.py` to include a suggester:
      ```python
      from azure.search.documents.indexes.models import Suggester
      # In the SearchIndex definition:
      suggesters=[Suggester(name="sg", search_fields=["HotelName"])]
      ```
- [ ] Add route `POST /{index}/docs/search.post.autocomplete` to the azure
      sub-router.
- [ ] Handler: parse index name, extract `suggesterName` from query params,
      parse body (extract `searchText`), look up the suggester in the index
      definition, perform case-insensitive prefix matching on the suggester's
      search fields across all documents. Return:
      ```json
      {"value": [{"text": "Boston", "queryPlusText": "bo Boston"}]}
      ```
      Limit to `top` results (default 5, from query param or body).
- [ ] Add contract tests.
- [ ] Document in `known_differences.md`: autocomplete uses simple prefix
      matching (not Azure's full suggester algorithm with scoring).
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

---

#### Gap 5 — Suggest `POST /indexes('{name}')/docs/search.post.suggest` (medium-hard, ~2-3 hrs)

**Sample:** `sample_query_suggestions.py`
**Route:** `POST /indexes('{indexName}')/docs/search.post.suggest?suggesterName=sg`
**Current failure:** 404 `Not Found`

**What is required:**

- [ ] Reuse the suggester infrastructure from Gap 4 (schema, parsing,
      validation, `setup_hotels.py`).
- [ ] Add route `POST /{index}/docs/search.post.suggest` to the azure
      sub-router.
- [ ] Handler: same pattern as autocomplete, but return full documents (all
      retrievable fields) with an additional `@search.text` field containing
      the matched text:
      ```json
      {"value": [{"@search.text": "coffee", "HotelId": "1", "HotelName": "...", ...}]}
      ```
      The sample calls `get_document(key=result["HotelId"])` for each
      suggestion, so the key field must be present.
- [ ] Add contract tests.
- [ ] Remove the `KNOWN_ISSUES` entry.
- [ ] Run `make test-ms` to confirm.

---

#### Cross-cutting changes (apply as each gap lands)

- [ ] `azure_guard` middleware (`source/src/api/mod.rs`): extend to cover
      `/synonymmaps`, `/synonymmaps(`, and `/servicestats` paths.
- [ ] `docs/supported_operations.md`: update the operations matrix for each
      new endpoint.
- [ ] `docs/known_differences.md`: document simplifications (inert synonym
      maps, prefix-match autocomplete/suggest, default analyzer for
      `/search.analyze`).
- [ ] `source/tests/python/tests/ms_samples/test_ms_samples.py`: remove
      `KNOWN_ISSUES` entries as each gap closes.
- [ ] `source/tests/python/ms_samples/setup_hotels.py`: add suggester
      definition (needed for Gaps 4 and 5).

### Skipped — needs `azure-search-documents >= 12.0.0` (17)

Upstream `main` targets SDK 12 (`12.0.0` is latest on PyPI); the harness pins
`11.6.0`, the version the emulator is validated against (see
`supported_operations.md`). These samples fail on import or SDK internals
before emulator behaviour is reached:

- `sample_agentic_retrieval.py` (`knowledgebases` module), `sample_index_alias_crud.py` (`SearchAlias`), `sample_index_client_custom_request.py` / `sample_search_client_custom_request.py` (`DEFAULT_VERSION` export), `sample_index_crud.py` / `sample_query_vector.py` (`SearchFieldDataType.STRING`), `sample_query_semantic.py` (semantic query kwargs),
- all knowledge-base/source previews: `sample_knowledge_base_configuration_preview.py`, `sample_knowledge_base_crud.py`, `sample_knowledge_retrieval_response_preview.py`, `sample_knowledge_source_crud.py`, `sample_knowledge_source_fabric_data_agent_preview.py`, `sample_knowledge_source_fabric_ontology_preview.py`, `sample_knowledge_source_file_preview.py`, `sample_knowledge_source_freshness_preview.py`, `sample_knowledge_source_mcp_server_preview.py`, `sample_knowledge_source_workiq_preview.py`.

Bumping the harness SDK would unblock these and give a truer gap list, but the
emulator's wire format would need re-validation against 12.x first.

### Skipped — external service (3)

`sample_indexer_crud.py`, `sample_indexer_datasource_crud.py`,
`sample_indexer_workflow.py` require `AZURE_STORAGE_CONNECTION_STRING`.

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
