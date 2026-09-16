---
status: complete
status_last_reviewed: 2026-09-16
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

SDK ↔ wire mapping (pinned SDKs: `azure-search-documents==12.0.0` Python,
`Azure.Search.Documents==12.0.0` .NET):

| SDK (Python / .NET 12.0.0) | Wire / emulator storage |
|:---|:---|
| `SemanticSearch(configurations=[...])` / `SemanticSearch { Configurations = {...} }` | `semantic` |
| `SemanticConfiguration(name, prioritized_fields, ranking_order)` / `SemanticConfiguration { Name, PrioritizedFields, RankingOrder }` | `semantic.configurations[]` |
| `SemanticPrioritizedFields(title_field, content_fields, keywords_fields)` | `priorities` + `sources` (see mapping below) |
| `SemanticField(field_name=...)` / `SemanticField { FieldName }` | a field-name entry |

The pinned 12.0.0 SDKs do not expose `priorities`/`sources` directly; they send
the `prioritizedFields` wire format:

```json
{
  "name": "my-semantic-config",
  "prioritizedFields": {
    "titleField": {"fieldName": "title"},
    "prioritizedContentFields": [{"fieldName": "content"}],
    "prioritizedKeywordsFields": [{"fieldName": "keywords"}]
  },
  "rankingOrder": "BoostedRerankerScore"
}
```

The emulator accepts **both** index-schema formats and normalizes the SDK
format to the canonical form internally:

| SDK `prioritizedFields` | Canonical form |
|:---|:---|
| `titleField` | `priorities.answers` + `priorities.captions`, context role |
| `prioritizedContentFields[]` | `priorities.answers` + `priorities.captions`, plus a `sources[]` entry per field |
| `prioritizedKeywordsFields[]` | `priorities.answers`, plus a `sources[]` entry per field |
| `rankingOrder` (any value) | `reranker: {name: "standard"}` |

Validation of SDK-format configurations is lenient: referenced fields must
exist in the index schema (same existence rule as the canonical form); the
`searchable`/`retrievable` attribute checks apply to the canonical form only.
`defaultConfiguration` / `DefaultConfigurationName` (index-level default) is
accepted but inert: every semantic query must still name its configuration
explicitly.

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
- `semanticErrorHandling` (optional): `"throwError"` (default) or `"returnPartialResults"`. Parsed and validated but inert (see §`semanticErrorHandling`).
- `semanticMaxWaitInMilliseconds` (optional): accepted but inert (operations are synchronous and fast; no timeout needed). Documented in `known_differences.md`.

Rejected with `400 InvalidQuery`:

- `semantic` present but the index has no `semantic` configuration.
- `semanticConfiguration` references an unknown configuration name.
- `answers.type` or `captions.type` other than `"extractive"`.
- `answers.count` or `captions.count` outside valid range.

### Query: SDK flat format (pinned SDKs 12.0.0)

The pinned 12.0.0 SDKs do not send the nested `semantic` object; they send
flat top-level search parameters with `queryType: "semantic"`. The exact body
the Python SDK emits for `search(query_type="semantic",
semantic_configuration_name="...", query_answer="extractive",
query_answer_count=3, query_answer_threshold=0.7,
query_caption="extractive", query_caption_highlight_enabled=True,
semantic_error_mode="fail", semantic_max_wait_in_milliseconds=500,
semantic_query="...")` is:

```json
{
  "search": "quantum computing",
  "queryType": "semantic",
  "semanticConfiguration": "my-semantic-config",
  "semanticErrorHandling": "fail",
  "semanticMaxWaitInMilliseconds": 500,
  "semanticQuery": "quantum?",
  "answers": "extractive|count-3,threshold-0.7",
  "captions": "extractive|highlight-true"
}
```

Note the configuration key is `semanticConfiguration` (the SDK's
`semantic_configuration_name` / `SemanticConfigurationName` property
serializes to this name), and `answers` / `captions` are **compound strings**
(`"<type>"`, `"<type>|count-N"`, `"<type>|count-N,threshold-T"`,
`"<type>|highlight-true|false"`), not objects. The .NET SDK emits the same
keys.

The emulator accepts **both** query formats. The flat format maps to the
canonical `semantic` object as follows:

| SDK flat parameter (Python / .NET) | Canonical equivalent |
|:---|:---|
| `query_type="semantic"` / `QueryType = Semantic` | selects the semantic pipeline (same as a present `semantic` object) |
| `semantic_configuration_name` / `SemanticConfigurationName` (wire: `semanticConfiguration`) | `semantic.semanticConfiguration` (required with `queryType: "semantic"`) |
| `semantic_query` / `SemanticQuery` (a separate query text for the semantic phase) | accepted but inert (answer/caption scoring uses the main `search` text) |
| `query_answer="extractive"` (wire: `answers: "extractive[|...]"`) | `semantic.answers` present |
| `query_answer_count=N` (wire: `answers: "extractive\|count-N"`) | `semantic.answers.count` (clamped to 1–5; default 3) |
| `query_answer_threshold` (wire: `answers: "...,threshold-T"`) | accepted but inert |
| `query_caption="extractive"` (wire: `captions: "extractive[|...]"`) | `semantic.captions` present (default count 1) |
| `query_caption_highlight_enabled` (wire: `captions: "extractive\|highlight-..."`) | accepted but inert (caption highlights are always produced) |
| `semantic_error_mode="fail"` / `"partial"` (wire: `semanticErrorHandling`) | `semanticErrorHandling` — both value sets accepted: `fail`/`partial` (flat) and `throwError`/`returnPartialResults` (nested); inert either way |
| `semantic_max_wait_in_milliseconds` / `MaxWait` | accepted but inert |

The legacy `queryAnswer` / `queryAnswerCount` / `queryCaption` properties are
also accepted as a fallback (the `answers` / `captions` compound strings take
precedence when present).

`queryType: "semantic"` without a `semanticConfiguration` →
`400 InvalidQuery`. `answers`/`captions` values other than
`"extractive"`/`"none"` are treated as absent (no answers/captions requested).
Top-level `semanticQuery`, `semanticErrorHandling`,
`semanticMaxWaitInMilliseconds` are accepted (the latter two inert).

### Answer extraction (extractive)

Azure's extractive answers select the most relevant sentence(s) from the document's prioritized fields that answer the query. The emulator approximates this with a BM25-based sentence scorer:

Algorithm:

1. For each result document, collect candidate sentences from the `priorities.answers` fields (in priority order).
2. Split field values into sentences (delimited by `.`, `!`, `?`, `;`, or newline; minimum 20 characters, maximum 500 characters per sentence).
3. Score each sentence against the query terms using a local BM25 computation (the same analyzer as full-text search: lowercase, stem, remove stopwords).
4. Select the top-N sentences (N = `answers.count`) with the highest scores, across all result documents.
5. A sentence is eligible only if its BM25 score against the query is above a threshold (at least one query term must match).
6. If `queryContext.questions` is present, the question text is appended to the query terms for scoring purposes (biasing toward sentences that address the question).

Response shape (top-level in the search response — the shape the SDKs
deserialize into `SearchDocumentsResult.answers` / `SearchResults.Answers`):

```json
{
  "@search.answers": [
    {
      "score": 0.85,
      "key": "2",
      "text": "Quantum computing uses quantum bits to process information.",
      "highlights": "<em>Quantum</em> computing uses <em>quantum</em> bits to process information."
    }
  ]
}
```

- `score`: normalized BM25 score for the sentence (0.0–1.0, higher = more relevant). Computed as `sentence_score / max_sentence_score` across the selected candidates.
- `key`: the key of the document the answer was extracted from.
- `text`: the full sentence text.
- `highlights`: the sentence with query-term matches wrapped in the request's `highlightPreTag`/`highlightPostTag` (default `<em>`/`</em>`), as a single string.
- `@search.answers` is present only when `answers` was requested and at least one answer was extracted. Omitted (not null) otherwise.

### Caption extraction (extractive)

Azure's extractive captions return a short summary of the document from its prioritized caption fields. The emulator approximates this with leading-sentence selection:

Algorithm:

1. For each result document, collect candidate text from the `priorities.captions` fields (in priority order).
2. Select the first sentence (or first 200 characters if no sentence boundary exists) from the highest-priority non-empty field.
3. If the selected text exceeds 200 characters, truncate at the last word boundary before 200 characters and append `...`.
4. If `captions.answers` is requested, extract answers from the caption text using the same sentence-scoring algorithm as §Answer extraction (but scoped to the caption text only).

Response shape (per document in `value[]` — the shape the SDKs deserialize
into `SearchResult.captions` / `SearchResult<T>.Captions`):

```json
{
  "@search.captions": [
    {
      "text": "This document describes the fundamentals of quantum computing and its applications.",
      "highlights": "This document describes the fundamentals of <em>quantum</em> <em>computing</em> and its applications.",
      "answers": [
        {
          "score": 0.72,
          "text": "Quantum computing uses quantum bits to process information.",
          "highlights": "<em>Quantum</em> <em>computing</em> uses <em>quantum</em> bits to process information."
        }
      ]
    }
  ]
}
```

- `text`: the caption text (truncated to ≤200 characters).
- `highlights`: the caption with query-term matches wrapped in tags, as a single string.
- `answers`: present only when `captions.answers` was requested; each entry carries `score`, `text`, and `highlights`.
- `@search.captions` is present only when `captions` was requested and at least one caption was extracted. Omitted (not null) otherwise.

### Reranker score

Azure always returns a reranker score for semantic searches. Each result document includes:

```json
{
  "@search.rerankerScore": 0.78
}
```

- `@search.rerankerScore`: a bare-number normalized relevance score in [0.0, 1.0]. The emulator computes this as the document's BM25 score normalized to the [0, 1] range: `score / (score + 1)` (a standard sigmoid-like normalization). It is the property the official SDKs deserialize into their models (Python `SearchResult.reranker_score`, .NET `SearchResult<T>.RerankerScore`).
- The reranker score does not change result ordering (ordering is still by the primary BM25 score). It is an additive diagnostic property.
- `@search.rerankerScore` is present on every document in a semantic search response. Omitted for non-semantic searches.

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
  "@search.answers": [
    {
      "score": 0.91,
      "key": "1",
      "text": "Quantum computing leverages superposition and entanglement.",
      "highlights": "<em>Quantum</em> <em>computing</em> leverages superposition and entanglement."
    }
  ],
  "value": [
    {
      "@search.score": 0.85,
      "@search.rerankerScore": 0.78,
      "@search.captions": [
        {
          "text": "An overview of quantum computing principles and applications.",
          "highlights": "An overview of <em>quantum</em> <em>computing</em> principles and applications."
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
- `@search.rerankerScore`: the normalized reranker score (additive), on every document.
- `@search.answers`: top-level; present only when requested and at least one answer extracted.
- `@search.captions`: per-document; present only when requested and at least one caption extracted.
- `@search.highlights`: present when `highlight` was requested (same as non-semantic).
- All properties are omitted (not null) when not applicable.

### `semanticErrorHandling`

- `"throwError"` (default) / `"returnPartialResults"` (flat: `fail` / `partial`): parsed and validated (unknown values → `400 InvalidQuery`) but **inert** — the emulator's extractive pipeline does not fail in the ways Azure's model-based pipeline can (a missing prioritized field simply yields no candidates for that document), so both modes behave identically. Documented in `known_differences.md`.

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
    d. Attach `@search.rerankerScore`, `@search.answers`, `@search.captions` to result documents.
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

## Deliverables

1. `source/rust/src/semantic/` module: `SemanticConfig` parser, sentence splitter, BM25 sentence scorer, answer selector, caption extractor, reranker score normalizer.
2. Index schema validation: `semantic` property parsing and validation (configurations, priorities, sources, reranker, rescorers).
3. Search path extension: `semantic` parameter parsing, validation against index config, semantic post-processing pipeline.
4. Response shape: `@search.rerankerScore`, `@search.answers`, `@search.captions` properties.
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
- Semantic search (basic): `search` + `semantic.semanticConfiguration` → `200` with `@search.rerankerScore` on all documents.
- Semantic search with answers: `semantic.answers.count=3` → `@search.answers` present with ≤3 entries; correct shape (score, key, text, highlights).
- Semantic search with captions: `semantic.captions.count=2` → `@search.captions` present with ≤2 entries; correct shape (text, highlights, answers).
- Semantic search with `captions.answers`: nested answers present in caption objects.
- `queryContext.questions`: accepted; affects answer selection (answers biased toward question terms).
- `semanticErrorHandling=returnPartialResults`: document with missing priority field still returned (without answers/captions for that doc).
- `semantic` + `vectorQueries` → `400 InvalidQuery`.
- `semantic` + `queryType=full` → `400 InvalidQuery`.
- `answers.type` other than `"extractive"` → `400 InvalidQuery`.
- `answers.count` out of range (0, 6, non-integer) → `400 InvalidQuery`.
- Non-semantic search on a semantic index: no `@search.rerankerScore`/`@search.answers`/`@search.captions` in response.
- Semantic search with `filter`, `orderby`, `select`, `facets`, `highlight`: all work in combination.
- Semantic search with `top`/`skip`: answers selected from full set before paging.
- `@search.answers`/`@search.captions` omitted (not null) when not requested or no candidates.

### Python SDK compatibility tests (`azure-search-documents==12.0.0`)

- `SearchIndexClient.create_index` with
  `semantic_search=SemanticSearch(configurations=[SemanticConfiguration(name=...,
  prioritized_fields=SemanticPrioritizedFields(title_field=SemanticField(...),
  content_fields=[SemanticField(...)]))])`.
- `SearchClient.search` with `query_type="semantic"`,
  `semantic_configuration_name="..."`, `query_answer="extractive"`,
  `query_answer_count=3`, `query_caption="extractive"`,
  `semantic_error_mode="fail"`.
- Response parsing: `SearchResult.captions` and `SearchResult.reranker_score`
  (SDK model properties populated from `@search.captions` /
  `@search.rerankerScore`); per-document `@search.answers` and the
  `@search.reranker` object asserted over raw HTTP (the pinned SDK has no
  model property for them — the same raw-HTTP pattern used for the
  bare-number `$count` facet).
- Canonical nested-`semantic`-object queries (with `semanticConfiguration`,
  `answers.count`, `captions.count`, `queryContext.questions`,
  `semanticErrorHandling`) asserted over raw HTTP.
- Semantic + filter + orderby + select combination.
- Semantic index without semantic query: normal results, no semantic properties.
- Error cases: unknown configuration name, `semantic` + `vectorQueries`,
  `semantic` + `queryType=full`, bad `answers.type`/`count` → `400 InvalidQuery`.

### C# SDK compatibility tests (`Azure.Search.Documents==12.0.0`)

- Mirror the Python additions via the .NET API: `SemanticSearch {
  Configurations = { new SemanticConfiguration(name) { PrioritizedFields = new
  SemanticPrioritizedFields(...) } } }` on the index;
  `SearchOptions { QueryType = SearchQueryType.Semantic, SemanticSearch = new
  SemanticSearchOptions { SemanticConfigurationName = "...", QueryAnswer =
  QueryAnswerType.Extractive, QueryCaption = QueryCaptionType.Extractive } }`
  on the query.
- Response parsing: `SearchResult<T>.Highlights` analogues —
  `SearchResult<T>.SemanticSearch.Captions` / `.RerankerScore` where the SDK
   populates them; `@search.answers` / `@search.rerankerScore` asserted over raw HTTP.

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

- [x] `semantic.configurations` parsed and validated (name, priorities, sources, reranker, rescorers).
- [x] Priority fields must be `retrievable: true`; source fields must be `searchable: true`.
- [x] `reranker.name` must be `"standard"`; `rescorers` must be empty.
- [x] Duplicate configuration names → `400 InvalidIndex`.
- [x] Index without `semantic` + query with `semantic` → `400 InvalidQuery`.
- [x] Unknown `semanticConfiguration` name → `400 InvalidQuery`.

### Query parsing

- [x] `semantic` parameter parsed: `semanticConfiguration`, `queryContext`, `answers`, `captions`.
- [x] `semanticErrorHandling` parsed (`throwError` / `returnPartialResults`).
- [x] `semanticMaxWaitInMilliseconds` accepted (inert).
- [x] `answers.count` / `captions.count` validated (range, integer).
- [x] `answers.type` / `captions.type` must be `"extractive"`.
- [x] `semantic` + `vectorQueries` → `400 InvalidQuery`.
- [x] `semantic` + `queryType=full` → `400 InvalidQuery`.

### Answer extraction

- [x] Sentences extracted from `priorities.answers` fields (in priority order).
- [x] BM25 sentence scoring against query terms.
- [x] Top-N selection across all result documents.
- [x] Threshold: at least one query term must match.
- [x] `queryContext.questions` appended to query terms for scoring.
- [x] Response shape: `score`, `key`, `text`, `highlights`.
- [x] `@search.answers` omitted when not requested or no candidates.

### Caption extraction

- [x] First sentence / 200-char truncation from `priorities.captions` fields.
- [x] Truncation at word boundary + `...`.
- [x] Fallback to next priority field if current is empty.
- [x] Nested `captions.answers` extraction.
- [x] Response shape: `text`, `highlights`, `answers`.
- [x] `@search.captions` omitted when not requested or no candidates.

### Reranker

- [x] `@search.rerankerScore` (bare number) present on all documents in semantic search.
- [x] BM25 → [0,1] via `score / (score + 1)`.
- [x] Does not affect result ordering.
- [x] Omitted for non-semantic searches.

### Integration

- [x] Semantic + filter + orderby + select + facets + highlight all work.
- [x] `top`/`skip` applied after semantic processing (answers from full set).
- [x] `count=true` reflects the full-text result set (not affected by semantic).
- [x] `semanticErrorHandling=returnPartialResults`: partial results on extraction failure.
- [x] Non-semantic search on semantic index: unchanged behaviour.
- [x] Concurrent semantic searches do not corrupt state.

### Errors

- [x] All new error cases return correct status code and Azure error structure.
- [x] `scoringProfile`/`scoringParameters`/`scoringStatistics` still rejected with `400 UnsupportedQuery`.
- [x] Vectorizer queries (`kind: "text"`) still rejected (Phase 2.4).

### Tests

- [x] Unit tests: sentence splitting, scoring, answer selection, caption extraction, reranker, config validation.
- [x] Contract tests: `source/rust/tests/contract/semantic_search.rs` covers all matrix entries.
- [x] Python SDK tests: semantic index creation, search with answers/captions/queryContext/errorHandling.
- [x] C# SDK tests: mirror Python additions.
- [x] Fixtures captured for C# replay.
- [x] E2E: full RAG-style semantic flow.
- [x] All existing tests still pass (no regression).

### Quality gates

- [x] `cargo fmt --check` passes.
- [x] `cargo clippy --all-targets -- -D warnings` passes.
- [x] All test suites green (unit, contract, SDK Python, SDK C#, e2e).
- [x] `docs/supported_operations.md` updated (semantic flipped to Supported; `answers`/`captions`/`semantic*` removed from rejected list).
- [x] `docs/known_differences.md` updated (extractive approximation, no query rewriting, BM25 reranker).

## Exit criteria

An application can create an index with a semantic configuration, upload documents, and perform semantic search with extractive answers, captions, query context, and reranker scores through the unmodified Python and C# SDKs, with the correct response shape (`@search.rerankerScore`, `@search.answers`, `@search.captions`) and all semantic options accepted — all covered by contract and SDK tests. The application's response-parsing code (answer rendering, caption display, reranker score logging) exercises the same paths it would against Azure, with content quality differences documented in `known_differences.md`.
