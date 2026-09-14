---
status: draft
status_last_reviewed: 2026-09-14
---

# Phase 2.3 — Semantic Search

## Purpose

Add semantic search support to the emulator so that applications using Azure AI Search's semantic ranking, extractive answers, and captions can run their full query and response-handling code paths against the emulator unmodified. The emulator provides the complete wire format, response shape, and configuration surface; the underlying "intelligence" is approximated with extractive and statistical techniques (no model inference).

## Motivation

Phase 2 explicitly descoped semantic search (`docs/phase_2_production_api.md` §Out of scope), and Phase 2.1 closed the raw-vector gap while leaving semantic queries rejected with `400 UnsupportedQuery`. Applications that use Azure AI Search's semantic capabilities (RAG pipelines with extractive answers, document captioning, semantic reranking) cannot run against the emulator: the search request is rejected before any results are produced. This phase closes that gap by implementing the full semantic configuration and query surface with extractive approximations, so that application code (request construction, response parsing, answer/caption rendering, reranker score display) exercises the same code paths it would against Azure.

The emulator does not perform model inference. Where Azure uses a cross-encoder for reranking or a language model for answer extraction, the emulator uses BM25-based sentence selection and field-content truncation. The response shape is identical; the content quality differs (documented in §Known differences).

## Scope

### Index schema: semantic configuration

The index definition accepts a `semantic` property (Azure wire format):

```json
{
  "semantic": {
    "configurations": [
      {
        "name": "my-semantic-config",
        "priorities": {
          "answers": ["title", "content"],
          "captions": ["summary", "content"]
        },
        "sources": [
          {
            "name": "content-source",
            "type": "text",
            "field": "content"
          },
          {
            "name": "title-source",
            "type": "text",
            "field": "title"
          }
        ],
        "reranker": {
          "name": "standard"
        },
        "rescorers": []
      }
    ]
  }
}
```

Rules (rejected with `400 InvalidIndex`):

- `semantic.configurations` must be a non-empty array.
- Each configuration requires a non-empty `name` (unique within the index).
- `priorities` is optional; when present, `answers` and/or `captions` must be non-empty arrays of field names.
- Every field referenced in `priorities` or `sources` must exist in the index schema and be `searchable: true` (for `sources`) or `retrievable: true` (for `priorities`).
- `sources[].type` must be `"text"` (the only type Azure supports). `sources[].field` must reference a `searchable: true` string field.
- `reranker.name` must be `"standard"` (the only reranker Azure exposes). Missing `reranker` is valid (no reranking).
- `rescorers` is accepted but must be an empty array (Azure's rescorers are preview/limited; non-empty → `400 InvalidIndex`).
- An index without a `semantic` property cannot be searched with `semantic` parameters (→ `400 InvalidQuery`: "index does not have a semantic configuration").
- An index with a `semantic` property can still be searched without semantic parameters (normal full-text/vector path, unchanged).

SDK ↔ wire mapping:

| SDK (`azure-search-documents`) | Wire / emulator storage |
|:---|:---|
| `SemanticConfiguration(name, priorities=..., sources=..., reranker=...)` | `semantic.configurations[]` |
| `SemanticField(priority="answers", field="title")` | `priorities.answers[]` |
| `SemanticField(priority="captions", field="summary")` | `priorities.captions[]` |
| `SemanticSource(name, type="text", field="content")` | `sources[]` |
| `StandardSemanticReranker()` | `reranker: {name: "standard"}` |

### Query: `semantic` parameter

The search request body accepts a `semantic` object (replacing the current `400 UnsupportedQuery` rejection):

```json
{
  "search": "quantum computing",
  "semantic": {
    "semanticConfiguration": "my-semantic-config",
    "queryContext": {
      "questions": ["What is quantum computing?"]
    },
    "answers": {
      "count": 3,
      "type": "extractive"
    },
    "captions": {
      "count": 2,
      "type": "extractive",
      "answers": {
        "count": 1,
        "type": "extractive"
      }
    }
  }
}
```

Field semantics:

- `semanticConfiguration` (required): name of a configuration in the index's `semantic.configurations` array. Unknown name → `400 InvalidQuery`.
- `queryContext` (optional): `questions` is an array of strings providing conversational context. Accepted and stored; used to bias answer selection (see §Answer extraction). Does not affect full-text matching or vector search.
- `answers` (optional):
  - `count`: number of answers to return (1–5, default 3).
  - `type`: must be `"extractive"` (the only type Azure supports). Other values → `400 InvalidQuery`.
- `captions` (optional):
  - `count`: number of captions to return (1–3, default 1).
  - `type`: must be `"extractive"`.
  - `answers` (nested): per-caption answers (same shape as top-level `answers`, default count 1).
- `semanticErrorHandling` (optional): `"throwError"` (default) or `"returnPartialResults"`. When `"returnPartialResults"`, if answer/caption extraction fails for a document, that document is still returned (without the failed component) rather than the entire request failing.
- `semanticMaxWaitInMilliseconds` (optional): accepted but inert (operations are synchronous and fast; no timeout needed). Documented in `known_differences.md`.

Rejected with `400 InvalidQuery`:

- `semantic` present but the index has no `semantic` configuration.
- `semanticConfiguration` references an unknown configuration name.
- `answers.type` or `captions.type` other than `"extractive"`.
- `answers.count` or `captions.count` outside valid range.

### Answer extraction (extractive)

Azure's extractive answers select the most relevant sentence(s) from the document's prioritized fields that answer the query. The emulator approximates this with a BM25-based sentence scorer:

Algorithm:

1. For each result document, collect candidate sentences from the `priorities.answers` fields (in priority order).
2. Split field values into sentences (delimited by `.`, `!`, `?`, `;`, or newline; minimum 20 characters, maximum 500 characters per sentence).
3. Score each sentence against the query terms using a local BM25 computation (the same analyzer as full-text search: lowercase, stem, remove stopwords).
4. Select the top-N sentences (N = `answers.count`) with the highest scores, across all result documents.
5. A sentence is eligible only if its BM25 score against the query is above a threshold (at least one query term must match).
6. If `queryContext.questions` is present, the question text is appended to the query terms for scoring purposes (biasing toward sentences that address the question).

Response shape (per document in `value[]`):

```json
{
  "@search.answers": [
    {
      "score": 0.85,
      "text": "Quantum computing uses quantum bits to process information.",
      "highlights": ["<em>quantum</em> computing uses <em>quantum</em> bits"],
      "source": "content",
      "sentence": 3
    }
  ]
}
```

- `score`: normalized BM25 score for the sentence (0.0–1.0, higher = more relevant). Computed as `sentence_score / max_sentence_score` across all candidates.
- `text`: the full sentence text.
- `highlights`: the sentence with query-term matches wrapped in the request's `highlightPreTag`/`highlightPostTag` (default `<em>`/`</em>`).
- `source`: the field name the sentence was extracted from.
- `sentence`: zero-based index of the sentence within the source field.
- `@search.answers` is present only when `answers` was requested and at least one answer was extracted. Omitted (not null) otherwise.

### Caption extraction (extractive)

Azure's extractive captions return a short summary of the document from its prioritized caption fields. The emulator approximates this with leading-sentence selection:

Algorithm:

1. For each result document, collect candidate text from the `priorities.captions` fields (in priority order).
2. Select the first sentence (or first 200 characters if no sentence boundary exists) from the highest-priority non-empty field.
3. If the selected text exceeds 200 characters, truncate at the last word boundary before 200 characters and append `...`.
4. If `captions.answers` is requested, extract answers from the caption text using the same sentence-scoring algorithm as §Answer extraction (but scoped to the caption text only).

Response shape (per document in `value[]`):

```json
{
  "@search.captions": [
    {
      "text": "This document describes the fundamentals of quantum computing and its applications.",
      "highlights": ["This document describes the fundamentals of <em>quantum</em> <em>computing</em>"],
      "source": "summary",
      "type": "extractive",
      "answers": [
        {
          "score": 0.72,
          "text": "Quantum computing uses quantum bits to process information.",
          "highlights": ["<em>quantum</em> <em>computing</em> uses <em>quantum</em> bits"],
          "source": "summary"
        }
      ]
    }
  ]
}
```

- `text`: the caption text (truncated to ≤200 characters).
- `highlights`: the caption with query-term matches wrapped in tags.
- `source`: the field name the caption was extracted from.
- `type`: always `"extractive"`.
- `answers`: present only when `captions.answers` was requested.
- `@search.captions` is present only when `captions` was requested and at least one caption was extracted. Omitted (not null) otherwise.

### Reranker score

When the semantic configuration includes a `reranker` (or even when it does not — Azure always returns a reranker score for semantic searches), each result document includes:

```json
{
  "@search.reranker": {
    "score": 0.78
  }
}
```

- `score`: a normalized relevance score in [0.0, 1.0]. The emulator computes this as the document's BM25 score (or vector similarity score, for vector/hybrid queries) normalized to the [0, 1] range: `score / (score + 1)` for BM25 (a standard sigmoid-like normalization), or the raw cosine similarity for vector queries (already in [-1, 1], clamped to [0, 1]).
- The reranker score does not change result ordering (ordering is still by the primary score: BM25 or vector similarity). It is an additive diagnostic property.
- `@search.reranker` is present on every document in a semantic search response. Omitted for non-semantic searches.

### Semantic + full-text interaction

- A semantic search (`semantic` parameter present) is always a full-text search underneath: the `search` text is required and must be non-empty (or `"*"` for match-all). The full-text engine produces the candidate set; semantic processing (answers, captions, reranker score) is applied to the results.
- `searchMode`, `searchFields`, `filter`, `orderby`, `select`, `facets`, `highlight` all work as in normal full-text search.
- `semantic` is incompatible with `vectorQueries`: if both are present → `400 InvalidQuery` ("semantic search and vector queries cannot be combined"). (Azure does not support this combination either.)
- `semantic` is incompatible with `queryType=full`: → `400 InvalidQuery`. (Azure's semantic search uses the simple query parser.)
- `minimumCoverage` (Phase 2.2) applies to the underlying full-text match before semantic processing.

### Semantic + hybrid (full-text only, no vector)

- Semantic search is full-text only. There is no "semantic + vector" hybrid in Azure. The `semantic` parameter and `vectorQueries` are mutually exclusive.

### Response shape (complete semantic search)

```json
{
  "@odata.context": "/$metadata#documents",
  "@odata.count": 2,
  "value": [
    {
      "@search.score": 0.85,
      "@search.reranker": { "score": 0.78 },
      "@search.answers": [
        {
          "score": 0.91,
          "text": "Quantum computing leverages superposition and entanglement.",
          "highlights": ["<em>Quantum</em> <em>computing</em> leverages superposition"],
          "source": "content",
          "sentence": 2
        }
      ],
      "@search.captions": [
        {
          "text": "An overview of quantum computing principles and applications.",
          "highlights": ["An overview of <em>quantum</em> <em>computing</em> principles"],
          "source": "summary",
          "type": "extractive"
        }
      ],
      "@search.highlights": {
        "title": ["<em>Quantum</em> <em>Computing</em> Fundamentals"]
      },
      "id": "1",
      "title": "Quantum Computing Fundamentals",
      "content": "Quantum computing leverages superposition and entanglement to process information in parallel...",
      "summary": "An overview of quantum computing principles and applications."
    }
  ]
}
```

- `@search.score`: the primary BM25 score (unchanged from non-semantic search).
- `@search.reranker.score`: the normalized reranker score (additive).
- `@search.answers`: present only when requested and at least one answer extracted.
- `@search.captions`: present only when requested and at least one caption extracted.
- `@search.highlights`: present when `highlight` was requested (same as non-semantic).
- All properties are omitted (not null) when not applicable.

### `semanticErrorHandling`

- `"throwError"` (default): if answer or caption extraction encounters an error (e.g., a prioritized field is missing from a document), the entire search returns `400 InvalidQuery` with a descriptive message.
- `"returnPartialResults"`: the document is still returned; the failed component (`@search.answers` or `@search.captions`) is simply absent for that document. Other documents are unaffected.

In practice, extraction failures are rare (a missing field simply yields no candidates for that document), so `"throwError"` rarely triggers. The distinction matters for SDK error-handling code paths.

### What is NOT in scope

- **Generative answers/captions** — Azure's `type: "extractive"` is the only supported type; there is no generative mode in the stable API. (The emulator implements extractive only, matching Azure's stable surface.)
- **Cross-encoder reranking** — Azure's `standard` reranker uses a neural cross-encoder. The emulator uses BM25 normalization. The score is present and in the correct range, but the values differ from Azure.
- **Query rewriting/expansion** — Azure's semantic pipeline may rewrite the query using a language model. The emulator does not rewrite queries; the original search text is used as-is for full-text matching.
- **`queryContext` conversational AI** — The `questions` array is used to bias answer selection (appended to query terms for scoring) but does not trigger multi-turn conversation or context-aware rewriting.
- **Rescorers** — Accepted but must be empty (Azure's rescorers are limited/preview).
- **Semantic search with vector queries** — Mutually exclusive (matching Azure).
- **Semantic search with `queryType=full`** — Rejected (matching Azure).
- **Model inference of any kind** — No external API calls, no local models. All "intelligence" is statistical/extractive.

## Architecture integration

```
source/rust/src/
  semantic/           — NEW: semantic search processing
    mod.rs            — SemanticConfig (parsed from index schema), SemanticQuery (parsed from request)
    answers.rs        — Sentence splitting, BM25 sentence scoring, answer selection
    captions.rs       — Caption extraction (leading-sentence selection, truncation)
    reranker.rs       — Score normalization
  query/
    mod.rs            — unchanged (full-text engine produces candidates)
  service/
    mod.rs            — search() extended: parse `semantic` parameter, validate against
                        index config, call full-text engine, apply semantic processing
                        (answers, captions, reranker) to results, build response
  storage/
    mod.rs            — IndexDefinition: add `semantic: Option<SemanticConfig>` (parsed)
  api/
    mod.rs            — unchanged (search handler passes body to service)
```

The `semantic` module is a post-processing layer: it receives the full-text result set (scored documents) and produces the semantic response components. It does not modify the full-text engine or the vector engine.

Processing pipeline (in `service::search`):

1. Parse and validate the `semantic` parameter against the index's `semantic` configuration.
2. Execute the full-text search (existing path) to get scored candidates.
3. Apply `filter`, `orderby`, `select`, `facets`, `highlight` (existing path).
4. If `semantic` is present:
   a. Compute reranker scores for all results.
   b. If `answers` requested: run answer extraction across all results, select top-N globally.
   c. If `captions` requested: run caption extraction per document.
   d. Attach `@search.reranker`, `@search.answers`, `@search.captions` to result documents.
5. Apply `top`/`skip` pagination (after semantic processing, so answers/captions are computed for the full result set before paging — matching Azure, where answers are selected from the full result set).
6. Build response.

Note: answers are selected from the full filtered result set (before `top`/`skip`), so the top answers may come from documents beyond the first page. This matches Azure's behaviour.

### Sentence splitting

- Delimiters: `.`, `!`, `?`, `;`, `\n` (each followed by whitespace or end-of-string).
- Minimum sentence length: 20 characters (shorter fragments are merged with the next sentence).
- Maximum sentence length: 500 characters (longer sentences are split at the last word boundary before 500).
- Abbreviations (e.g., "Dr.", "e.g.", "i.e.") are not special-cased (documented difference; may produce slightly different sentence boundaries than Azure).

## Configuration

No new environment variables. Semantic behaviour is driven entirely by the index schema (`semantic.configurations`) and the per-query `semantic` parameter.

Optional: `EMULATOR_SEMANTIC__MAX_ANSWERS` (default `5`) and `EMULATOR_SEMANTIC__MAX_CAPTIONS` (default `3`) to cap the maximum `count` values accepted, useful for memory-constrained environments.

## Error handling

New error cases (all `400` with Azure structure):

| Condition | Code | Message pattern |
|-----------|------|-----------------|
| Index has no `semantic` property but query includes `semantic` | `InvalidQuery` | "Index 'X' does not have a semantic configuration" |
| `semanticConfiguration` references unknown name | `InvalidQuery` | "Unknown semantic configuration 'Y' in index 'X'" |
| `semantic` + `vectorQueries` both present | `InvalidQuery` | "Semantic search and vector queries cannot be combined" |
| `semantic` + `queryType=full` | `InvalidQuery` | "Semantic search requires queryType 'simple'" |
| `answers.type` / `captions.type` not `"extractive"` | `InvalidQuery` | "Unsupported semantic answer/caption type 'T'; only 'extractive' is supported" |
| `answers.count` / `captions.count` out of range | `InvalidQuery` | "Semantic answers count must be 1-5" / "Semantic captions count must be 1-3" |
| `semanticErrorHandling` not `"throwError"` / `"returnPartialResults"` | `InvalidQuery` | "Invalid semanticErrorHandling 'H'" |
| Index schema: `semantic.configurations` empty | `InvalidIndex` | "Semantic configuration must have at least one entry" |
| Index schema: duplicate configuration name | `InvalidIndex` | "Duplicate semantic configuration name 'N'" |
| Index schema: `sources[].field` not searchable | `InvalidIndex` | "Semantic source field 'F' must be searchable" |
| Index schema: `priorities` field not retrievable | `InvalidIndex` | "Semantic priority field 'F' must be retrievable" |
| Index schema: `reranker.name` not `"standard"` | `InvalidIndex` | "Unknown reranker 'R'; only 'standard' is supported" |
| Index schema: `rescorers` non-empty | `InvalidIndex` | "Rescorers are not supported" |

Previously rejected options now accepted (removed from `400 UnsupportedQuery` list):

- `semantic`, `semanticConfiguration`, `semanticQuery`, `semanticErrorHandling`, `semanticMaxWaitInMilliseconds` — all accepted.
- `answers`, `captions` — accepted (as sub-properties of `semantic`).

Remaining rejected options (still `400 UnsupportedQuery`):

- `scoringProfile`, `scoringParameters`, `scoringStatistics` (scoring profiles — separate concern).
- `debug` (moved to Phase 2.2).

## Deliverables

1. `source/rust/src/semantic/` module: `SemanticConfig` parser, sentence splitter, BM25 sentence scorer, answer selector, caption extractor, reranker score normalizer.
2. Index schema validation: `semantic` property parsing and validation (configurations, priorities, sources, reranker, rescorers).
3. Search path extension: `semantic` parameter parsing, validation against index config, semantic post-processing pipeline.
4. Response shape: `@search.reranker`, `@search.answers`, `@search.captions` properties.
5. `semanticErrorHandling` support (`throwError` / `returnPartialResults`).
6. Updated `docs/supported_operations.md`: semantic search flipped from Unsupported to Supported; `answers`/`captions`/`semantic*` removed from the rejected list.
7. Updated `docs/known_differences.md`: extractive approximation rationale, no query rewriting, BM25 reranker vs cross-encoder, sentence-splitting differences.
8. Contract tests: `source/rust/tests/contract/semantic_search.rs`.
9. Unit tests: sentence splitting, BM25 sentence scoring, answer selection, caption extraction, reranker normalization, config validation.
10. Python SDK compatibility tests: semantic index creation, semantic search with answers/captions, `queryContext`, `semanticErrorHandling`.
11. C# SDK compatibility tests: mirror Python additions.
12. HTTP fixtures: extend `source/tests/python/fixtures/` with semantic search wire-format captures.

## Test plan

### Unit tests (`source/rust/src/semantic/`)

- **Sentence splitting:** standard sentences, multiple delimiters, short-fragment merging, long-sentence truncation, empty input, single-sentence input, Unicode text.
- **BM25 sentence scoring:** known query + sentences → expected ranking; no-match sentences score 0; stopword-only sentences score 0.
- **Answer selection:** top-N across multiple documents; threshold filtering; `queryContext` bias; single-document case; no-eligible-sentences case.
- **Caption extraction:** first-sentence selection; 200-char truncation at word boundary; empty field fallback to next priority; nested `captions.answers`.
- **Reranker normalization:** BM25 score → [0,1] mapping; vector cosine → [0,1] clamp; zero-score document.
- **Config validation:** all error cases (empty configurations, duplicate names, non-searchable source fields, non-retrievable priority fields, unknown reranker, non-empty rescorers).

### Contract tests (`source/rust/tests/contract/semantic_search.rs`)

- Create index with semantic configuration: valid → `201`; missing `semantic` → search with `semantic` → `400`; unknown configuration name → `400`; invalid schema (non-searchable source, duplicate config name, bad reranker) → `400 InvalidIndex`.
- Semantic search (basic): `search` + `semantic.semanticConfiguration` → `200` with `@search.reranker` on all documents.
- Semantic search with answers: `semantic.answers.count=3` → `@search.answers` present with ≤3 entries; correct shape (score, text, highlights, source, sentence).
- Semantic search with captions: `semantic.captions.count=2` → `@search.captions` present with ≤2 entries; correct shape (text, highlights, source, type).
- Semantic search with `captions.answers`: nested answers present in caption objects.
- `queryContext.questions`: accepted; affects answer selection (answers biased toward question terms).
- `semanticErrorHandling=returnPartialResults`: document with missing priority field still returned (without answers/captions for that doc).
- `semantic` + `vectorQueries` → `400 InvalidQuery`.
- `semantic` + `queryType=full` → `400 InvalidQuery`.
- `answers.type` other than `"extractive"` → `400 InvalidQuery`.
- `answers.count` out of range (0, 6, non-integer) → `400 InvalidQuery`.
- Non-semantic search on a semantic index: no `@search.reranker`/`@search.answers`/`@search.captions` in response.
- Semantic search with `filter`, `orderby`, `select`, `facets`, `highlight`: all work in combination.
- Semantic search with `top`/`skip`: answers selected from full set before paging.
- `@search.answers`/`@search.captions` omitted (not null) when not requested or no candidates.

### Python SDK compatibility tests

- `SearchIndexClient.create_index` with `semantic=Semantic(search_configurations=[SemanticConfiguration(...)])`.
- `SearchClient.search` with `semantic=SemanticQuery(semantic_configuration="...", answers=SemanticAnswer(count=3), captions=SemanticCaption(count=2))`.
- `query_context=QueryContext(questions=["..."])`.
- `semantic_error_handling="returnPartialResults"`.
- Response parsing: `result.get_answers()`, `result.get_captions()`, `result.reranker_score` (SDK model properties).
- Semantic + filter + orderby + select combination.
- Semantic index without semantic query: normal results, no semantic properties.

### C# SDK compatibility tests

- Mirror the Python additions (same scenarios, .NET SDK API: `SemanticConfiguration`, `SemanticQuery`, `SemanticAnswer`, `SemanticCaption`).

### E2E

- Full RAG-style semantic flow: create index with semantic config (priorities for answers + captions, sources), upload documents with title/content/summary fields, search with semantic + answers + captions + queryContext, verify response shape and content (answers are actual sentences from the documents, captions are truncated field content, reranker scores in [0,1]).

## Known differences (to be documented)

| Behaviour | Emulator | Azure | Rationale |
|-----------|----------|-------|-----------|
| Answer extraction | BM25 sentence scoring (extractive) | Neural extractive model (sentence selection + abstractive rewriting) | Same approach (extract a sentence from the document); exact sentence selection and wording differ. Test assertions must check that the answer is a substring of the source field, not exact text. |
| Caption extraction | First sentence / 200-char truncation | Neural summarization (extractive) | Same approach (short text from the document); exact content differs. Test assertions must check length and source field, not exact text. |
| Reranker score | BM25 sigmoid normalization / cosine clamp | Cross-encoder neural score | Same range [0,1], same direction (higher = more relevant). Exact values differ. Test assertions must check range and ordering correlation, not equality. |
| Query rewriting | None (original query used as-is) | Language-model query expansion/rewriting | The emulator does not rewrite queries. Answer/caption selection uses the original query terms. |
| `queryContext` | Biases answer scoring (appended to query terms) | Full conversational context (multi-turn, coreference resolution) | The emulator uses the question text as additional scoring terms; no multi-turn state. |
| Sentence splitting | Punctuation-delimited, 20-500 char bounds | Azure's internal sentence segmentation | May differ on edge cases (abbreviations, lists, code blocks). |
| `semanticMaxWaitInMilliseconds` | Accepted but inert | Actual timeout for model inference | Operations are synchronous and fast; no timeout needed. |
| `rescorers` | Must be empty | Supports limited rescorers (preview) | Preview feature; not needed for test doubles. |

## Checklist

### Schema and validation

- [ ] `semantic.configurations` parsed and validated (name, priorities, sources, reranker, rescorers).
- [ ] Priority fields must be `retrievable: true`; source fields must be `searchable: true`.
- [ ] `reranker.name` must be `"standard"`; `rescorers` must be empty.
- [ ] Duplicate configuration names → `400 InvalidIndex`.
- [ ] Index without `semantic` + query with `semantic` → `400 InvalidQuery`.
- [ ] Unknown `semanticConfiguration` name → `400 InvalidQuery`.

### Query parsing

- [ ] `semantic` parameter parsed: `semanticConfiguration`, `queryContext`, `answers`, `captions`.
- [ ] `semanticErrorHandling` parsed (`throwError` / `returnPartialResults`).
- [ ] `semanticMaxWaitInMilliseconds` accepted (inert).
- [ ] `answers.count` / `captions.count` validated (range, integer).
- [ ] `answers.type` / `captions.type` must be `"extractive"`.
- [ ] `semantic` + `vectorQueries` → `400 InvalidQuery`.
- [ ] `semantic` + `queryType=full` → `400 InvalidQuery`.

### Answer extraction

- [ ] Sentences extracted from `priorities.answers` fields (in priority order).
- [ ] BM25 sentence scoring against query terms.
- [ ] Top-N selection across all result documents.
- [ ] Threshold: at least one query term must match.
- [ ] `queryContext.questions` appended to query terms for scoring.
- [ ] Response shape: `score`, `text`, `highlights`, `source`, `sentence`.
- [ ] `@search.answers` omitted when not requested or no candidates.

### Caption extraction

- [ ] First sentence / 200-char truncation from `priorities.captions` fields.
- [ ] Truncation at word boundary + `...`.
- [ ] Fallback to next priority field if current is empty.
- [ ] Nested `captions.answers` extraction.
- [ ] Response shape: `text`, `highlights`, `source`, `type`, `answers`.
- [ ] `@search.captions` omitted when not requested or no candidates.

### Reranker

- [ ] `@search.reranker.score` present on all documents in semantic search.
- [ ] BM25 → [0,1] via `score / (score + 1)`.
- [ ] Vector cosine → [0,1] clamp.
- [ ] Does not affect result ordering.
- [ ] Omitted for non-semantic searches.

### Integration

- [ ] Semantic + filter + orderby + select + facets + highlight all work.
- [ ] `top`/`skip` applied after semantic processing (answers from full set).
- [ ] `count=true` reflects the full-text result set (not affected by semantic).
- [ ] `semanticErrorHandling=returnPartialResults`: partial results on extraction failure.
- [ ] Non-semantic search on semantic index: unchanged behaviour.
- [ ] Concurrent semantic searches do not corrupt state.

### Errors

- [ ] All new error cases return correct status code and Azure error structure.
- [ ] `scoringProfile`/`scoringParameters`/`scoringStatistics` still rejected with `400 UnsupportedQuery`.
- [ ] Vectorizer queries (`kind: "text"`) still rejected (Phase 2.4).

### Tests

- [ ] Unit tests: sentence splitting, scoring, answer selection, caption extraction, reranker, config validation.
- [ ] Contract tests: `source/rust/tests/contract/semantic_search.rs` covers all matrix entries.
- [ ] Python SDK tests: semantic index creation, search with answers/captions/queryContext/errorHandling.
- [ ] C# SDK tests: mirror Python additions.
- [ ] Fixtures captured for C# replay.
- [ ] E2E: full RAG-style semantic flow.
- [ ] All existing tests still pass (no regression).

### Quality gates

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] All test suites green (unit, contract, SDK Python, SDK C#, e2e).
- [ ] `docs/supported_operations.md` updated (semantic flipped to Supported; `answers`/`captions`/`semantic*` removed from rejected list).
- [ ] `docs/known_differences.md` updated (extractive approximation, no query rewriting, BM25 reranker).

## Exit criteria

An application can create an index with a semantic configuration, upload documents, and perform semantic search with extractive answers, captions, query context, and reranker scores through the unmodified Python and C# SDKs, with the correct response shape (`@search.reranker`, `@search.answers`, `@search.captions`) and all semantic options accepted — all covered by contract and SDK tests. The application's response-parsing code (answer rendering, caption display, reranker score logging) exercises the same paths it would against Azure, with content quality differences documented in `known_differences.md`.
