---
status: draft
status_last_reviewed: 2026-09-09
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
- Provides L2, Cosine, and a custom-distance trait (inner product / dot product implemented via the trait).
- HNSW graph for approximate nearest-neighbour; flat (brute-force) mode for small indexes and exact results.
- SIMD-accelerated (AVX2) for f32 vectors via `anndists`.
- In-process, no external service — matches the Tantivy integration pattern (see `docs/decisions/0003-search-engine.md`).

Alternative considered: **usearch** (C++ core, thin Rust bindings). Rejected for this project because it introduces a C++ build dependency that complicates the Nix build environment (the same class of problem that excluded Tantivy's `zstd` feature). hnsw_rs is the safer fit for the existing build constraints.

### Index schema: vector field types

Accept and validate `Edm.Collection(Edm.Single, <dimension>)` (and the SDK form `Collection(Edm.Single, <dimension>)`) as a field type.

- `<dimension>` is a positive integer (1–3072, matching Azure's limit).
- The field must not be `key`, `searchable`, `filterable`, `sortable`, or `facetable` (Azure restriction).
- The field must be `retrievable: true` (default) to be returned in results.
- Multiple vector fields per index are supported (up to 16, matching Azure).
- The index definition must include a `vectorSearch` property with at least one algorithm entry when vector fields are present; its absence is a `400 InvalidIndex`.

#### `vectorSearch` algorithm configuration

```json
{
  "vectorSearch": {
    "algorithms": [
      {
        "name": "algo1",
        "kind": "hnsw",
        "vectorFormat": "float32",
        "metric": "cosine",
        "exhaustiveThreshold": 0,
        "parameters": {
          "m": 4,
          "efConstruction": 100,
          "efSearch": 100
        }
      }
    ]
  }
}
```

Supported:

- `kind`: `hnsw` (HNSW graph) and `flat` (brute-force; maps to hnsw_rs with `efSearch = index size`).
- `vectorFormat`: `float32` only. `byte` (quantized) is rejected with `400 InvalidIndex`.
- `metric`: `cosine`, `dotProduct`, `euclidean`.
- `exhaustiveThreshold`: accepted and stored; when the index has fewer vectors than this threshold, brute-force is used regardless of `kind` (matching Azure behaviour). Default `0` (always use the declared algorithm).
- `parameters`: `m` (default 4), `efConstruction` (default 100), `efSearch` (default 100). Accepted and passed to hnsw_rs.

Each vector field in the schema references an algorithm by name via its `vectorSearchAlgorithm` property. A field referencing a non-existent algorithm name is a `400 InvalidIndex`.

### Document validation: vector fields

- A vector field value must be a JSON array of numbers (floats).
- The array length must exactly match the declared dimension.
- Values must be finite (no `NaN`, no `Infinity`).
- Violations produce a per-document `400` in the batch response (same pattern as existing type validation).
- Vector fields are stored in the `Document.fields` map as `Value::Array(Vec<Value::Number>)` (same as any other field); the vector index is a secondary structure updated alongside storage.

### Vector index lifecycle

A new `VectorIndex` component (in `src/vector/`) mirrors the `SearchEngine` pattern:

- One hnsw_rs index per (emulator index, vector field) pair.
- Created at index creation time (from the schema's vector fields + algorithm config).
- Updated on document upload/merge (upsert by key) and delete.
- Destroyed on index deletion and service reset.
- Guarded by the same `RwLock` pattern as `SearchEngine` (read lock for search, write lock for mutation).
- Immediate consistency: vectors are searchable as soon as the upload batch returns (synchronous insert, matching the full-text engine's commit+reload pattern).

#### hnsw_rs integration notes

- hnsw_rs uses `f32` slices as its point type. Vectors are stored as `Vec<f32>` in the index.
- The index is keyed by a `u32` internal ID; a side-map (`BTreeMap<u32, String>`) tracks internal ID → document key for result resolution.
- For `flat` kind or when `exhaustiveThreshold` is met, hnsw_rs is configured with `efSearch = total_vectors` (effectively brute-force).
- Dot product: implemented via `anndists`'s custom distance trait (inner product; hnsw_rs optimises for minimising distance, so we negate the dot product or use the appropriate metric wrapper).
- Cosine: first-class in `anndists`.
- Euclidean (L2): first-class in `anndists`.

### Query: `vectorQueries` parameter

The search request body accepts a `vectorQueries` array:

```json
{
  "vectorQueries": [
    {
      "key": "embedding",
      "vector": [0.1, 0.2, 0.3, "..."],
      "k": 50,
      "filters": "category eq 'tech'"
    }
  ],
  "vectorFilterMode": "postFilter"
}
```

- `key`: must reference a vector field in the index schema. Unknown key → `400 InvalidQuery`.
- `vector`: array of floats; length must match the field's dimension. Mismatch → `400 InvalidQuery`.
- `k`: number of nearest neighbours to retrieve (default 1000, max 1000). Must be a positive integer.
- `filters`: optional OData filter expression, parsed by the existing `src/filter` module. Applied per `vectorFilterMode`.
- Multiple `vectorQueries` entries are supported (Azure allows up to 5); results are the union of all vector query matches, scored by the best (highest) score across queries.

#### `vectorFilterMode`

- `postFilter` (default): retrieve top-k by vector similarity, then apply the filter to the results.
- `preFilter`: apply the filter first to get a candidate set, then find top-k within that set. hnsw_rs supports filtered search via its `filter` module (a predicate over point IDs).

Both modes are implemented. `postFilter` is the simple path (filter after retrieval); `preFilter` uses hnsw_rs's filter trait to constrain the graph traversal.

#### Vector-only vs hybrid search

- **Vector-only:** `vectorQueries` present, no `search` text (or `search: "*"`). Results ordered by vector score descending.
- **Hybrid:** `vectorQueries` present AND a non-trivial `search` text. The full-text engine produces a candidate set; the vector index produces a scored set; results are the intersection (documents matching both), ordered by vector score. This matches Azure's default hybrid behaviour where both signals must match.
- **Full-text only:** no `vectorQueries`. Existing behaviour unchanged.

### Scoring and response

- `@search.score` for vector results is the similarity score from hnsw_rs:
  - `cosine`: value in [0, 1] (1 = identical direction). hnsw_rs returns distance; we convert: `score = 1 - distance` (cosine distance is `1 - cosine_similarity`).
  - `dotProduct`: the raw inner product value (can be negative).
  - `euclidean`: `1 / (1 + distance)` (mapping to (0, 1], higher = closer). Azure uses a similar inverse-distance mapping.
- For hybrid results, the vector score is used (full-text score remains `1.0` as in Phase 2).
- For full-text-only results, `@search.score` remains `1.0` (unchanged from Phase 2).
- Results are ordered by `@search.score` descending (highest similarity first), with the key field as tie-breaker for determinism.

### `vectorFilterMode` and other vector search options

Accepted and functional:

- `vectorQueries` (array, as above).
- `vectorFilterMode` (`postFilter` / `preFilter`).

Rejected with `400 UnsupportedQuery` (semantic search, not vector):

- `semantic`, `semanticConfiguration`, `semanticQuery`, `semanticErrorHandling`, `semanticMaxWaitInMilliseconds`.

### Architecture integration

```
src/
  vector/
    mod.rs          — VectorIndex, VectorEngine (per-index vector store)
    distance.rs     — Metric enum, anndists trait impls (dot product wrapper)
  query/
    mod.rs          — unchanged (full-text only)
  service/
    mod.rs          — search() extended: parse vectorQueries, call VectorEngine,
                      merge with full-text results, apply filter mode, score, order
  storage/
    mod.rs          — FieldDefinition: add `vector_dimension: Option<usize>`,
                      `vector_search_algorithm: Option<String>`
                      IndexDefinition: add `vector_search: Option<Value>` (raw)
  api/
    mod.rs          — search handler: pass vectorQueries/vectorFilterMode to service
```

The `VectorEngine` sits alongside `SearchEngine` in the service layer. The service orchestrates:

1. Parse `vectorQueries` from the request body.
2. If present, call `VectorEngine.search(index, field, vector, k, filter)` → scored keys.
3. If full-text is also active, call `SearchEngine.search(...)` → matching keys.
4. Intersect (hybrid) or use vector results alone.
5. Apply `vectorFilterMode` semantics (pre vs post).
6. Resolve keys to documents from storage.
7. Order by score descending, apply top/skip/select.
8. Build response with real `@search.score` values.

### Configuration

No new environment variables. Vector behaviour is entirely driven by the index schema (algorithm, metric, parameters). The `exhaustiveThreshold` in the schema controls when brute-force is used.

Optional: `EMULATOR_VECTOR__MAX_DIMENSION` (default `3072`) to cap accepted vector dimensions, useful for memory-constrained CI environments.

### Error handling

New error cases (all `400` with Azure structure):

| Condition | Code | Message pattern |
|-----------|------|-----------------|
| Vector field with invalid dimension (0, >3072, non-integer) | `InvalidIndex` | "Vector field 'X' has invalid dimension N; must be 1-3072" |
| Vector field with `searchable`/`filterable`/`sortable`/`facetable`/`key` | `InvalidIndex` | "Vector field 'X' cannot be searchable/filterable/sortable/facetable/key" |
| Index has vector fields but no `vectorSearch` property | `InvalidIndex` | "Index 'X' has vector fields but no vectorSearch configuration" |
| Vector field references unknown algorithm name | `InvalidIndex` | "Vector field 'X' references unknown algorithm 'Y'" |
| `vectorFormat` other than `float32` | `InvalidIndex` | "Unsupported vectorFormat 'byte'; only 'float32' is supported" |
| `vectorQueries` references unknown field | `InvalidQuery` | "Vector query key 'X' is not a vector field in index 'Y'" |
| `vectorQueries` vector length mismatch | `InvalidQuery` | "Vector query for 'X' has dimension N; expected M" |
| `vectorQueries` vector contains non-finite values | `InvalidQuery` | "Vector query for 'X' contains non-finite values" |
| `k` not a positive integer | `InvalidQuery` | "Vector query 'k' must be a positive integer" |
| More than 5 `vectorQueries` entries | `InvalidQuery` | "At most 5 vector queries are supported" |
| Document vector field wrong length | per-doc `400` | "Field 'X' expects a vector of dimension M, got N" |
| Document vector field contains non-numeric values | per-doc `400` | "Field 'X' must contain only numeric values" |

### What is NOT in scope

- **Semantic search** (`semantic`, `semanticConfiguration`, etc.) — Azure's hosted model inference (extractive answers, captions, query rewriting). Rejected with `400 UnsupportedQuery` (unchanged from Phase 2).
- **`byte` vector format** (quantized vectors) — rejected at index creation.
- **Vector field in filters** — vector fields cannot appear in OData `$filter` expressions (they are not `filterable`).
- **Vector field in `orderby`** — not sortable.
- **Vector field in `facets`** — not facetable.
- **Vector field in `select`** — accepted (the vector is returned in the response if `retrievable: true`), but this is a large payload; documented as a consideration.
- **Multiple metrics per index** — each algorithm entry has one metric; different vector fields can use different algorithms/metrics (supported).
- **Vector index persistence** — in-memory only (same as full-text and document storage).

## Deliverables

1. `src/vector/` module: `VectorEngine`, `VectorIndex`, distance metric wrappers.
2. Schema validation extended: `Edm.Collection(Edm.Single, N)` field type, `vectorSearch` config parsing and validation.
3. Document validation extended: vector field type checking (array of floats, correct dimension, finite values).
4. Search path extended: `vectorQueries` parsing, vector search execution, hybrid merge, scoring, ordering.
5. `vectorFilterMode` support (`postFilter`, `preFilter`).
6. Updated `docs/supported_operations.md`: vector field types, vector search operations, new error codes.
7. Updated `docs/known_differences.md`: vector scoring differences, HNSW approximation vs Azure's exact results at small scale.
8. `docs/decisions/0004-vector-index.md`: hnsw_rs selection rationale.
9. Contract tests: `tests/contract/vector_search.rs`.
10. Unit tests: vector module (distance, index lifecycle, filter modes), schema validation, document validation.
11. Python SDK compatibility tests: vector index creation, document upload with vectors, vector search, hybrid search.
12. Updated `docs/phase_2_production_api.md`: remove vector from "Out of scope" (mark as Phase 2.1).

## Test plan

### Unit tests (`src/vector/`)

- Distance metric correctness: cosine, dot product, euclidean against known vectors.
- Index lifecycle: create, insert, search, delete, reset.
- `exhaustiveThreshold`: small index uses brute-force, large index uses HNSW.
- Filter mode: `preFilter` constrains candidates; `postFilter` filters after retrieval.
- Dimension mismatch rejection.
- Concurrent insert/search (8 threads, no corruption).

### Contract tests (`tests/contract/vector_search.rs`)

- Create index with vector field: valid schema → `201`; missing `vectorSearch` → `400`; invalid dimension → `400`; wrong `vectorFormat` → `400`.
- Upload documents with vectors: correct dimension → `201`; wrong dimension → per-doc `400`; non-numeric → per-doc `400`.
- Vector search: returns correct nearest neighbours; `k` respected; score ordering correct.
- Hybrid search: intersection of full-text and vector results.
- `vectorFilterMode`: `preFilter` vs `postFilter` produce correct result sets.
- Multiple vector queries: union of results, best score wins.
- Error cases: unknown key, dimension mismatch, non-finite values, `k` validation.
- Vector field in `select`: returned in response.
- Vector field NOT in `filter`/`orderby`/`facets`: `400 InvalidQuery`.

### Python SDK compatibility tests

- `SearchIndexClient.create_index` with vector fields and `vectorSearch` config.
- `SearchClient.upload_documents` with vector field values.
- `SearchClient.search` with `vector_queries` parameter (the SDK's `vector_queries=` kwarg).
- Hybrid: `search` + `vector_queries` together.
- `vector_filter_mode` parameter.
- Score values are present and ordered correctly.

### E2E

- Full RAG-style flow: create index with text + vector fields, upload documents with embeddings, search by vector, search hybrid, verify result ordering and scores.

## Known differences (to be documented)

| Behaviour | Emulator | Azure | Rationale |
|-----------|----------|-------|-----------|
| Vector search algorithm | HNSW (approximate) or flat (exact) | HNSW (approximate) | Same algorithm family; recall may differ slightly due to implementation. At `exhaustiveThreshold` or `flat` kind, results are exact. |
| `@search.score` for cosine | `1 - cosine_distance` (from hnsw_rs) | Azure's internal scoring | Both in [0,1], higher = more similar. Exact values may differ. |
| `@search.score` for euclidean | `1 / (1 + l2_distance)` | Azure's internal scoring | Both in (0,1], higher = closer. Exact values may differ. |
| Hybrid ranking | Vector score only (full-text is a gate) | Azure combines both signals with a ranking model | Deterministic; test assertions must not depend on Azure's hybrid ranking model. |
| `byte` vector format | Rejected | Supported | Quantized vectors are an optimisation not needed for a test double. |
| Max vector queries per search | 5 | 5 | Matches Azure. |
| Max dimension | 3072 | 3072 | Matches Azure. |

## Checklist

### Schema and validation

- [ ] `Edm.Collection(Edm.Single, N)` accepted as a field type (both `Edm.` and bare `Collection(...)` forms).
- [ ] Dimension validated (1–3072, integer).
- [ ] Vector fields rejected when `key`/`searchable`/`filterable`/`sortable`/`facetable` is true.
- [ ] `vectorSearch` property required when vector fields are present.
- [ ] Algorithm config validated: `kind`, `vectorFormat`, `metric`, `parameters`.
- [ ] `byte` format rejected with clear error.
- [ ] Field-to-algorithm reference validated.
- [ ] Multiple vector fields per index supported (up to 16).

### Document management

- [ ] Vector field values validated: array of floats, correct dimension, finite.
- [ ] Per-document errors for invalid vectors in batch responses.
- [ ] Vector stored in document (retrievable in search results via `select`).
- [ ] Merge semantics: vector field replaced wholesale on merge (same as collections).
- [ ] Delete removes vector from the vector index.

### Vector search

- [ ] `vectorQueries` parsed: `key`, `vector`, `k`, `filters`.
- [ ] Single vector query returns top-k by similarity.
- [ ] Multiple vector queries: union, best score.
- [ ] `k` validated (positive integer, max 1000).
- [ ] Score values correct per metric (cosine, dotProduct, euclidean).
- [ ] Results ordered by score descending, key tie-breaker.
- [ ] `vectorFilterMode=postFilter`: filter applied after retrieval.
- [ ] `vectorFilterMode=preFilter`: filter constrains graph traversal.
- [ ] Hybrid search: intersection with full-text, ordered by vector score.
- [ ] Vector-only search (no full-text term): all vector results returned.
- [ ] `top`/`skip` pagination applied to vector results.
- [ ] `select` projection includes/excludes vector fields correctly.
- [ ] `count=true` works with vector search.

### Integration

- [ ] Vector index created/destroyed with the emulator index.
- [ ] Vector index updated on upload/merge/delete (immediate consistency).
- [ ] Service reset clears all vector indexes.
- [ ] Concurrent vector search + document upload does not corrupt state.
- [ ] Existing full-text-only searches unaffected (no `vectorQueries` → same path as before).

### Errors

- [ ] All new error cases return correct status code and Azure error structure.
- [ ] Vector field in `filter`/`orderby`/`facets` → `400 InvalidQuery`.
- [ ] Semantic search params still rejected with `400 UnsupportedQuery`.

### Tests

- [ ] Unit tests: distance metrics, index lifecycle, filter modes, concurrency.
- [ ] Contract tests: `tests/contract/vector_search.rs` covers all matrix entries.
- [ ] Python SDK tests: vector index creation, upload, search, hybrid.
- [ ] E2E: RAG-style flow.
- [ ] All existing tests still pass (no regression).

### Quality gates

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] All test suites green (unit, contract, SDK, e2e).
- [ ] `docs/supported_operations.md` updated.
- [ ] `docs/known_differences.md` updated.
- [ ] `docs/decisions/0004-vector-index.md` written.

## Exit criteria

An application can create an index with vector fields, upload documents with embedding vectors, and perform vector similarity search and hybrid (vector + full-text) search through the unmodified Python SDK, with correct scoring, filtering, and pagination — all covered by contract and SDK tests.
