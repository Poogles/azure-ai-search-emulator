---
status: complete
status_last_reviewed: 2026-09-10
---

# Phase 2.1 — Vector Indexing and Vector Search

## Purpose

Add vector field support and vector similarity search to the emulator, enabling applications that use Azure AI Search vector capabilities (RAG pipelines, embedding-based retrieval, hybrid search) to run against the emulator unmodified. This is an extension of Phase 2's query engine and schema validation, not a new phase in the admin/packaging sequence.

## Motivation

Phase 2 explicitly descoped vector/semantic search (`docs/phase_2_production_api.md` §Out of scope). Applications using Azure AI Search for RAG or embedding-based retrieval cannot run against the emulator. This phase closes that gap for the vector subset, leaving semantic search (Azure AI's hosted model inference) out of scope.

## Scope

### Library selection

**hnsw_rs** (https://crates.io/crates/hnsw_rs) as the embedded vector index, paired with its `anndists` distance-metric crate.

Rationale:

- Pure Rust (no C++ toolchain dependency), consistent with the single-static-binary goal.
- Provides L2 and Cosine first-class. Dot product has no graph-safe distance wrapper (see hnsw_rs notes below) and always executes as an exact brute-force scan.
- HNSW graph for approximate nearest-neighbour; exact results via direct brute-force linear scan (not via HNSW tuning), for `exhaustiveKnn` kind and per-query `exhaustive: true`.
- SIMD via `anndists` is opt-in (`simdeez_f` feature + `RUSTFLAGS=-C target-cpu=native`); not automatic. The exact feature combination is pinned in `docs/decisions/0004-vector-index.md`. Behaviour differs on aarch64 CI (no AVX2); correctness tests must not depend on SIMD being active.
- In-process, no external service — matches the Tantivy integration pattern (see `docs/decisions/0003-search-engine.md`).

Alternative considered: **usearch** (C++ core, thin Rust bindings). Rejected for this project because it introduces a C++ build dependency that complicates the Nix build environment (the same class of problem that excluded Tantivy's `zstd` feature). hnsw_rs is the safer fit for the existing build constraints.

### Index schema: vector field types

Vector fields use the Azure wire format — the dimension is a **separate property**, not part of the type string:

```json
{
  "name": "content_vector",
  "type": "Collection(Edm.Single)",
  "searchable": true,
  "retrievable": true,
  "dimensions": 1536,
  "vectorSearchProfile": "my-vector-profile"
}
```

Rules:

- `type` must be `Collection(Edm.Single)` (both `Collection(Edm.Single)` as sent by the SDK and `Edm.Collection(Edm.Single)` as in REST docs are accepted and normalized, matching the existing `normalize_field_type` behaviour in `source/rust/src/storage/mod.rs`).
- `dimensions` is required on vector fields: a positive integer, 1–3072 (matching Azure's limit; cap overridable via `EMULATOR_VECTOR__MAX_DIMENSION`, see §Configuration). Missing/non-integer/out-of-range → `400 InvalidIndex`.
- `vectorSearchProfile` is required on vector fields and must reference a profile in the index's `vectorSearch.profiles` array. Unknown profile name → `400 InvalidIndex`.
- `searchable` **must be `true`** on vector fields (Azure requirement). `searchable: false` → `400 InvalidIndex`.
- The field must not be `key`, `filterable`, `sortable`, or `facetable` (Azure restriction) → `400 InvalidIndex`.
- `retrievable` may be `true` or `false` (default `true`). `retrievable: false` vectors are still searchable but omitted from responses unless explicitly selected (same as Azure). The SDK's `stored` property is accepted but inert (documented in `known_differences.md`).
- Multiple vector fields per index are supported (up to 16, matching Azure).
- The index definition must include a `vectorSearch` property with at least one algorithm entry **and** at least one profile entry when vector fields are present; its absence is a `400 InvalidIndex`.

#### `vectorSearch` algorithm and profile configuration

```json
{
  "vectorSearch": {
    "algorithms": [
      {
        "name": "hnsw-1",
        "kind": "hnsw",
        "hnswParameters": {
          "m": 4,
          "efConstruction": 400,
          "efSearch": 500,
          "metric": "cosine"
        }
      },
      {
        "name": "eknn-1",
        "kind": "exhaustiveKnn",
        "exhaustiveKnnParameters": { "metric": "cosine" }
      }
    ],
    "profiles": [
      { "name": "my-vector-profile", "algorithmConfigurationName": "hnsw-1" }
    ]
  }
}
```

Supported:

- `kind`: `hnsw` (HNSW graph) and `exhaustiveKnn` (brute-force linear scan; never touches the HNSW graph). A missing `kind` defaults to `hnsw` (emulator-only leniency). There is no `kind: "flat"` in Azure — `flat` from earlier drafts of this doc is replaced by `exhaustiveKnn`.
- Parameters are nested per kind: `hnswParameters: {m, efConstruction, efSearch, metric}`, `exhaustiveKnnParameters: {metric}`. A top-level `parameters` object is accepted as an alias for the kind-specific object (emulator-only leniency); top-level `metric`/`vectorFormat`/`exhaustiveThreshold` (earlier draft shape) are **not** accepted; unknown top-level keys on algorithm entries are ignored for forward-compat, but a missing kind-specific parameters object falls back to defaults (`m: 4`, `efConstruction: 400`, `efSearch: 500`, `metric: cosine`). `m` is validated as 1-256 (the emulator is lenient; Azure restricts it to 4-100).
- `metric`: `cosine`, `dotProduct`, `euclidean` (Azure spelling; also accept `cosineSimilarity`? No — reject unknown metrics with `400 InvalidIndex`).
- `profiles[]`: each entry maps `name` → `algorithmConfigurationName` (an algorithm in the same `vectorSearch.algorithms` array). A profile referencing an unknown algorithm → `400 InvalidIndex`. Duplicate profile or algorithm names → `400 InvalidIndex`.
- Exhaustive search is a **per-query** flag (`vectorQueries[].exhaustive: true`), not a schema-level `exhaustiveThreshold`. There is no `exhaustiveThreshold` property in the `2024-07-01` REST API; this doc does not define one.
- Quantized vector types (`Collection(Edm.Half)`, `Collection(Edm.Int8)`, `Collection(Edm.UInt8)`, etc.) are rejected with `400 InvalidIndex` ("quantized vector types are not supported; only 'Collection(Edm.Single)' is supported"). There is no `vectorFormat: "byte"` property in the index schema.

SDK ↔ REST key mapping (handled in `source/rust/src/storage/mod.rs` for field definitions and `source/rust/src/vector/mod.rs` for the `vectorSearch` config):

| SDK (`azure-search-documents`)                                     | REST / emulator storage                                     |
|:-------------------------------------------------------------------|:------------------------------------------------------------|
| `SearchField(type=Collection(Single), vector_search_dimensions=N)` | `dimensions: N`                                             |
| `SearchField(vector_search_profile_name="p")`                      | `vectorSearchProfile: "p"`                                  |
| `HnswAlgorithmConfiguration(name, kind="hnsw", parameters=...)`    | `vectorSearch.algorithms[]` with `hnswParameters`           |
| `ExhaustiveKnnAlgorithmConfiguration(...)`                         | `vectorSearch.algorithms[]` with `kind: "exhaustiveKnn"`    |
| `VectorSearchProfile(name, algorithm_configuration_name=...)`      | `vectorSearch.profiles[]` with `algorithmConfigurationName` |

### Document validation: vector fields

- A vector field value must be a JSON array of numbers (floats).
- The array length must exactly match the field's declared `dimensions`.
- Values must be finite (no `NaN`, no `Infinity`; JSON has no NaN/Infinity literals, so this covers programmatic `f64::NAN` via SDK serialisation guards and string-coerced values).
- Violations produce a per-document `400` in the batch response (same pattern as existing type validation).
- Vector fields are stored in the `Document.fields` map as `Value::Array(Vec<Value::Number>)` (same as any other field); the vector index is a secondary structure updated alongside storage.

### Vector index lifecycle

A new `VectorIndex` component (in `source/rust/src/vector/`) mirrors the `SearchEngine` pattern:

- One hnsw_rs index per (emulator index, vector field) pair (hnsw_rs indexes have a fixed dimension, so sharing across fields with different dimensions is not possible).
- Created at index creation time (from the schema's vector fields + resolved profile → algorithm config).
- Updated on document upload/merge (upsert by key) and delete.
- Destroyed on index deletion and service reset.
- Guarded by the same `RwLock` pattern as `SearchEngine` (read lock for search, write lock for mutation).
- Immediate consistency: vectors are searchable as soon as the upload batch returns (synchronous insert, matching the full-text engine's commit+reload pattern).

#### hnsw_rs integration notes

- hnsw_rs uses `f32` slices as its point type. Vectors are stored as `Vec<f32>` in the index (converted from JSON numbers; non-finite rejected at validation time).
- The index is keyed by hnsw_rs `DataId`; a side-map (`BTreeMap<DataId, String>`, `DataId` is `u64` in current hnsw_rs — confirm against the pinned version in `0004-vector-index.md`) tracks internal ID → document key for result resolution.
- **Deletes via rebuild.** hnsw_rs has no reliable point-deletion API, so document delete (and vector-field removal on merge) rebuilds the affected per-field index from the surviving stored documents. Acceptable at emulator scale; document the O(n) cost in `0004-vector-index.md`. Correctness test: insert → delete → search must not return the deleted key.
- **Exact path bypasses HNSW.** For `exhaustiveKnn` profiles and per-query `exhaustive: true`, run a direct linear scan over the stored `Vec<f32>` values — do not approximate by setting `efSearch = index size`.
- Dot product: no graph wrapper — `hnsw_rs` asserts non-negative distances and `anndists`'s `DistDot` asserts `dot <= 1`, so raw inner products over unnormalized vectors cannot back a graph. `dotProduct` always scans exactly (see `docs/decisions/0004-vector-index.md`).
- Cosine: first-class in `anndists` via `DistCosine`, which evaluates `1 - dot/(|a||b|)` internally and is therefore well-defined for unnormalized SDK input — no pre-normalization on insert or at search time. Euclidean (L2): first-class in `anndists`, no normalization.
- `m` / `efConstruction` / `efSearch` from `hnswParameters` are passed to hnsw_rs; `efSearch` may additionally be raised per-query to satisfy `k` (never lowered below the configured value silently — document the rule in `0004`).

### Query: `vectorQueries` parameter

The search request body accepts an Azure-shaped `vectorQueries` array plus a **top-level** `filter` and `vectorFilterMode`:

```json
{
  "search": "*",
  "filter": "category eq 'tech'",
  "vectorQueries": [
    {
      "kind": "vector",
      "vector": [0.1, 0.2, 0.3],
      "fields": "content_vector",
      "k": 5,
      "exhaustive": true
    }
  ],
  "vectorFilterMode": "preFilter"
}
```

Field semantics (wire names; SDK names in parentheses):

- `kind`: must be `"vector"`. `kind: "text"` (vectorizer queries) → `400 UnsupportedQuery` (no vectorizer in the emulator). Missing `kind` defaults to `"vector"` for back-compat with early adopters, but this leniency is documented as emulator-only.
- `vector` (SDK: `vector`): array of floats; length must match **every** field listed in `fields`. Mismatch → `400 InvalidQuery`. Non-finite values → `400 InvalidQuery`.
- `fields` (SDK: `fields`, single string): comma-separated field list (`"a,b"`) or JSON array (`["a","b"]`) — both accepted. Every entry must be a vector field in the index schema. Unknown/non-vector field → `400 InvalidQuery`. A single query targeting multiple fields searches each field's per-field index and unions the per-field hits (best score per document wins).
- `k` (SDK: `k_nearest_neighbors`): number of nearest neighbours per query. Positive integer, max 1000. Missing `k` defaults to 3 (matching the SDK default), **not** 1000. Non-positive/non-integer → `400 InvalidQuery`.
- `exhaustive` (SDK: `exhaustive`): optional bool, default `false`. When `true`, forces the brute-force path for that query even if the profile's algorithm is `hnsw`.
- `weight` (SDK: `weight`): optional positive float, default `1.0`. Accepted but **inert** (no weighted fusion in the emulator); documented in `known_differences.md` like `sessionId`.
- There is **no per-query `filters`** property (earlier draft shape with `key`/`filters` is removed). Filtering uses the top-level `filter` expression, parsed by the existing `source/rust/src/filter/` module, applied per `vectorFilterMode`.
- Multiple `vectorQueries` entries are supported (max 5 per search, matching Azure); results are the **union** of all vector query matches, scored by the best (highest) score across queries (and across fields within a query).

SDK ↔ wire mapping for search (the service layer accepts both SDK and REST keys; see `parse_vector_options` in `source/rust/src/service/mod.rs`):

| SDK (`SearchClient.search`)                                                                         | Wire body                                                          |
|:----------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------|
| `vector_queries=[VectorizedQuery(vector=..., fields=..., k_nearest_neighbors=..., exhaustive=...)]` | `vectorQueries: [{kind: "vector", vector, fields, k, exhaustive}]` |
| `vector_filter_mode="preFilter"` / `"postFilter"`                                                   | `vectorFilterMode` (same strings)                                  |
| `filter="category eq 'tech'"`                                                                       | `filter` (top-level; shared by full-text and vector paths)         |

#### `vectorFilterMode`

- `postFilter` (default): retrieve top-k by vector similarity, then apply the top-level `filter` to the retrieved hits.
- `preFilter`: apply the top-level `filter` first to get a candidate key set, then find top-k within that set via a constrained brute-force scan (exact). The HNSW predicate path is a documented future optimization (see `docs/decisions/0004-vector-index.md`).

#### Vector-only vs hybrid search

- **Vector-only:** `vectorQueries` present, no `search` text (or `search: "*"` / empty). Results are the union of vector hits, ordered by vector score descending.
- **Hybrid:** `vectorQueries` present AND a non-trivial `search` text. The full-text engine produces a scored set; the vector index produces a scored set; results are the **union** (documents matching either side), ordered by score descending where a document's score is the max of its vector score and its full-text score (`1.0` in Phase 2). Azure fuses with RRF/a ranking model, so exact hybrid ordering is an acknowledged difference (see §Known differences) — but recall (union, not intersection) matches Azure.
- **Full-text only:** no `vectorQueries`. Existing behaviour unchanged.

### Scoring and response

- `@search.score` for vector results is the similarity score for the query's metric:
  - `cosine`: `1 - cosine_distance` (cosine similarity), value in [-1, 1] (1 = identical direction).
  - `dotProduct`: the raw inner product value (can be negative; ordering still descending).
  - `euclidean`: `1 / (1 + l2_distance)` (mapping to (0, 1], higher = closer).
- All three formulas are emulator-defined approximations of Azure's internal scoring — exact values differ from Azure (see §Known differences). Test assertions must check ordering and recall, not exact score equality.
- For hybrid results, score is `max(vector score, full-text score)`; full-text-only score remains `1.0` (unchanged from Phase 2).
- Results are ordered by `@search.score` descending (highest similarity first), with the key field as tie-breaker for determinism.
- `top`/`skip` pagination and `count=true` apply to the **merged** (union) result set, after ordering. `select` projection includes/excludes vector fields like any other field (large payload warning documented in §What is NOT in scope).

### Continuation tokens with vector search

Phase 2 tokens are `base64(json{filter, orderby, skip, state_version})`. Vector queries are part of the search identity, so:

- When `vectorQueries` is present, the token additionally carries a hash of the `vectorQueries` array (as serialized) plus `vectorFilterMode`. A next-page request whose vector queries differ from the token → `400 InvalidQuery` ("vector query changed during paging").
- `state_version` staleness applies unchanged (document mutation between pages → `400` stale token).

### `vectorFilterMode` and other vector search options

Accepted and functional:

- `vectorQueries` (array, Azure shape as above).
- `vectorFilterMode` (`postFilter` / `preFilter`).
- Top-level `filter` (shared with full-text; gates vector results per `vectorFilterMode`).
- Per-query `exhaustive` (forces brute-force for that query).
- `weight`: accepted but inert (documented difference).

Rejected with `400 UnsupportedQuery` (semantic search / vectorization, not raw vector):

- `semantic`, `semanticConfiguration`, `semanticQuery`, `semanticErrorHandling`, `semanticMaxWaitInMilliseconds`.
- `vectorQueries[].kind: "text"` (vectorizer queries), `vectorizers` in index config, `hybridSearch` preview knobs, per-query `threshold` (preview).

Vector fields in other query options (all `400 InvalidQuery`):

- `filter`: vector fields cannot appear (not `filterable`).
- `orderby`: vector fields cannot appear (not `sortable`).
- `facets`: vector fields cannot appear (not `facetable`).
- `searchFields` / `search`: vector fields are not full-text indexed; referencing one → `400 InvalidQuery`.

### Architecture integration

```
source/rust/src/
  vector/
    mod.rs          — VectorIndex, VectorEngine (per-index vector store)
    distance.rs     — Metric enum, score derivation, exact brute-force scoring helpers
  query/
    mod.rs          — unchanged (full-text only)
  service/
    mod.rs          — search() extended: parse vectorQueries, call VectorEngine,
                      merge with full-text results (union), apply filter mode, score, order
  storage/
    mod.rs          — FieldDefinition: add `vector_dimensions: Option<usize>`,
                      `vector_search_profile: Option<String>`
                      IndexDefinition: add `vector_search: Option<Value>` (raw, echoed)
                      + parsed profiles/algorithms table
  api/
    mod.rs          — search handler: pass the raw body to the service
```

The `VectorEngine` sits alongside `SearchEngine` in the service layer. The service orchestrates:

1. Parse and validate `vectorQueries` from the request body (accepting both REST and SDK keys, e.g. `k_nearest_neighbors`→`k`).
2. Resolve each query's `fields` → per-field `VectorIndex` (via field's `vectorSearchProfile` → profile → algorithm).
3. Execute each (query × field): HNSW path by default; brute-force path when the profile kind is `exhaustiveKnn` or the query sets `exhaustive: true`. Returns scored keys.
4. Union per-field hits per query (best score wins), then union across queries (best score wins).
5. If full-text is also active, call `SearchEngine.search(...)` and union with vector hits (`max` score).
6. Apply `vectorFilterMode` semantics against the top-level `filter` (pre constrains candidates before top-k; post filters after retrieval).
7. Resolve keys to documents from storage.
8. Order by score descending (key tie-breaker), apply top/skip/select; `count` reflects the merged set.
9. Build response with `@search.score` values per §Scoring.

### Configuration

No new behaviour flags. Vector behaviour is driven by the index schema (profile → algorithm, metric, parameters) plus per-query `exhaustive`.

Optional: `EMULATOR_VECTOR__MAX_DIMENSION` (default `3072`, same `__` nesting convention as `EMULATOR_STORAGE__MODE`) to cap accepted vector dimensions, useful for memory-constrained CI environments. Values above the cap → `400 InvalidIndex` at index creation.

### Error handling

New error cases (all `400` with Azure structure):

| Condition | Code | Message pattern |
|-----------|------|-----------------|
| Vector field missing/invalid `dimensions` (missing, 0, >cap, non-integer) | `InvalidIndex` | "Vector field 'X' has invalid dimensions N; must be 1-3072" |
| Vector field with `searchable: false` | `InvalidIndex` | "Vector field 'X' must be searchable" |
| Vector field with `filterable`/`sortable`/`facetable`/`key` | `InvalidIndex` | "Vector field 'X' cannot be filterable/sortable/facetable/key" |
| Index has vector fields but no `vectorSearch` property (or no `profiles`) | `InvalidIndex` | "Index 'X' has vector fields but no vectorSearch configuration" |
| Vector field references unknown profile name | `InvalidIndex` | "Vector field 'X' references unknown vector search profile 'Y'" |
| Profile references unknown algorithm name | `InvalidIndex` | "Vector search profile 'Y' references unknown algorithm 'Z'" |
| Unknown `metric` | `InvalidIndex` | "Unknown metric 'M' on algorithm 'Z'" |
| Quantized vector type (`Collection(Edm.Half)`, `...Int8`, etc.) | `InvalidIndex` | "Unsupported vector field type 'T'; only 'Collection(Edm.Single)' is supported" |
| `vectorQueries` references unknown/non-vector field | `InvalidQuery` | "Vector query field 'X' is not a vector field in index 'Y'" |
| `vectorQueries` vector length mismatch | `InvalidQuery` | "Vector query for 'X' has dimension N; expected M" |
| `vectorQueries` vector contains non-finite values | `InvalidQuery` | "Vector query for 'X' contains non-finite values" |
| `k`/`k_nearest_neighbors` not a positive integer (or >1000) | `InvalidQuery` | "Vector query 'k' must be a positive integer (max 1000)" |
| More than 5 `vectorQueries` entries | `InvalidQuery` | "At most 5 vector queries are supported" |
| `vectorQueries[].kind: "text"` (vectorizer) | `UnsupportedQuery` | "Vectorizer queries (kind 'text') are not supported" |
| Vector query changed between pages (token mismatch) | `InvalidQuery` | "Vector query changed during paging; restart the search" |
| Document vector field wrong length | per-doc `400` | "Field 'X' expects a vector of dimension M, got N" |
| Document vector field contains non-numeric values | per-doc `400` | "Field 'X' must contain only numeric values" |

### What is NOT in scope

- **Semantic search** (`semantic`, `semanticConfiguration`, etc.) — Azure's hosted model inference (extractive answers, captions, query rewriting). Rejected with `400 UnsupportedQuery` (unchanged from Phase 2).
- **Vectorizers / integrated vectorization** (`kind: "text"` queries, `vectorizers` config) — rejected with `400 UnsupportedQuery`. Callers must supply raw vectors.
- **Quantized vector types** (`Collection(Edm.Half)` etc.) — rejected at index creation.
- **Weighted fusion / RRF** — `weight`, `hybridSearch` preview knobs accepted-but-inert or rejected per §Vector search options; hybrid is union + max-score (documented difference).
- **Vector field in filters** — vector fields cannot appear in OData `$filter` expressions (they are not `filterable`).
- **Vector field in `orderby`** — not sortable.
- **Vector field in `facets`** — not facetable.
- **Vector field in `searchFields`/`search`** — not full-text indexed; referencing one is `400 InvalidQuery`.
- **Vector field in `select`** — accepted (the vector is returned in the response subject to `retrievable`), but this is a large payload; documented as a consideration.
- **`stored` property** — accepted but inert (no separate stored/retrievable enforcement beyond `retrievable`).
- **Multiple metrics per index** — each algorithm entry has one metric; different vector fields can use different profiles/metrics (supported).
- **Vector index persistence** — in-memory only (same as full-text and document storage).

## Deliverables

1. `source/rust/src/vector/` module: `VectorEngine`, `VectorIndex`, `HnswBackend`, distance metric helpers and exact brute-force scoring (no dot-product graph wrapper — see `docs/decisions/0004-vector-index.md`).
2. Schema validation extended: `Collection(Edm.Single)` + `dimensions` + `vectorSearchProfile`, `vectorSearch` algorithms + profiles parsing and validation.
3. Document validation extended: vector field type checking (array of floats, correct dimension, finite values).
4. Search path extended: `vectorQueries` parsing (Azure shape + SDK key translation), vector search execution, hybrid union merge, scoring, ordering.
5. `vectorFilterMode` support (`postFilter`, `preFilter`) against the top-level `filter`; per-query `exhaustive` support.
6. Updated `docs/supported_operations.md`: vector field types, vector search operations (flip `vectorQueries`/`vectorFilterMode` from Unsupported to Supported, document inert `weight`/`stored`), new error codes.
7. Updated `docs/known_differences.md`: vector scoring approximations, HNSW approximation vs exact brute-force paths, hybrid union+max vs Azure fusion, inert `weight`/`stored`, stale-token-on-vector-change.
8. `docs/decisions/0004-vector-index.md`: hnsw_rs selection rationale + pinned version, `DataId` type, SIMD feature decision, delete-via-rebuild, exact-path bypass.
9. Contract tests: `source/rust/tests/contract/vector_search.rs`.
10. Unit tests: vector module (distance scoring, index lifecycle incl. delete-rebuild, filter modes, exhaustive flag, dotProduct exactness), schema validation, document validation.
11. Python SDK compatibility tests: vector index creation, document upload with vectors, vector search, hybrid search.
12. HTTP fixtures: extend `source/tests/python/fixtures/` with vector wire-format captures for Phase 3 C# replay.
13. Updated `docs/phase_2_production_api.md`: remove vector from "Out of scope" (mark as Phase 2.1).

## Test plan

### Unit tests (`source/rust/src/vector/`)

- Distance metric correctness: cosine (incl. unnormalized input), dot product (incl. negative and large magnitudes), euclidean against known vectors.
- No pre-normalization: `DistCosine` normalizes internally; euclidean scans/computes directly; dotProduct always scans exactly.
- Index lifecycle: create, insert, search, delete (rebuild → deleted key absent), reset.
- Exact path: `exhaustiveKnn` profile and `exhaustive: true` return exact brute-force neighbours (cross-checked against a naive scan).
- Filter mode: `preFilter` constrains candidates via exact scan; `postFilter` filters after retrieval; differing result sets on a crafted fixture.
- Dimension mismatch rejection (insert and query).
- Concurrent insert/search (8 threads, no corruption).

### Contract tests (`source/rust/tests/contract/vector_search.rs`)

- Create index with vector field: valid Azure-shaped schema → `201`; missing `vectorSearch`/`profiles` → `400`; invalid `dimensions` → `400`; unknown profile / unknown algorithm → `400`; quantized type → `400`; `searchable: false` → `400`.
- Upload documents with vectors: correct dimension → per-doc `201`; wrong dimension → per-doc `400`; non-numeric → per-doc `400`.
- Vector search (raw wire format): returns correct nearest neighbours; `k` respected; `fields` as string and array both work; score ordering correct.
- Per-query `exhaustive: true` matches brute-force order on a fixture where HNSW order could differ.
- `kind: "text"` → `400 UnsupportedQuery`.
- Hybrid search: **union** of full-text and vector results (doc matching only one side is present), ordered by max score.
- `vectorFilterMode`: `preFilter` vs `postFilter` produce the documented different result sets on a crafted fixture.
- Multiple vector queries + multi-field query: union of results, best score wins.
- Paging: `top`/`skip` on merged set; `count=true` reflects merged set; changed `vectorQueries` with a continuation token → `400`.
- Error cases: unknown field, dimension mismatch, non-finite values, `k` validation (zero/negative/non-integer/>1000), >5 queries.
- Vector field in `select`: returned (subject to `retrievable`); in `filter`/`orderby`/`facets`/`searchFields` → `400 InvalidQuery`.

### Python SDK compatibility tests

- `SearchIndexClient.create_index` with vector fields (`vector_search_dimensions`, `vector_search_profile_name`) and `VectorSearch` + `HnswAlgorithmConfiguration` / `VectorSearchProfile`.
- `SearchClient.upload_documents` with vector field values.
- `SearchClient.search` with `vector_queries=[VectorizedQuery(vector=..., fields=..., k_nearest_neighbors=..., exhaustive=...)]`.
- Hybrid: `search_text=` + `vector_queries` together (union recall).
- `vector_filter_mode=` (`preFilter`/`postFilter`) + top-level `filter=`.
- Score values present and ordered; no exact-score assertions.

### E2E

- Full RAG-style flow: create index with text + vector fields, upload documents with embeddings, search by vector, search hybrid, verify result ordering and scores (ordering only).

## Known differences (to be documented)

| Behaviour | Emulator | Azure | Rationale |
|-----------|----------|-------|-----------|
| Vector search algorithm | HNSW (approximate) or brute-force (exact, for `exhaustiveKnn` / `exhaustive: true`) | HNSW (approximate) + exhaustiveKnn | Same algorithm family; recall may differ slightly on the HNSW path. The brute-force path is exact. |
| `@search.score` (all metrics) | Emulator-defined formulas (§Scoring) | Azure's internal scoring | Same direction (higher = more similar) and, for cosine, same [0,1] range. Exact values differ; do not assert equality. |
| Hybrid ranking | Union + max-score (deterministic) | RRF / ranking-model fusion | Deterministic; recall matches (union) but ordering may differ. Test assertions must not depend on Azure's hybrid ranking model. |
| Per-query `weight`, `stored`, `sessionId` | Accepted but inert | Affect ranking/storage/scoring | Deterministic test double; assertions must not depend on them. |
| Quantized vector types | Rejected | Supported | Quantization is an optimisation not needed for a test double. |
| Vectorizer (`kind: "text"`) queries | Rejected (`400 UnsupportedQuery`) | Supported with a vectorizer | No model inference in the emulator. |
| Paging with vectors | Token binds `vectorQueries`+`vectorFilterMode`; changing them mid-paging is `400` | Tokens tolerate broader reuse | Fail-fast beats silently shifted pages. |
| Max vector queries per search | 5 | 5 | Matches Azure. |
| Max dimension | 3072 (or `EMULATOR_VECTOR__MAX_DIMENSION`) | 3072 | Matches Azure; cap is lowerable for CI. |

## Checklist

### Schema and validation

- [x] `Collection(Edm.Single)` accepted (both `Edm.`-prefixed and bare SDK forms, normalized).
- [x] `dimensions` required and validated (1–cap, integer).
- [x] `vectorSearchProfile` required; unknown profile → `400`.
- [x] `vectorSearch.profiles[]` validated (unknown algorithm, duplicates → `400`).
- [x] Vector fields require `searchable: true`; rejected when `key`/`filterable`/`sortable`/`facetable`.
- [x] `retrievable: true/false` honored; `stored` accepted-but-inert.
- [x] Algorithm config validated: `kind` (`hnsw`/`exhaustiveKnn`), nested `hnswParameters`/`exhaustiveKnnParameters`, `metric`.
- [x] Quantized types rejected with clear error.
- [x] Field→profile→algorithm resolution validated.
- [x] Multiple vector fields per index supported (up to 16).

### Document management

- [x] Vector field values validated: array of floats, correct dimension, finite.
- [x] Per-document errors for invalid vectors in batch responses.
- [x] Vector stored in document (returned via `select` subject to `retrievable`).
- [x] Cosine uses `DistCosine` directly (no pre-normalization); dotProduct always scans exactly; euclidean untouched.
- [x] Merge semantics: vector field replaced wholesale on merge (same as collections).
- [x] Delete removes vector (rebuild) and deleted keys never match.

### Vector search

- [x] `vectorQueries` parsed: `kind`, `vector`, `fields` (string + array), `k`, `exhaustive`, `weight` (inert).
- [x] SDK key translation (`k_nearest_neighbors`→`k`, `fields`, `exhaustive`, `vector_filter_mode`) in the service layer.
- [x] Single vector query returns top-k by similarity; multi-field query unions per-field hits.
- [x] Multiple vector queries: union, best score.
- [x] `k` validated (positive integer, max 1000, default 3 when missing).
- [x] `kind: "text"` → `400 UnsupportedQuery`.
- [x] Score values correct per metric (cosine, dotProduct, euclidean); ordered desc, key tie-breaker.
- [x] `vectorFilterMode=postFilter`: top-level `filter` applied after retrieval.
- [x] `vectorFilterMode=preFilter`: filter constrains candidates via exact scan.
- [x] Per-query `exhaustive: true` forces brute-force.
- [x] Hybrid search: union with full-text, ordered by max score.
- [x] Vector-only search (no full-text term): all vector results returned.
- [x] `top`/`skip` pagination applied to merged results; `count` reflects merged set; token binds vector queries.
- [x] `select` projection includes/excludes vector fields correctly.
- [x] Vector fields in `filter`/`orderby`/`facets`/`searchFields` → `400 InvalidQuery`.

### Integration

- [x] Vector index created/destroyed with the emulator index.
- [x] Vector index updated on upload/merge/delete (immediate consistency; delete via rebuild).
- [x] Service reset clears all vector indexes.
- [x] Concurrent vector search + document upload does not corrupt state.
- [x] Existing full-text-only searches unaffected (no `vectorQueries` → same path as before).

### Errors

- [x] All new error cases return correct status code and Azure error structure.
- [x] Semantic/vectorizer params still rejected with `400 UnsupportedQuery`.

### Tests

- [x] Unit tests: distance metrics, index lifecycle incl. delete-rebuild, filter modes, exhaustive flag, dotProduct exactness, concurrency.
- [x] Contract tests: `source/rust/tests/contract/vector_search.rs` covers all matrix entries.
- [x] Python SDK tests: vector index creation, upload, search, hybrid, filter modes.
- [x] Fixtures captured for C# replay.
- [x] E2E: RAG-style flow.
- [x] All existing tests still pass (no regression).

### Quality gates

- [x] `cargo fmt --check` passes.
- [x] `cargo clippy --all-targets -- -D warnings` passes.
- [x] All test suites green (unit, contract, SDK, e2e).
- [x] `docs/supported_operations.md` updated (vector flipped to Supported; inert `weight`/`stored` noted).
- [x] `docs/known_differences.md` updated.
- [x] `docs/decisions/0004-vector-index.md` written (hnsw_rs version, DataId, SIMD, rebuild, exact bypass).

## Exit criteria

An application can create an index with vector fields, upload documents with embedding vectors, and perform vector similarity search and hybrid (vector + full-text) search through the unmodified Python SDK, with correct scoring, filtering, and pagination — all covered by contract and SDK tests.
