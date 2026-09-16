---
status: draft
status_last_reviewed: 2026-09-14
---

# Phase 2.2 — Closing Compatibility Gaps

## Purpose

Close the behavioural gaps between the emulator and Azure AI Search that cause application code or SDK tests to produce different results when run against the emulator versus real Azure. This phase targets features that are currently rejected with `400`, accepted-but-inert, or silently divergent, where the fix is feasible within the test-double constraint (no model inference, no external services, in-memory storage).

## Motivation

Phase 2 and Phase 2.1 established the core API surface and vector search. The supported-operations matrix (`docs/supported_operations.md`) and known-differences document (`docs/known_differences.md`) identify a set of gaps where the emulator either rejects valid Azure requests or accepts them without applying the documented behaviour. Applications that use synonym maps, date/string filter functions, nested projections, or the full suggest/autocomplete option set will encounter failures or silently wrong results. This phase closes those gaps so that the emulator is a faithful test double for the full query and schema surface our applications exercise.

## Scope

### 1. Synonym map application

**Current state:** Implemented — Solr synonym rules are parsed at map creation and applied to analyzed query tokens at search time for indexes that reference the map via `synonymMaps` (unit + contract tests green; Python/C# SDK suites pending).

**Target:** Apply Solr synonym map rules to the full-text query pipeline.

Implementation:

- Parse the Solr synonym format (`synonyms` field: newline-separated rules; each rule is a comma-separated group of equivalent terms, e.g. `new, novel, book` or `foo => bar, baz`).
- At query time, for each searchable string field that has a synonym map associated (via the index's `synonymMaps` array), expand each query token to its synonym group before matching.
- Expansion is bidirectional for equivalence groups (`a, b, c` means any of `a`/`b`/`c` matches any of the others); directional rules (`a => b, c`) expand only `a` to `b`/`c`.
- Synonym expansion applies to the analyzed (lowercased, stemmed) form of tokens, matching Azure's behaviour where synonyms operate on the analyzed token stream.
- The index definition's `synonymMaps` array (a list of synonym map names) associates maps with the index. Maps not referenced by the index are not applied.
- Synonym expansion does not affect `filter`, `orderby`, `facets`, or `select` — only full-text matching.
- Fuzzy terms (`term~N`) are not synonym-expanded (matching Azure: fuzzy operates on the raw term).

Edge cases:

- Multiple synonym maps on one index: all are applied (union of expansions).
- A synonym map referenced by name but not existing → `400 InvalidIndex` at index creation (matching Azure).
- Empty synonym groups or malformed rules → `400 InvalidSynonymMap` at map creation (already implemented).
- Synonyms that expand to stopword-only groups: the expanded terms are still subject to stopword removal (a synonym of a stopword matches nothing, same as the stopword itself).

### 2. Filter function extensions

**Current state:** Implemented — the parser additionally supports the OData date functions (`year`/`month`/`day`/`hour`/`minute`/`second`/`date`/`time`/`now`), the string functions (`length`/`indexof`/`substring`/`tolower`/`toupper`/`trim`), `search.ismatch` as a case-insensitive regex, and one level of lambda subfield access (`field/any(var: var/Subfield op value)`), alongside the pre-existing `datepart`/`dateadd`/`datediff`/`utcdatetime` functions (unit + contract tests green; Python/C# SDK suites pending).

**Target:** Extend the filter parser to support the full OData function set Azure documents.

#### 2a. Date/time functions

All operate on `Edm.DateTimeOffset` field values (or string values parseable as `DateTimeOffset`):

| Function | Semantics |
|----------|-----------|
| `year(field)` | Extract year (integer) |
| `month(field)` | Extract month (1-12) |
| `day(field)` | Extract day (1-31) |
| `hour(field)` | Extract hour (0-23) |
| `minute(field)` | Extract minute (0-59) |
| `second(field)` | Extract second (0-59) |
| `date(field)` | Truncate to date (time set to 00:00:00Z) |
| `time(field)` | Truncate to time (date set to 0001-01-01) |
| `now()` | Current UTC timestamp (no arguments) |

- Results are comparable with `eq`/`ne`/`gt`/`ge`/`lt`/`le` against integer literals (for component functions) or `DateTimeOffset` literals (for `date`/`time`/`now`).
- `now()` is evaluated once per query execution (not per document), matching Azure.
- Applied to a non-`DateTimeOffset` field → `400 InvalidQuery`.

#### 2b. String functions

All operate on `Edm.String` field values:

| Function | Semantics |
|----------|-----------|
| `length(field)` | Character count (integer) |
| `indexof(field, 'substr')` | Zero-based index of first occurrence, or `-1` |
| `substring(field, start)` / `substring(field, start, length)` | Substring extraction |
| `tolower(field)` | Lowercase (Unicode) |
| `toupper(field)` | Uppercase (Unicode) |
| `trim(field)` | Remove leading/trailing whitespace |

- `length` result is comparable with integer operators.
- `indexof` result is comparable with integer operators.
- `substring`/`tolower`/`toupper`/`trim` results are comparable with string operators (`eq`, `startswith`, etc.).
- Applied to a non-string field → `400 InvalidQuery`.
- `substring` with out-of-range `start` → empty string (matching OData semantics).

#### 2c. `search.ismatch`

- `search.ismatch(field, 'pattern')` — case-insensitive regex match against the field value.
- The pattern is a .NET-compatible regular expression (use the `regex` crate with `Regex::new`; document that exotic .NET regex features may not be supported).
- Returns a boolean; usable in `and`/`or`/`not` expressions.
- Applied to a non-string field → `400 InvalidQuery`.
- Invalid regex pattern → `400 InvalidQuery` (not a per-document error; the query is rejected at parse time).

#### 2d. Lambda subfield access

**Current state:** `Rooms/any(r: r/Type eq 'x')` is rejected. Only direct paths (`Rooms/Type eq 'x'`) work.

**Target:** Support lambda bodies that address subfields of the element variable.

- Parse `field/any(var: body)` and `field/all(var: body)` where `body` may reference `var/Subfield` (one level of subfield access through the lambda variable).
- The lambda variable must be a single identifier; `var/Subfield` resolves to the subfield of each element.
- Deeper nesting (`var/A/B`) is rejected with `400 InvalidQuery` (matching the current "no nested complex types" limitation; will work once nested complex types are supported in §4).
- The direct-path shorthand (`Rooms/Type eq 'x'`) continues to work as an alias for `Rooms/any(r: r/Type eq 'x')`.

### 3. Suggest and autocomplete option support

**Current state:** Implemented — `searchFields`, `select` (suggest), `orderby`, `autocompleteMode` (`oneTerm`/`twoTerms`/`oneTermWithContext`), `fuzzy`, and highlight tags are parsed, validated, and applied on the suggest/autocomplete routes; `minimumCoverage` is accepted but inert (unit + contract + Python/C# SDK tests green).

**Target:** Implement the options that affect result content and ordering.

#### 3a. `searchFields`

- When present, restrict matching to the listed fields (intersection with the suggester's configured fields).
- Unknown or non-searchable fields → `400 InvalidQuery`.
- When absent, all of the suggester's fields are used (current behaviour).

#### 3b. `select`

- When present, limit the fields returned in suggest document results to the listed fields (plus `@search.text`).
- `*` selects all fields (current behaviour).
- Unknown fields → `400 InvalidQuery`.
- Does not apply to autocomplete (which returns `text`/`queryPlusText` only).

#### 3c. `orderby`

- When present, order suggest/autocomplete results by the specified field(s) instead of key order.
- Fields must exist and be `sortable` (same validation as search `orderby`).
- `@search.score` is not available on these routes (no relevance score in the current matching algorithm).
- When absent, results are ordered by key (current behaviour, documented difference).

#### 3d. `autocompleteMode`

Implemented against the SDK wire format (`autocompleteMode`: `oneTerm` / `twoTerms` / `oneTermWithContext`; the `mode` SDK parameter). Note: the `twoTermAnd` / `twoTermOr` / stopword values drafted here do not exist in the Azure SDK wire format and were not implemented.

- `oneTerm` (default): only the last whitespace-separated term is completed (`queryPlusText` replaces the last input term with the completion; single-term input keeps the `"<input> <completion>"` shape).
- `twoTerms`: matching consecutive two-word index phrases are suggested (`"new y"` → `"New York"`; `queryPlusText` is the completed phrase with any earlier input terms prepended).
- `oneTermWithContext`: like `oneTerm`, but every preceding term must appear (case-insensitive infix) in the candidate document.
- Applies only when the search text contains two or more whitespace-separated terms; single-term input behaves as `oneTerm` in every mode.
- Unknown values → `400 InvalidQuery`.

#### 3e. Fuzzy matching

Implemented against the SDK wire format (`fuzzy` boolean from `useFuzzyMatching`; there is no `fuzzyMinEditDistance` in the SDK wire format).

- `fuzzy: true`: enable 1-edit (Levenshtein ≤ 1) typo-tolerant matching in addition to the infix match, on both routes.
- `fuzzy: false` / absent: exact infix matching only (current behaviour).
- Non-boolean values → `400 InvalidQuery`.

#### 3f. `minimumCoverage`

- Accepted but remains inert on suggest/autocomplete (the matching algorithm is prefix/infix, not term-coverage-based). Documented in `known_differences.md`.

#### 3g. Highlight tags

- `highlightPreTag` / `highlightPostTag` on suggest: wrap the matched portion of `@search.text` in the specified tags.
- Default: no highlighting on suggest (current behaviour).
- Applies to `@search.text` only, not to document field values.

### 4. Schema extensions

#### 4a. Nested complex types

**Current state:** `Edm.ComplexType` subfields must be scalar or collection-of-scalar. A complex type within a complex type is rejected.

**Target:** Support arbitrary nesting depth of `Edm.ComplexType`.

- A complex type's subfields may themselves be `Edm.ComplexType` or `Edm.Collection(Edm.ComplexType)`.
- Validation recurses: each level's subfields must have unique names, be valid types, and cannot be keys.
- Document validation recurses: nested objects are validated against the nested schema.
- Full-text indexing: searchable string subfields at any depth are indexed (path: `A/B/C`).
- Filter paths: `A/B/C eq 'x'` resolves through nested complex types (extending the existing path resolution).
- `select`: nested paths (`A/B/C`) are supported (see §4c).
- `orderby`/`facets`: nested paths through complex types are supported if the terminal subfield is `sortable`/`facetable`.
- Collection-of-complex lambda subfield access (§2d) works at any depth once nested types are supported.

#### 4b. Additional `Edm` field types

**Current state:** `Edm.Int8`, `Edm.Int16`, `Edm.Time`, `Edm.Duration`, `Edm.Binary` are rejected.

**Target:** Accept and store these types.

| Type | Storage | Filter support | Notes |
|------|---------|----------------|-------|
| `Edm.Int8` | `i8` (stored as integer) | Comparison operators, `in` | Range -128 to 127 validated at document upload |
| `Edm.Int16` | `i16` (stored as integer) | Comparison operators, `in` | Range -32768 to 32767 validated at document upload |
| `Edm.Time` | Time-of-day string (`"HH:MM:SS"`) | `eq`, `ne`, `gt`, `ge`, `lt`, `le` (lexicographic on normalised form) | No date functions apply |
| `Edm.Duration` | ISO 8601 duration string (`"P1DT2H"`) | `eq`, `ne` only (ordering on durations is ambiguous) | Stored as normalised string |
| `Edm.Binary` | Base64 string | `eq`, `ne` only | Stored as the base64 string |

- All five types are `filterable`, `sortable` (except `Duration`/`Binary` which are `eq`/`ne` only), and `retrievable`-able.
- None are full-text searchable (same as other non-string types).
- `Edm.Time` and `Edm.Duration` are not collections in Azure (no `Collection(Edm.Time)`); reject collection forms.
- `Edm.Binary` may be a collection (`Collection(Edm.Binary)`).

#### 4c. Nested `select` paths

**Current state:** `select` supports only top-level field names. `select=Address/City` is rejected.

**Target:** Support nested paths in `select`.

- `select=Address/City` returns only the `City` subfield of the `Address` complex type (the response contains `{"Address": {"City": "..."}}` with other subfields omitted).
- `select=Address` returns the entire complex object (current top-level behaviour).
- `select=Rooms/Type` on a collection-of-complex returns `{"Rooms": [{"Type": "..."}, ...]}` (the subfield from each element).
- `*` selects all fields at all depths (unchanged).
- Unknown paths → `400 InvalidQuery`.
- A path that resolves to a non-leaf (e.g. `select=Address` when `Address` is a complex type) returns the full sub-object.

### 5. Full-text indexing of non-string fields

**Current state:** Implemented — `searchable: true` non-string fields (numeric, boolean, `DateTimeOffset`, `Guid`) are indexed with their canonical string representation; collection elements are indexed individually.

**Target:** Index the string representation of numeric and boolean field values for full-text matching.

- `Edm.Int32`, `Edm.Int64`, `Edm.Single`, `Edm.Double`, `Edm.Boolean` fields marked `searchable: true` are indexed with their canonical string representation (`"42"`, `"3.14"`, `"true"`, `"false"`).
- `Edm.DateTimeOffset` fields marked `searchable: true` are indexed with their ISO 8601 string representation.
- `Edm.Guid` fields marked `searchable: true` are indexed with their string representation.
- Collection fields: each element's string representation is indexed (same as string collections).
- Matching: a search term matches if it equals (after analysis) the string representation of the value. A search for `"42"` matches documents with `Int32` field value `42`.
- This does not change filter behaviour (numeric filters still use typed comparison).
- The analyzer (lowercasing, stemming, stopword removal) is applied to the string representation, so `"42"` is indexed as the token `"42"` (no stemming effect on pure numerics).

### 6. Service statistics

**Current state:** Implemented — `GET /servicestats` returns real counts of indexes, documents, synonym maps, aliases, knowledge bases, and knowledge sources, plus approximate storage usage in KB and static limits (contract tests green).

**Target:** Return actual counts.

- `documentCount`: total documents across all indexes.
- `indexCount`: number of indexes.
- `storageUsageInKB`: approximate in-memory footprint (sum of document sizes).
- `maxIndexCount` / `maxIndexSizeInMB`: static limits (matching the emulator's effective limits).
- `indexingStatus`: `"available"` (always, since operations are synchronous).
- `queryStatus`: `"available"` (always).
- `synonymMapCount`: number of synonym maps.
- `aliasCount`: number of aliases.

### 7. Alias resolution on management routes

**Current state:** Implemented — `GET`/`PUT`/`DELETE /indexes('aliasName')` resolve the alias to its target index (the alias itself is not modified); `POST /indexes` (create) is unaffected; a missing alias target returns `404 ResourceNotFound` (contract tests green).

**Target:** Resolve aliases on index management routes, matching Azure.

- `GET /indexes('aliasName')` → returns the target index's definition (with the alias name in the response `name` field? No — Azure returns the target index's actual name. The emulator should return the target index definition with its real name).
- `PUT /indexes('aliasName')` → updates the target index (the alias itself is not modified).
- `DELETE /indexes('aliasName')` → deletes the target index.
- `POST /indexes` (create) is unaffected (no resolution; a new index cannot be created "through" an alias).
- If the alias target does not exist → `404 ResourceNotFound` (same as current alias data-plane behaviour).

### 8. Highlighting: sentence-window fragments

**Current state:** Highlights return the entire field value with matches wrapped in tags.

**Target:** Return sentence-window excerpts around matches, matching Azure's behaviour.

- For each matched field, extract a window of text around each match: the containing sentence (delimited by `.`, `!`, `?`, or newline) or, if the sentence exceeds a maximum length (e.g. 200 characters), a character window of ±100 characters around the match.
- Multiple matches in the same field produce multiple fragments (up to 3 per field, matching Azure's default).
- Fragments are ordered by position in the field value.
- The pre/post tags wrap only the matched term within the fragment (not the entire fragment).
- If the field value is shorter than the window, the entire value is the fragment (current behaviour for short fields).
- Phrase queries: the entire phrase is one match (one fragment).
- Fuzzy matches do not produce highlight fragments (unchanged).

### 9. `minimumCoverage` (search route)

**Current state:** Rejected with `400 UnsupportedQuery`.

**Target:** Accept and apply `minimumCoverage` on the search route.

- `minimumCoverage` is a float in [0.0, 1.0] (default 0.0, meaning no minimum).
- For a multi-term query with `searchMode=any`: a document must match at least `ceil(minimumCoverage * total_terms)` terms to be included in results.
- For `searchMode=all`: `minimumCoverage` has no effect (all terms are already required).
- For single-term queries: `minimumCoverage` has no effect.
- Values outside [0.0, 1.0] or non-numeric → `400 InvalidQuery`.
- `minimumCoverage` does not affect `@search.score` (scoring is unchanged; it only gates inclusion).

### 10. `debug` option

**Current state:** Rejected with `400 UnsupportedQuery`.

**Target:** Accept `debug: true` and return query diagnostics.

- When `debug: true` is present in the search request body, the response includes an additional `@search.debug` object (alongside the normal results):
  ```json
  {
    "@search.debug": {
      "query": {
        "parsed": "the parsed query representation",
        "fields": ["field1", "field2"],
        "synonymExpansions": {"term": ["syn1", "syn2"]}
      },
      "execution": {
        "totalCandidates": 150,
        "matchedDocuments": 42,
        "filterApplied": true,
        "vectorQueries": 1
      }
    }
  }
  ```
- The normal response shape is unchanged; `@search.debug` is an additive property.
- `debug: false` or absent → no `@search.debug` in response (current behaviour).
- `debug` does not affect result ordering, scoring, or filtering.

### 11. `stored` property enforcement

**Current state:** The `stored` property on field definitions is accepted but inert.

**Target:** Enforce the `stored`/`retrievable` separation.

- `stored: true, retrievable: false`: the field value is persisted (available for `select` and `get_document`) but omitted from search results unless explicitly selected.
- `stored: false, retrievable: true`: the field value is returned in search results but not persisted (not available via `get_document` or `select` after the search response is generated). In practice, for an in-memory emulator, this means the value is in the search response but excluded from `get_document` responses.
- `stored: true, retrievable: true` (default): full availability (current behaviour).
- `stored: false, retrievable: false`: the field is indexed (if `searchable`) but its value is never returned.
- Document upload validation is unchanged (the value must be present and type-correct at upload time regardless of `stored`/`retrievable`).
- `get_document` returns only fields with `stored: true` (or `retrievable: true`, matching Azure's "stored OR retrievable" rule for get-document).

### 12. Knowledge-base retrieve: return source documents

**Current state:** `POST /knowledgebases('{name}')/retrieve` returns an empty response.

**Target:** Return documents from the knowledge base's source index (without model inference).

- Parse the knowledge base definition's `knowledgeSources` array.
- For each source with `kind: "searchIndex"`, resolve `searchIndexParameters.searchIndexName` to the index.
- Execute a match-all search (`search: "*"`) against each source index, limited by the request's `top` (default 3, max 1000).
- Return the documents in the `response` array (each document is the source index document, with an added `@search.source` field indicating which knowledge source it came from).
- `activity` and `references` remain empty arrays (no model inference).
- If a source index does not exist → `404 ResourceNotFound` (the retrieve call fails, matching Azure's behaviour when a source is unavailable).
- The request body's `query` field (if present) is used as the search text instead of `"*"` (basic full-text match against the source index).
- This is a best-effort approximation: Azure's retrieve runs a full RAG pipeline (chunking, embedding, model inference, citation generation). The emulator returns raw source documents, which is sufficient for tests that verify the retrieve endpoint is reachable and returns data.

## What is NOT in scope

- **Semantic search** (`semantic`, `semanticConfiguration`, `answers`, `captions`) — requires model inference. Remains rejected with `400 UnsupportedQuery`.
- **Vectorizer queries** (`kind: "text"`) — requires a text-embedding model. Remains rejected with `400 UnsupportedQuery`.
- **Scoring profiles** (`scoringProfile`, `scoringParameters`, `scoringStatistics`) — custom relevance formulas are complex and application-specific; BM25 is the emulator's scoring model. Remains rejected with `400 UnsupportedQuery`.
- **Knowledge-base ingestion/synchronization** — external data connectors and model pipelines. CRUD only (unchanged).
- **Indexer / data source / skillset routes** — admin-plane resources with external dependencies. Remain unregistered (`404`).
- **Async index operations** — everything remains synchronous.
- **File storage mode** — in-memory only (unchanged).
- **Azure AD bearer-token auth** — `api-key` only (unchanged).
- **Exact BM25 score parity with Azure** — ordering parity is the bar (unchanged).
- **Quantized vector types** (`Collection(Edm.Int8)`, etc.) — remains rejected (see Phase 2.1).
- **`minimumCoverage` on suggest/autocomplete** — remains inert (the matching algorithm is prefix/infix, not term-coverage-based).
- **`sessionId`** — remains inert (deterministic tie-breaking makes it irrelevant).

## Architecture integration

```
source/rust/src/
  filter/
    mod.rs          — extended: date functions, string functions, search.ismatch,
                      lambda subfield access
  query/
    mod.rs          — extended: synonym expansion, minimumCoverage, non-string
                      field indexing, debug diagnostics
  service/
    mod.rs          — extended: suggest/autocomplete options (searchFields, select,
                      orderby, autocompleteMode, fuzzy), service statistics,
                      alias resolution on management routes, knowledge-base retrieve
  storage/
    mod.rs          — extended: nested complex types, additional Edm types,
                      stored/retrievable enforcement
  synonym/          — NEW: Solr synonym map parser and query expansion
    mod.rs          — SynonymMap, parse_solr_rules, expand_tokens
  api/
    mod.rs          — unchanged (routes already exist; service layer handles new logic)
```

## Deliverables

1. `source/rust/src/synonym/` module: Solr synonym parser, token expansion, integration with the query pipeline.
2. Filter parser extensions: date functions, string functions, `search.ismatch`, lambda subfield access.
3. Suggest/autocomplete option implementation: `searchFields`, `select`, `orderby`, `autocompleteMode`, fuzzy matching, highlight tags.
4. Schema validation extensions: nested complex types, `Edm.Int8`/`Int16`/`Time`/`Duration`/`Binary`, nested `select` paths.
5. Full-text indexing of non-string fields (numeric, boolean, datetime, guid string representations).
6. Service statistics: real counts.
7. Alias resolution on index management routes.
8. Highlighting: sentence-window fragments.
9. `minimumCoverage` on the search route.
10. `debug` option with query diagnostics.
11. `stored`/`retrievable` enforcement.
12. Knowledge-base retrieve: return source index documents.
13. Updated `docs/supported_operations.md`: flip all items from Unsupported/inert to Supported; document new filter functions, schema types, and options.
14. Updated `docs/known_differences.md`: remove closed gaps; document remaining accepted differences.
15. Contract tests: new/extended modules in `source/rust/tests/contract/`.
16. Unit tests: synonym parser, filter functions, nested type validation, non-string indexing.
17. Python SDK compatibility tests: new scenarios for each closed gap.
18. C# SDK compatibility tests: mirror the Python additions.
19. HTTP fixtures: extend `source/tests/python/fixtures/` for new wire formats.

## Test plan

### Unit tests

- **Synonym parser:** equivalence groups, directional rules, multi-line input, empty rules, malformed input, Unicode terms.
- **Synonym expansion:** single token, multi-token query, stopword interaction, fuzzy-term exclusion, multiple maps.
- **Filter date functions:** each function against known `DateTimeOffset` values; `now()` stability within a query; type mismatch rejection.
- **Filter string functions:** each function against known strings; edge cases (empty string, out-of-range substring, Unicode).
- **`search.ismatch`:** valid regex, invalid regex rejection, case-insensitivity, non-string field rejection.
- **Lambda subfield access:** `any`/`all` with `var/Subfield`; direct-path equivalence; deep nesting rejection.
- **Nested complex types:** schema validation (3+ levels), document validation, filter path resolution, full-text indexing of deep subfields.
- **Additional Edm types:** document validation (range for Int8/Int16, format for Time/Duration/Binary), filter comparison.
- **Non-string full-text indexing:** numeric/boolean/datetime/guid values indexed and matched; collection elements indexed.
- **`stored`/`retrievable`:** all four combinations; `get_document` and search response field inclusion.
- **Highlighting:** sentence extraction, window truncation, multiple matches, phrase queries, short fields.

### Contract tests (`source/rust/tests/contract/`)

- **`synonym_maps.rs`** (extended): create index with `synonymMaps`, search with synonym-expanded terms, verify expansion in results; directional rules; multiple maps; map-not-found at index creation.
- **`filtering.rs`** (extended): date functions, string functions, `search.ismatch`, lambda subfield access; type mismatch errors; combined expressions.
- **`search.rs`** (extended): `minimumCoverage` (gates results, does not affect score); `debug: true` (response includes `@search.debug`); non-string field full-text match; nested `select` paths.
- **`suggest_autocomplete.rs`** (extended): `searchFields` restriction, `select` projection, `orderby`, `autocompleteMode` (all four modes), fuzzy matching, highlight tags.
- **`index_management.rs`** (extended): nested complex type schema creation; `Edm.Int8`/`Int16`/`Time`/`Duration`/`Binary` fields; alias resolution on `GET`/`PUT`/`DELETE /indexes`.
- **`document_management.rs`** (extended): nested complex type document validation; additional Edm type validation; `stored`/`retrievable` field visibility.
- **`aliases_knowledge.rs`** (extended): knowledge-base retrieve returns source documents; source index missing → `404`.
- **`errors.rs`** (extended): new error cases (date function on non-date field, invalid regex, nested path not found, etc.).

### Python SDK compatibility tests

- Synonym map: create map, create index referencing it, upload docs, search with synonym term → expanded results.
- Filter: date functions (`year`, `month`, `date`, `now`), string functions (`length`, `substring`, `tolower`), `search.ismatch`, lambda subfield access.
- Suggest/autocomplete: `search_fields=`, `select=`, `orderby=`, `autocomplete_mode=`, `fuzzy=True`.
- Schema: nested complex type index creation; `Edm.Int8`/`Int16` fields; nested `select`.
- Search: `minimum_coverage=0.5`; `debug=True`; non-string field search.
- Service statistics: non-zero document/index counts after upload.
- Alias: `get_index`/`delete_index` through alias name.
- Knowledge-base retrieve: returns source documents.

### C# SDK compatibility tests

- Mirror the Python additions (same scenarios, .NET SDK API).

### E2E

- Full flow: create index with synonym map + nested complex types + additional Edm types, upload documents, search with synonym expansion + date filter + nested select + minimumCoverage, verify all features interact correctly.

## Known differences (to be documented after implementation)

| Behaviour | Emulator | Azure | Rationale |
|-----------|----------|-------|-----------|
| Synonym expansion | Solr rules applied to analyzed tokens | Solr rules applied to analyzed tokens | Should match; verify with Azure comparison suite. |
| `search.ismatch` regex | Rust `regex` crate (RE2 subset) | .NET regex (full) | Exotic .NET constructs (lookahead, backreferences) may not be supported. |
| Non-string full-text indexing | String representation indexed | Azure's internal tokenisation of non-string values | Same matching behaviour for simple values; edge cases (locale-specific number formatting) may differ. |
| Highlighting fragments | Sentence-window (±100 char fallback) | Azure's sentence-window (implementation-specific) | Same approach; exact fragment boundaries may differ. |
| `minimumCoverage` | Gates inclusion, does not affect score | May affect score in Azure's ranking model | Emulator keeps BM25 score unchanged; only inclusion is gated. |
| Knowledge-base retrieve | Returns raw source documents | Full RAG pipeline (chunking, embedding, model, citations) | No model inference; sufficient for endpoint-reachability tests. |
| `debug` diagnostics | Emulator-internal query representation | Azure's internal query plan | Different internal representations; shape is emulator-defined. |
| `stored: false, retrievable: true` | Value in search response, absent from `get_document` | Same | Matches Azure's documented behaviour. |
| Suggest/autocomplete ordering (no `orderby`) | Key order (deterministic) | Relevance score | Documented difference (unchanged); `orderby` now available to override. |
| `autocompleteMode` stopword handling | English stopword list | Azure's stopword list (locale-dependent) | Single-analyzer limitation (documented). |

## Checklist

### Synonym maps

- [x] Solr synonym parser handles equivalence groups and directional rules.
- [x] Synonym expansion applied to analyzed query tokens in the full-text pipeline.
- [x] Fuzzy terms are not synonym-expanded.
- [x] Multiple synonym maps on one index: union of expansions.
- [x] Index referencing a non-existent synonym map → `400 InvalidIndex`.
- [x] Synonym expansion does not affect filter/orderby/facets/select.
- [x] Contract tests: synonym-expanded search returns correct results.
- [x] Python SDK test: create map + index, search with synonym term.
- [x] C# SDK test: mirror of the Python synonym-expansion scenario.
- [x] Per-field `synonymMaps` (SDK wire format) unioned with index-level array.

### Filter extensions

- [x] Date functions: `year`, `month`, `day`, `hour`, `minute`, `second`, `date`, `time`, `now`.
- [x] String functions: `length`, `indexof`, `substring`, `tolower`, `toupper`, `trim`.
- [x] `search.ismatch` with case-insensitive regex.
- [x] Lambda subfield access: `field/any(var: var/Subfield op value)`.
- [x] Type mismatch rejection (date function on non-date, string function on non-string).
- [x] Invalid regex → `400 InvalidQuery`.
- [x] Contract tests: each function, combined expressions, error cases.
- [x] Python SDK test: filter with date/string functions, `ismatch`, lambda subfield.
- [x] C# SDK test: mirror of the Python filter-functions scenario.

### Suggest/autocomplete

- [x] `searchFields` restricts matching to listed fields.
- [x] `select` limits returned fields in suggest results.
- [x] `orderby` re-orders results by sortable field.
- [x] `autocompleteMode`: `oneTerm`, `twoTerms`, `oneTermWithContext` (SDK wire format; the `twoTerm*` values drafted earlier do not exist in the SDK).
- [x] Fuzzy matching: `fuzzy=true` (1-edit tolerance; no `fuzzyMinEditDistance` in the SDK wire format).
- [x] Highlight tags on `@search.text`.
- [x] `minimumCoverage` remains inert (documented).
- [x] Contract tests: each option, combined options, error cases.
- [x] Python SDK test: suggest/autocomplete with new options.
- [x] C# SDK test: mirror of the Python suggest/autocomplete scenario.

### Schema extensions

**Current state:** Implemented — complex types nest to any depth (schema, document, filter, orderby/facet, full-text, lambda, and select paths all recurse); `Edm.Int8`/`Edm.SByte`, `Edm.Int16`, `Edm.Time`, `Edm.Duration`, `Edm.Binary` (+ `Collection(Edm.Binary)` and narrow-int collections) are accepted with range/format validation and typed filter support; nested `select` paths project sub-objects (unit + contract + Python/C# SDK tests green).

- [x] Nested complex types: schema validation (3+ levels), document validation, filter paths, full-text indexing.
- [x] `Edm.Int8`, `Edm.Int16`: accepted, range-validated, filterable, sortable.
- [x] `Edm.Time`: accepted, lexicographic comparison, filterable, sortable.
- [x] `Edm.Duration`: accepted, `eq`/`ne` only, filterable.
- [x] `Edm.Binary`: accepted, `eq`/`ne` only, filterable; `Collection(Edm.Binary)` accepted.
- [x] Nested `select` paths: `A/B/C`, collection-of-complex subfield projection.
- [x] Contract tests: nested schema creation, document upload, filter, select.
- [x] Python SDK test: nested complex type index, additional Edm types, nested select.
- [x] C# SDK test: mirror of the Python schema-extensions scenarios.

### Full-text indexing of non-string fields

**Current state:** Implemented — `Edm.Int32`/`Int64`/`Single`/`Double`/`Boolean`/`DateTimeOffset`/`Guid` fields marked `searchable: true` are indexed with their canonical string representation; collection elements are indexed individually; the analyzer is applied to the string representation (unit + contract + Python/C# SDK tests green).

- [x] `Edm.Int32`/`Int64`/`Single`/`Double`/`Boolean`/`DateTimeOffset`/`Guid` values indexed as strings.
- [x] Collection elements indexed individually.
- [x] Search term matches the string representation.
- [x] Does not affect filter behaviour (typed comparison unchanged).
- [x] Contract tests: search for numeric/boolean value in a non-string field.
- [x] Python SDK test: upload doc with numeric field, search for the number as text.
- [x] C# SDK test: mirror of the Python non-string field search scenario.

### Service statistics

**Current state:** Implemented — `GET /servicestats` returns real counts of indexes, documents, synonym maps, aliases, knowledge bases, and knowledge sources, plus approximate storage usage in KB and static limits (contract tests green).

- [x] `documentCount`, `indexCount`, `synonymMapCount`, `aliasCount` reflect actual state.
- [x] `storageUsageInKB` is non-zero after document upload.
- [x] Contract test: stats before and after operations.

### Alias resolution on management routes

**Current state:** Implemented — `GET`/`PUT`/`DELETE /indexes('aliasName')` resolve the alias to its target index (the alias itself is not modified); `POST /indexes` (create) is unaffected; a missing alias target returns `404 ResourceNotFound` (contract tests green).

- [x] `GET /indexes('aliasName')` returns the target index definition.
- [x] `PUT /indexes('aliasName')` updates the target index.
- [x] `DELETE /indexes('aliasName')` deletes the target index.
- [x] `POST /indexes` (create) is unaffected.
- [x] Alias target missing → `404 ResourceNotFound`.
- [x] Contract tests: CRUD through alias name.

### Highlighting

- [ ] Sentence-window fragments (not whole-value).
- [ ] Max 3 fragments per field.
- [ ] Pre/post tags wrap only the matched term.
- [ ] Phrase queries: single fragment for the phrase.
- [ ] Short fields: entire value is the fragment.
- [ ] Fuzzy matches: no fragments (unchanged).
- [ ] Contract tests: highlighting with sentence windows.

### `minimumCoverage`

- [ ] Accepted on the search route (no longer `400 UnsupportedQuery`).
- [ ] Gates inclusion for `searchMode=any` multi-term queries.
- [ ] No effect for `searchMode=all` or single-term queries.
- [ ] Does not affect `@search.score`.
- [ ] Invalid values → `400 InvalidQuery`.
- [ ] Contract tests: coverage threshold gates results.

### `debug` option

- [ ] `debug: true` adds `@search.debug` to the response.
- [ ] `debug: false` / absent: no `@search.debug`.
- [ ] Does not affect results, ordering, or scoring.
- [ ] Contract test: debug response shape.

### `stored`/`retrievable` enforcement

- [ ] All four combinations behave correctly.
- [ ] `get_document` respects `stored`/`retrievable`.
- [ ] Search response respects `retrievable`.
- [ ] `select` can retrieve `stored: true, retrievable: false` fields.
- [ ] Contract tests: field visibility across operations.

### Knowledge-base retrieve

- [ ] Returns documents from `searchIndex` knowledge sources.
- [ ] `query` field in request body used as search text.
- [ ] `top` limits results (default 3, max 1000).
- [ ] Source index missing → `404 ResourceNotFound`.
- [ ] `activity` and `references` remain empty.
- [ ] Contract test: retrieve returns source documents.

### Integration

- [ ] All features interact correctly (synonym + filter + select + minimumCoverage in one query).
- [ ] Existing tests still pass (no regression).
- [ ] Concurrent requests with new features do not corrupt state.

### Errors

- [ ] All new error cases return correct status code and Azure error structure.
- [ ] Semantic/vectorizer/scoring-profile params still rejected with `400 UnsupportedQuery`.

### Tests

- [ ] Unit tests: synonym parser, filter functions, nested types, non-string indexing, highlighting.
- [ ] Contract tests: all new/extended modules cover the matrix entries.
- [ ] Python SDK tests: all new scenarios.
- [ ] C# SDK tests: mirror Python additions.
- [ ] Fixtures captured for C# replay.
- [ ] E2E: integrated flow exercising multiple new features.
- [ ] All existing tests still pass (no regression).

### Quality gates

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] All test suites green (unit, contract, SDK Python, SDK C#, e2e).
- [ ] `docs/supported_operations.md` updated (all closed gaps flipped to Supported).
- [ ] `docs/known_differences.md` updated (closed gaps removed, new differences documented).

## Exit criteria

An application can use synonym maps, date/string filter functions, `search.ismatch`, nested complex types, additional `Edm` types, nested `select` paths, non-string field full-text search, the full suggest/autocomplete option set, `minimumCoverage`, `debug`, `stored`/`retrievable` enforcement, real service statistics, alias resolution on management routes, sentence-window highlighting, and knowledge-base retrieve (source documents) through the unmodified Python and C# SDKs, with every closed gap covered by contract and SDK tests, and remaining unsupported operations (semantic, vectorizer, scoring profiles) still failing explicitly.
