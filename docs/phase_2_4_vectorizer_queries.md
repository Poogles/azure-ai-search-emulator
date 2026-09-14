---
status: draft
status_last_reviewed: 2026-09-14
---

# Phase 2.4 — Vectorizer Queries

## Purpose

Add vectorizer query support (`kind: "text"`) to the emulator so that applications that use Azure AI Search's integrated text-to-vector pipeline can run their full query code paths against the emulator unmodified. The emulator accepts the vectorizer configuration in the index schema and generates deterministic vectors from text at query time, enabling the complete vectorizer query flow (text in → vector search → ranked results) without external model inference.

## Motivation

Phase 2.1 implemented raw-vector search (`kind: "vector"`) but explicitly rejected vectorizer queries (`kind: "text"`) with `400 UnsupportedQuery`, documenting that "no vectorizer in the emulator; callers must supply raw vectors." Applications that use Azure AI Search's vectorizer feature (where the service calls an embedding model to convert text to vectors) cannot run against the emulator: the search request is rejected. This is a common pattern in RAG applications that do not manage embeddings client-side — the application sends a text query and the service handles vectorization.

This phase closes that gap by implementing a deterministic, in-process text-to-vector function. The generated vectors are not semantically meaningful in the way a neural embedding model would produce them, but they are:

- **Deterministic:** the same text always produces the same vector.
- **Dimensionally correct:** the vector length matches the field's declared `dimensions`.
- **Token-sensitive:** texts sharing tokens produce more similar vectors than texts with disjoint tokens (basic lexical similarity).

This is sufficient for applications to test their vectorizer query code paths (request construction, response parsing, result ordering, hybrid search with vectorizer queries) without requiring an external embedding service.

## Scope

### Index schema: vectorizer configuration

The index definition accepts a `vectorizers` property (Azure wire format) alongside the existing `vectorSearch` property:

```json
{
  "vectorizers": [
    {
      "name": "my-embedder",
      "kind": "uri",
      "parameters": {
        "uri": "https://my-resource.openai.azure.com/embeddings",
        "parameters": {
          "deploymentName": "text-embedding-ada-002",
          "dimensions": 1536
        }
      },
      "sourceContext": {
        "sourceType": "field",
        "fields": ["content"]
      }
    }
  ],
  "vectorSearch": {
    "algorithms": [
      {
        "name": "hnsw-1",
        "kind": "hnsw",
        "hnswParameters": { "m": 4, "efConstruction": 400, "efSearch": 500, "metric": "cosine" }
      }
    ],
    "profiles": [
      { "name": "my-profile", "algorithmConfigurationName": "hnsw-1" }
    ]
  }
}
```

Rules (rejected with `400 InvalidIndex`):

- `vectorizers` is an array (may be empty or absent for raw-vector-only indexes).
- Each vectorizer requires a non-empty `name` (unique within the index).
- `kind` must be `"uri"` (the only kind Azure supports in the stable API). Other kinds → `400 InvalidIndex`.
- `parameters.uri` is required and must be a non-empty string. The URI is **not called** by the emulator (documented difference); it is stored and echoed for wire-format compatibility.
- `parameters.parameters` is an optional object (stored opaquely, echoed in responses).
- `sourceContext` is optional:
  - `sourceType`: must be `"field"` (the only type Azure supports).
  - `fields`: array of field names (must exist in the index schema, must be `searchable: true` string fields). Used to determine which field's text is vectorized during document indexing.
- A vector field can reference a vectorizer via a `vectorizer` property (see §Vector field vectorizer reference).
- An index can have multiple vectorizers (different embedding models for different fields).
- The `vectorizers` property is independent of `vectorSearch`: an index can have vectorizers without vector fields (unusual but valid) or vector fields without vectorizers (raw-vector-only, Phase 2.1 behaviour).

SDK ↔ wire mapping:

| SDK (`azure-search-documents`) | Wire / emulator storage |
|:---|:---|
| `UriVectorizationSource(name, uri, parameters=...)` | `vectorizers[]` with `kind: "uri"` |
| `SearchField(vectorizer_name="my-embedder")` | Field's `vectorizer` property |
| `source_context=SourceContext(fields=["content"])` | `sourceContext` |

### Vector field: vectorizer reference

A vector field can optionally reference a vectorizer by name:

```json
{
  "name": "content_vector",
  "type": "Collection(Edm.Single)",
  "searchable": true,
  "retrievable": true,
  "dimensions": 1536,
  "vectorSearchProfile": "my-profile",
  "vectorizer": "my-embedder"
}
```

Rules:

- `vectorizer` is an optional string referencing a name in the index's `vectorizers` array.
- If present, the name must match an existing vectorizer → unknown name → `400 InvalidIndex`.
- When a vector field references a vectorizer, document upload can omit the vector field value: the emulator generates the vector from the `sourceContext.fields` text at indexing time (see §Document indexing with vectorizer).
- When a vector field does NOT reference a vectorizer, the vector must be supplied in the document (Phase 2.1 behaviour, unchanged).
- A vector field with a vectorizer can still receive an explicit vector in the document (the explicit vector takes precedence over the generated one).

### Document indexing with vectorizer

When a document is uploaded/merged and a vector field references a vectorizer:

- If the document includes an explicit value for the vector field: use it (validate as in Phase 2.1: correct dimension, finite values). The vectorizer is not invoked.
- If the document omits the vector field (or includes `null`): generate a vector from the source text.
  - Source text: concatenate the values of the vectorizer's `sourceContext.fields` (in order), separated by a space. If no source fields are configured or all are empty/missing, generate a zero vector.
  - The generated vector is stored in the document's field map (as if it were uploaded) and indexed in the vector engine.
  - The generated vector is returned in search responses and `get_document` (subject to `retrievable`), same as an uploaded vector.
- Merge semantics: if a merge provides an explicit vector, it replaces the previously generated one. If a merge omits the vector field, the previously stored vector (generated or explicit) is preserved (same as any other field on merge).
- Delete: unchanged (vector removed from the index).

### Text-to-vector function

The emulator uses a deterministic hash-based embedding to convert text to a fixed-dimension vector:

Algorithm:

1. **Tokenize:** split the input text into tokens using the same analyzer as full-text search (lowercase, punctuation split, stopword removal, stemming). This ensures that "running" and "run" produce the same token, matching the full-text analyzer's behaviour.
2. **Hash each token:** compute a FNV-1a 64-bit hash of the token string. Use the hash to determine:
   - **Bucket index:** `hash % dimensions` (which vector component to accumulate into).
   - **Sign:** `+1` if `hash >> 63 == 0`, else `-1` (signed hashing to reduce collision bias).
3. **Accumulate:** for each token, add `sign` to `vector[bucket]`. If multiple tokens hash to the same bucket, their signs accumulate (cancellation is possible).
4. **Normalize:** L2-normalize the vector (divide by its Euclidean norm) so that the result is a unit vector. This ensures cosine similarity is well-defined and in [-1, 1]. If the vector is all zeros (empty text or all tokens cancelled), return a zero vector.

Properties:

- **Deterministic:** same text → same vector (FNV-1a is deterministic; the analyzer is deterministic).
- **Dimensionally correct:** output length is always `dimensions` (the field's declared dimension).
- **Token-sensitive:** two texts sharing tokens will have overlapping non-zero components in the same buckets, producing higher cosine similarity than texts with disjoint tokens.
- **Not semantically meaningful:** "king" and "monarch" do not produce similar vectors (no distributed representation). Only exact token overlap (after analysis) produces similarity.
- **Fast:** O(tokens × 1) hash operations + O(dimensions) normalization. No external calls, no model loading.

Rationale for hash-based embedding (vs. alternatives):

- **TF-IDF / bag-of-words:** would require a vocabulary built from the corpus, which changes as documents are added (non-deterministic across test runs unless the corpus is fixed). Hash-based is corpus-independent.
- **Character n-gram hashing:** more sensitive to spelling variations but less aligned with the full-text analyzer's tokenization. Token-level hashing is consistent with the rest of the emulator's text processing.
- **External embedding API call:** violates the "no external services" constraint and introduces network dependency, latency, and non-determinism (model version changes).
- **Local neural model (e.g., a small sentence-transformer):** introduces a large binary dependency (hundreds of MB), contradicts the <20 MB Docker image constraint, and adds startup latency.

The hash-based approach is the simplest function that satisfies: deterministic, dimensionally correct, token-sensitive, no external dependencies, fast. It is sufficient for testing code paths; it is not sufficient for testing semantic quality (documented difference).

### Query: `kind: "text"` vectorizer queries

The search request body accepts `vectorQueries` entries with `kind: "text"` (replacing the current `400 UnsupportedQuery` rejection):

```json
{
  "search": "",
  "vectorQueries": [
    {
      "kind": "text",
      "text": "quantum computing applications",
      "fields": "content_vector",
      "k": 10,
      "exhaustive": false
    }
  ]
}
```

Field semantics:

- `kind`: `"text"` (vectorizer query). The query's `text` is vectorized using the target field's vectorizer.
- `text` (required for `kind: "text"`): the text to vectorize. Non-empty string. Empty/missing → `400 InvalidQuery`.
- `fields` (required): the vector field(s) to search. Each field must reference a vectorizer (via its `vectorizer` property). A field without a vectorizer → `400 InvalidQuery` ("field 'X' does not have a vectorizer; use kind 'vector' with an explicit vector").
- `k`, `exhaustive`, `weight`: same semantics as `kind: "vector"` (Phase 2.1).
- `vector` property: **must not be present** when `kind: "text"` (the vector is generated, not supplied). If both `text` and `vector` are present → `400 InvalidQuery`.
- Multiple `kind: "text"` queries in one search: supported (same as multiple `kind: "vector"` queries; union of results, best score wins).
- Mixed `kind: "text"` and `kind: "vector"` queries in one search: supported (union of all results).

Vectorization at query time:

1. Resolve the target field's vectorizer (field's `vectorizer` property → vectorizer in the index's `vectorizers` array).
2. Generate the query vector from `text` using the text-to-vector function (§Text-to-vector function), with `dimensions` = the field's declared dimension.
3. Execute the vector search as normal (HNSW or brute-force, per the field's profile and `exhaustive` flag).
4. Score and rank as in Phase 2.1.

### Document indexing: vectorizer at upload time

The same text-to-vector function is used at document indexing time (when the vector field value is omitted) and at query time. This ensures that:

- A document indexed with generated vector `[v1, v2, ...]` and a query vectorized to `[q1, q2, ...]` produce a meaningful cosine similarity (high when the document text and query text share tokens).
- The system is self-consistent: the "embedding model" is the same function at index time and query time.

### Hybrid search with vectorizer queries

- `kind: "text"` vectorizer queries participate in hybrid search (vector + full-text) the same as `kind: "vector"` queries:
  - If `search` text is also present (non-empty, non-`"*"`): the full-text engine produces a scored set; the vectorizer query produces a scored set; results are fused with RRF (Phase 2.1 behaviour).
  - `vectorFilterMode` (`preFilter`/`postFilter`) applies the same way.
- The `search` text for full-text and the `text` for vectorization are independent: the full-text engine uses `search`, the vectorizer uses `vectorQueries[].text`.

### `sourceContext` and document indexing

The vectorizer's `sourceContext.fields` determines which document fields are used as the text source for vector generation at indexing time:

- If `sourceContext` is present with `fields: ["content"]`: the document's `content` field value is the text source.
- If `sourceContext` is present with `fields: ["title", "content"]`: the text source is `title + " " + content` (concatenated in order).
- If `sourceContext` is absent: the text source is the concatenation of all `searchable: true` string fields in the document (in schema order). This is a fallback for vectorizers configured without explicit source context.
- If the source text is empty (all source fields missing or empty): generate a zero vector. A zero vector has zero cosine similarity to all query vectors (the document will not appear in vector search results unless the query vector is also zero).

### What is NOT in scope

- **External API calls** — The `parameters.uri` is stored and echoed but never called. No HTTP requests to embedding services. The emulator is fully self-contained.
- **Neural embedding quality** — The hash-based embedding produces lexically-similar (token-overlap) vectors, not semantically-similar (meaning-based) vectors. "king" and "monarch" are not similar; "king" and "king" are identical. Test assertions must use token-overlap-based expectations, not semantic-similarity expectations.
- **`kind: "none"` vectorizer** — Azure's `kind: "none"` means "no vectorizer" (raw vectors only); it is not a vectorizer configuration. The emulator does not accept `kind: "none"` in the `vectorizers` array (→ `400 InvalidIndex`).
- **Multiple source contexts per vectorizer** — One `sourceContext` per vectorizer (matching Azure).
- **Vectorizer-specific HNSW tuning** — The vectorizer does not affect the HNSW parameters; the field's profile/algorithm configuration (Phase 2.1) governs the search algorithm.
- **Async vectorization** — Vectorization is synchronous (fast hash computation). No async/polling pattern.
- **Vectorizer model versioning** — The `parameters.parameters` object is stored opaquely; the emulator does not interpret model names or versions.
- **Semantic search with vectorizer queries** — `semantic` + `vectorQueries` (any kind) remains mutually exclusive (Phase 2.3).

## Architecture integration

```
source/rust/src/
  vector/
    mod.rs            — unchanged (VectorEngine, VectorIndex)
    vectorizer.rs     — NEW: text-to-vector function (tokenize, hash, accumulate, normalize)
  storage/
    mod.rs            — IndexDefinition: add `vectorizers: Vec<VectorizerConfig>` (parsed)
                        FieldDefinition: add `vectorizer: Option<String>` (name reference)
  service/
    mod.rs            — search() extended: parse `kind: "text"` queries, call vectorizer
                        to generate query vector, execute vector search
                        document upload/merge: if vector field omitted and field has
                        vectorizer, generate vector from source text
  api/
    mod.rs            — unchanged
```

The `vectorizer` module is a pure function: `fn text_to_vector(text: &str, dimensions: usize) -> Vec<f32>`. It is called from two sites:

1. **Document indexing** (`service::upload_documents` / `service::merge_documents`): when a vector field with a vectorizer reference has no explicit value in the document.
2. **Query execution** (`service::search`): when a `vectorQueries` entry has `kind: "text"`.

Both sites use the same function, ensuring index-time and query-time consistency.

### Text-to-vector implementation details

```rust
// Pseudocode (actual implementation in source/rust/src/vector/vectorizer.rs)

fn text_to_vector(text: &str, dimensions: usize) -> Vec<f32> {
    let tokens = analyze(text);  // reuse the full-text analyzer: lowercase, stem, stopword removal
    let mut vec = vec![0.0f32; dimensions];
    for token in &tokens {
        let hash = fnv1a_64(token);
        let bucket = (hash % dimensions as u64) as usize;
        let sign = if (hash >> 63) == 0 { 1.0f32 } else { -1.0f32 };
        vec[bucket] += sign;
    }
    let norm = (vec.iter().map(|x| x * x).sum::<f32>()).sqrt();
    if norm > 0.0 {
        vec.iter_mut().for_each(|x| *x /= norm);
    }
    vec
}
```

- FNV-1a 64-bit: `offset_basis = 0xcbf29ce484222325`, `prime = 0x100000001b3`. Pure integer arithmetic, no dependencies.
- The analyzer is the same one used for full-text search (English: lowercase, punctuation split, stopword removal, Porter stemming). This means "running" and "run" produce the same token and thus the same hash contribution.
- Stopwords are removed before hashing (a query of "the" produces a zero vector, same as a stopword-only full-text query matching nothing).

### Document indexing flow (extended)

In `service::upload_documents` (and `merge_documents`):

1. For each document in the batch:
   a. Validate against the schema (existing logic).
   b. For each vector field in the schema:
      - If the document has an explicit value for the field: validate (dimension, finite) and use it.
      - If the document omits the field (or has `null`):
        - If the field has a `vectorizer` reference: generate the vector from the source text (per `sourceContext.fields` or fallback). Store the generated vector in the document's field map.
        - If the field has no `vectorizer` reference: the field is optional (omit from the document; no vector indexed). This is a new leniency: Phase 2.1 required the vector to be present. With a vectorizer, the field can be omitted.
   c. Index the document (full-text + vector) as normal.

Note: for a vector field WITHOUT a vectorizer, omitting the field in a document is now accepted (the document simply has no vector for that field; it will not appear in vector search results for that field). This is a relaxation from Phase 2.1 (which required the vector to be present). Documented as a leniency.

### Query execution flow (extended)

In `service::search`, the `vectorQueries` parsing (existing, Phase 2.1) is extended:

1. For each `vectorQueries` entry:
   - If `kind == "vector"`: existing path (use the supplied `vector`).
   - If `kind == "text"`:
     a. Validate: `text` present and non-empty; `vector` absent; `fields` reference vector fields with vectorizers.
     b. For each field in `fields`:
        - Resolve the field's vectorizer.
        - Generate the query vector: `text_to_vector(text, field.dimensions)`.
        - Execute vector search on that field's index (HNSW or brute-force).
     c. Union per-field hits (best score wins), same as Phase 2.1.
   - If `kind` is anything else: `400 InvalidQuery`.
2. Union across all queries (best score wins), same as Phase 2.1.
3. Hybrid fusion with full-text (if `search` text present), same as Phase 2.1.

## Configuration

No new environment variables. Vectorizer behaviour is driven entirely by the index schema (`vectorizers` array, field `vectorizer` references) and the per-query `vectorQueries` entries.

The text-to-vector function is fixed (FNV-1a hash + analyzer tokenization). There is no configuration for the embedding function. If a different function is needed in the future, it would be a new `kind` value (e.g., `kind: "hash"` vs. a future `kind: "neural"`), but the current implementation is the only one.

## Error handling

New error cases (all `400` with Azure structure):

| Condition | Code | Message pattern |
|-----------|------|-----------------|
| Vectorizer `kind` not `"uri"` | `InvalidIndex` | "Unsupported vectorizer kind 'K'; only 'uri' is supported" |
| Vectorizer missing/empty `name` | `InvalidIndex` | "Vectorizer must have a non-empty name" |
| Duplicate vectorizer name | `InvalidIndex` | "Duplicate vectorizer name 'N'" |
| Vectorizer missing `parameters.uri` | `InvalidIndex` | "Vectorizer 'N' must have a parameters.uri" |
| Vectorizer `sourceContext.sourceType` not `"field"` | `InvalidIndex` | "Unsupported sourceContext sourceType 'T'; only 'field' is supported" |
| Vectorizer `sourceContext.fields` references unknown/non-searchable field | `InvalidIndex` | "Vectorizer 'N' source field 'F' must be a searchable string field" |
| Vector field references unknown vectorizer name | `InvalidIndex` | "Vector field 'F' references unknown vectorizer 'V'" |
| `kind: "text"` query: `text` missing or empty | `InvalidQuery` | "Vectorizer query requires a non-empty 'text' property" |
| `kind: "text"` query: `vector` also present | `InvalidQuery` | "Vectorizer query (kind 'text') must not include a 'vector' property" |
| `kind: "text"` query: field has no vectorizer | `InvalidQuery` | "Vector field 'F' does not have a vectorizer; use kind 'vector' with an explicit vector" |
| `kind: "text"` query: field is not a vector field | `InvalidQuery` | "Vector query field 'F' is not a vector field in index 'I'" (existing, Phase 2.1) |

Previously rejected, now accepted:

- `vectorQueries[].kind: "text"` — accepted (was `400 UnsupportedQuery`).
- `vectorizers` in index config — accepted (was not parsed).

Remaining rejected:

- `vectorQueries[].kind` other than `"vector"` or `"text"` → `400 InvalidQuery`.
- `semantic` + `vectorQueries` (any kind) → `400 InvalidQuery` (Phase 2.3).

## Deliverables

1. `source/rust/src/vector/vectorizer.rs`: `text_to_vector` function (FNV-1a hash, analyzer tokenization, L2 normalization).
2. Index schema validation: `vectorizers` array parsing and validation; field `vectorizer` reference validation.
3. Document indexing extension: vector generation from source text when vector field is omitted and field has a vectorizer.
4. Query execution extension: `kind: "text"` parsing, query vector generation, vector search execution.
5. Hybrid search: `kind: "text"` queries participate in RRF fusion with full-text (same as `kind: "vector"`).
6. Updated `docs/supported_operations.md`: `kind: "text"` flipped from Unsupported to Supported; `vectorizers` documented in the index schema section.
7. Updated `docs/known_differences.md`: hash-based embedding rationale, no external API calls, lexical (not semantic) similarity, zero-vector behaviour.
8. Contract tests: `source/rust/tests/contract/vectorizer_queries.rs`.
9. Unit tests: `text_to_vector` (determinism, dimension correctness, token sensitivity, zero vector, normalization), vectorizer config validation, document indexing with vectorizer.
10. Python SDK compatibility tests: vectorizer index creation, document upload without explicit vectors, `kind: "text"` search, hybrid with vectorizer.
11. C# SDK compatibility tests: mirror Python additions.
12. HTTP fixtures: extend `source/tests/python/fixtures/` with vectorizer wire-format captures.

## Test plan

### Unit tests (`source/rust/src/vector/vectorizer.rs`)

- **Determinism:** same text → same vector (call twice, assert equality).
- **Dimension correctness:** output length equals the `dimensions` parameter for various values (1, 8, 1536, 3072).
- **Token sensitivity:** "hello world" and "hello world" → cosine similarity 1.0; "hello world" and "goodbye moon" → cosine similarity < 0.5; "hello world" and "hello there" → cosine similarity > 0.5 (shared "hello" token).
- **Analyzer consistency:** "Running" and "run" → same vector (stemming); "The cat" and "cat" → same vector (stopword removal).
- **Zero vector:** empty text → all-zero vector; stopword-only text ("the and a") → all-zero vector.
- **Normalization:** non-zero vector has L2 norm of 1.0 (within floating-point tolerance).
- **Sign distribution:** a long random text produces both positive and negative components (signed hashing works).
- **Performance:** vectorizing a 1000-token text to 1536 dimensions completes in < 1ms (sanity check; no hard assertion).

### Contract tests (`source/rust/tests/contract/vectorizer_queries.rs`)

- Create index with vectorizer: valid `vectorizers` array → `201`; missing `kind` → `400`; `kind` not `"uri"` → `400`; missing `parameters.uri` → `400`; duplicate name → `400`; `sourceContext` with unknown field → `400`.
- Create index with vector field referencing vectorizer: valid → `201`; unknown vectorizer name → `400`.
- Upload document without vector field (field has vectorizer): → per-doc `201`; vector generated and stored (verify via `get_document` that the vector field is present with the correct dimension).
- Upload document with explicit vector (field has vectorizer): → per-doc `201`; explicit vector used (not overwritten by generated one).
- Upload document without vector field (field has NO vectorizer): → per-doc `201`; field omitted (no vector indexed).
- `kind: "text"` search: valid → `200` with results; correct ordering (token-overlap similarity); `k` respected.
- `kind: "text"` search: `text` missing → `400`; `vector` also present → `400`; field without vectorizer → `400`.
- `kind: "text"` + `kind: "vector"` mixed in one search: union of results.
- `kind: "text"` hybrid (with `search` text): RRF fusion with full-text results.
- `kind: "text"` with `vectorFilterMode=preFilter`: filter constrains candidates.
- `kind: "text"` with `exhaustive: true`: brute-force path.
- Multiple `kind: "text"` queries: union, best score.
- `sourceContext.fields` determines the text source: document with `title` + `content` → vector generated from both.
- Zero-vector document (empty source text): does not appear in vector search results.
- Paging: `top`/`skip` on vectorizer query results; continuation token works.
- `select` projection: vector field returned (subject to `retrievable`).

### Python SDK compatibility tests

- `SearchIndexClient.create_index` with `vectorizers=[UriVectorizationSource(...)]` and a vector field with `vectorizer_name="..."`.
- `SearchClient.upload_documents` WITHOUT the vector field (vectorizer generates it).
- `SearchClient.search` with `vector_queries=[VectorizedQuery(kind="text", text="...", fields="content_vector", k_nearest_neighbors=10)]`.
- Hybrid: `search_text="..."` + `vector_queries=[VectorizedQuery(kind="text", ...)]`.
- `vector_filter_mode="preFilter"` + `filter="..."` with `kind: "text"`.
- `get_document` returns the generated vector.
- Multiple vectorizer queries in one search.

### C# SDK compatibility tests

- Mirror the Python additions (same scenarios, .NET SDK API: `UriVectorizationSource`, `VectorizedQuery` with `Kind = VectorizedQueryKind.Text`).

### E2E

- Full vectorizer RAG flow: create index with text field + vector field (with vectorizer), upload documents WITHOUT explicit vectors (vectorizer generates them from the text field), search with `kind: "text"` query, verify results are ordered by token-overlap similarity (documents sharing query terms rank higher). Hybrid: `search_text` + `kind: "text"` vectorizer query, verify RRF fusion.

## Known differences (to be documented)

| Behaviour | Emulator | Azure | Rationale |
|-----------|----------|-------|-----------|
| Text-to-vector function | FNV-1a hash + analyzer tokenization (lexical) | Neural embedding model (semantic) | Same interface (text → fixed-dim vector); similarity is lexical (token overlap) not semantic (meaning). Test assertions must use token-overlap expectations, not semantic-similarity expectations. |
| External API call | None (URI stored but not called) | Calls the configured embedding service | No external dependencies; fully self-contained. The URI is a wire-format placeholder. |
| Vector quality | Deterministic, reproducible, corpus-independent | Model-dependent, may change with model updates | Determinism is a feature for test doubles; model version changes are a non-goal. |
| "king" vs. "monarch" | Not similar (no shared tokens) | Similar (semantic embedding) | Hash-based embedding has no distributed representation. Use synonym-aware test data or accept the limitation. |
| "running" vs. "run" | Similar (same stem → same token) | Similar (semantic) | The analyzer's stemming provides basic morphological matching. |
| Stopword-only text | Zero vector (no match) | Model produces a non-zero vector | Stopwords are removed before hashing; a stopword-only text has no tokens. |
| Zero-vector document | Never appears in vector results (zero cosine similarity) | Model produces a non-zero vector even for empty text | Edge case; documents with empty source text are unlikely in practice. |
| `parameters.parameters` | Stored opaquely, not interpreted | Used to configure the embedding model | The emulator does not call the model; the parameters are a wire-format placeholder. |
| Vector field without explicit value (no vectorizer) | Accepted (field omitted, no vector) | Required (must supply vector) | Emulator leniency: omitting a non-vectorizer vector field is accepted (document simply has no vector for that field). |

## Checklist

### Schema and validation

- [ ] `vectorizers` array parsed and validated (name, kind, parameters.uri, sourceContext).
- [ ] `kind` must be `"uri"`; other kinds → `400 InvalidIndex`.
- [ ] `parameters.uri` required and non-empty.
- [ ] `sourceContext.sourceType` must be `"field"`; `fields` must reference searchable string fields.
- [ ] Duplicate vectorizer names → `400 InvalidIndex`.
- [ ] Vector field `vectorizer` property validated (references existing vectorizer).
- [ ] Unknown vectorizer reference on a field → `400 InvalidIndex`.
- [ ] Index without `vectorizers` + `kind: "text"` query → `400 InvalidQuery` (field has no vectorizer).

### Text-to-vector function

- [ ] Deterministic: same text → same vector.
- [ ] Dimensionally correct: output length = `dimensions`.
- [ ] Token-sensitive: shared tokens → higher cosine similarity.
- [ ] Analyzer-consistent: stemming and stopword removal applied.
- [ ] L2-normalized (unit vector) for non-zero input.
- [ ] Zero vector for empty/stopword-only input.
- [ ] Fast: < 1ms for 1000 tokens → 1536 dims.

### Document indexing

- [ ] Vector field with vectorizer + omitted value → vector generated from source text.
- [ ] Vector field with vectorizer + explicit value → explicit value used.
- [ ] Vector field without vectorizer + omitted value → field omitted (no vector).
- [ ] `sourceContext.fields` determines the text source (concatenation in order).
- [ ] No `sourceContext` → fallback to all searchable string fields.
- [ ] Empty source text → zero vector.
- [ ] Generated vector stored in document (returned by `get_document`, subject to `retrievable`).
- [ ] Merge: explicit vector replaces generated; omitted field preserves existing.
- [ ] Delete: vector removed (unchanged from Phase 2.1).

### Query execution

- [ ] `kind: "text"` parsed: `text` required, `vector` must be absent.
- [ ] Query vector generated from `text` using the field's vectorizer dimensions.
- [ ] Vector search executed (HNSW or brute-force per profile/`exhaustive`).
- [ ] `k`, `exhaustive`, `weight` semantics same as `kind: "vector"`.
- [ ] Multiple `kind: "text"` queries: union, best score.
- [ ] Mixed `kind: "text"` + `kind: "vector"`: union.
- [ ] Hybrid with full-text: RRF fusion.
- [ ] `vectorFilterMode` applies.
- [ ] Paging: `top`/`skip`, continuation token.
- [ ] `select` projection includes/excludes vector field.

### Errors

- [ ] All new error cases return correct status code and Azure error structure.
- [ ] `kind: "text"` + `vector` present → `400 InvalidQuery`.
- [ ] `kind: "text"` + field without vectorizer → `400 InvalidQuery`.
- [ ] `semantic` + `vectorQueries` (any kind) → `400 InvalidQuery`.

### Integration

- [ ] Vectorizer + full-text + filter + orderby + select + facets all work.
- [ ] Concurrent vectorizer queries + document uploads do not corrupt state.
- [ ] Existing raw-vector (`kind: "vector"`) searches unaffected.
- [ ] Service reset clears all vector indexes (including generated vectors).

### Tests

- [ ] Unit tests: `text_to_vector` (determinism, dimensions, token sensitivity, zero vector, normalization, performance).
- [ ] Unit tests: vectorizer config validation, document indexing with vectorizer.
- [ ] Contract tests: `source/rust/tests/contract/vectorizer_queries.rs` covers all matrix entries.
- [ ] Python SDK tests: vectorizer index creation, upload without vectors, `kind: "text"` search, hybrid.
- [ ] C# SDK tests: mirror Python additions.
- [ ] Fixtures captured for C# replay.
- [ ] E2E: full vectorizer RAG flow.
- [ ] All existing tests still pass (no regression).

### Quality gates

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] All test suites green (unit, contract, SDK Python, SDK C#, e2e).
- [ ] `docs/supported_operations.md` updated (`kind: "text"` flipped to Supported; `vectorizers` documented).
- [ ] `docs/known_differences.md` updated (hash-based embedding, no external calls, lexical similarity).

## Exit criteria

An application can create an index with a vectorizer configuration and vector fields that reference the vectorizer, upload documents without explicit vectors (the emulator generates them from the source text), and perform `kind: "text"` vectorizer queries (including hybrid with full-text, filtering, and pagination) through the unmodified Python and C# SDKs, with correct response shapes and deterministic token-overlap-based ordering — all covered by contract and SDK tests. The application's vectorizer query code paths (request construction with `kind: "text"`, response parsing, result rendering) exercise the same paths they would against Azure, with embedding-quality differences documented in `known_differences.md`.
