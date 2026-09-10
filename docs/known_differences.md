---
status: complete
status_last_reviewed: 2026-09-10
---

# Known Differences from Azure AI Search

Accepted behavioural differences between the emulator and a real Azure AI Search service, with rationale. This list is the reference for "is this an emulator bug or expected?" — anything not listed here that diverges from Azure is a defect.

Differences fall into two categories:

- **Explicitly rejected** — the emulator returns `400` with a clear code instead of approximating the behaviour. Tests that need the real behaviour must run against Azure (Phase 3 comparison suite).
- **Silently different** — the operation succeeds, but the result may differ from Azure. Application code and test assertions must not depend on the Azure-specific behaviour.

## Explicitly rejected (fail with `400 UnsupportedQuery` / `400 UnsupportedAction`)

| Azure behaviour                                       | Emulator behaviour                | Rationale                                                                                           |
|:------------------------------------------------------|:----------------------------------|:----------------------------------------------------------------------------------------------------|
| `searchMode` (`any` vs `all`)                         | Rejected                          | Search always uses AND semantics; `searchMode` is rejected explicitly rather than silently ignored. |
| Highlighting (`highlight`, pre/post tags)             | Rejected                          | No highlight fragments are produced.                                                                |
| Scoring profiles, parameters, statistics              | Rejected                          | Scoring is a constant placeholder (see below).                                                      |
| Semantic queries                                      | Rejected                          | No model inference in the emulator (initial design non-goal).                                       |
| Vectorizer (`kind: "text"`) queries                   | Rejected (`400 UnsupportedQuery`) | No vectorizer in the emulator; callers must supply raw vectors.                                     |
| Quantized vector types (`Collection(Edm.Half)`, etc.) | Rejected (`400 InvalidIndex`)     | Quantization is an optimisation not needed for a test double.                                       |
| `queryType` other than `simple` (e.g. `full`/Lucene)  | Rejected                          | Only simple-query semantics are implemented.                                                        |

## Silently different (operation succeeds, result may differ from Azure)

### Relevance and scoring

- `@search.score` is `1.0` for every full-text result. Azure computes a relevance score.
- Full-text results are ordered by the index key field, not by relevance. Azure orders by score (then by its own tie-breaks).
- `sessionId` is accepted but inert: Azure uses it to maintain consistent scoring across a user session; the emulator's deterministic ordering and constant scoring make it irrelevant.
- **Rationale:** deterministic ordering makes tests stable and independent of index contents; real relevance ranking is not needed for a test double. Test assertions must not assume relevance ordering or score values.

### Vector search scoring and ranking (Phase 2.1)

- `@search.score` for vector results uses emulator-defined formulas: cosine similarity for `cosine`, the raw inner product for `dotProduct`, `1/(1+l2)` for `euclidean`. Same direction as Azure (higher = more similar) and, for cosine, the same [0,1] range — but exact values differ from Azure's internal scoring. Test assertions must check ordering and recall, not exact score equality.
- The HNSW path (cosine/euclidean on indexes larger than the `ef` window) is approximate; recall may differ slightly from Azure. Everything else is exact: the brute-force path (`exhaustiveKnn` profiles, per-query `exhaustive: true`, `preFilter`, small indexes) and `dotProduct`, which always scans (raw inner products cannot back an HNSW graph — see `docs/decisions/0004-vector-index.md`).
- Hybrid (vector + full-text) ranking is union + max-score (deterministic): documents matching either side are returned, ordered by the best score. Azure fuses with RRF/a ranking model, so recall matches but ordering may differ.
- Per-query `weight`, the index-schema `stored` property, and `sessionId` are accepted but inert. A missing vector-query `kind` defaults to `"vector"` (emulator-only leniency).
- Algorithm-config leniencies (emulator-only): a missing algorithm `kind` defaults to `"hnsw"`; a top-level `parameters` object is accepted as an alias for the kind-specific parameters object; `m` is validated as 1-256 (Azure restricts it to 4-100).
- Paging with vectors binds `vectorQueries` + `vectorFilterMode` into the continuation token; changing them mid-paging is `400` (Azure tokens tolerate broader reuse). Fail-fast beats silently shifted pages.
- At most 5 vector queries per search and at most 16 vector fields per index (both match Azure); max dimension 3072 (or `EMULATOR_VECTOR__MAX_DIMENSION`).

### Query matching

- **Simple** query semantics with `+`/`-` modifiers and `"quoted phrases"`: a multi-term search matches when every required analyzer token matches at least one searchable string field (AND). Azure simple queries additionally support proximity behaviour and more modifiers.
- Tokenization uses Tantivy's default English analyzer: lowercasing and punctuation splitting, **no stemming and no stopword removal**. Azure uses its own analyzers (e.g. the basic English analyzer stems and can drop stopwords), so `running` does not match `run` in the emulator, and `the` is a matchable term.
- `POST /search.analyze` always uses Tantivy's default analyzer regardless of the `analyzerName` or `field` parameters (accepted but inert). Token offsets and positions are accurate for the default analyzer.
- **Rationale:** whole-token, case-insensitive matching covers the assertions our tests make; e2e assertions deliberately use whole-token search terms so they would also pass against Azure.

### Autocomplete and suggest

- Autocomplete and suggest use **case-insensitive prefix matching** of the search text against the whitespace-separated words of the suggester's search fields. Azure uses its full suggester algorithm (analyzing infix matching with scoring, fuzzy matching, and `searchMode` behaviour).
- Autocomplete returns distinct completed terms (`text` + `queryPlusText`); suggest returns the matching documents (all fields) plus an `@search.text` field carrying the first matched word.
- Results are ordered by index key (deterministic), not by relevance; `top` (default 5) limits the count.
- The suggester's `searchMode` is accepted but inert, as are the other options the SDKs support on these routes (`filter`, `select`, `searchFields`, `orderby`, fuzzy matching, highlight tags, `autocompleteMode`, `minimumCoverage`). In particular a `filter` does not narrow suggestions — test assertions must not rely on it.
- **Rationale:** prefix matching covers the assertions the reference samples make (a term that prefixes a field word); real suggester scoring and infix matching are out of scope for a test double.

### Filter matching

- Only the documented operator set (`and`/`or`/`not`, parentheses, `eq`/`ne`/`gt`/`ge`/`lt`/`le`, `any`/`all` in both the space-separated and `field/any(var: body)` lambda forms); anything else (e.g. `in`, string functions, `search.ismatch`) is rejected with `400 InvalidQuery` rather than approximated.
- String comparisons are ordinal and case-sensitive; Azure can be configured otherwise. Type mismatches and missing fields never match.
- **Rationale:** explicit rejection beats silently wrong result sets; test filters stay within the supported set.

### Searchable field coverage

- Only `searchable: true` **string** fields are full-text indexed. A search term that appears only in a numeric, boolean, or collection field matches nothing. Azure indexes and matches across more field types.
- **Rationale:** string full-text search is the behaviour exercised by our applications; numeric matching belongs to filtering, which is implemented separately.

### Facets, ordering, projection

- Facet counts are exact (computed over the in-memory result set); Azure returns approximate counts at scale.
- Missing values sort last regardless of direction; Azure sorts nulls first in ascending order.
- `searchFields` weights (`field^2`) are accepted but inert (scoring is constant).
- **Rationale:** deterministic, exact behaviour suits a test double; assertions must not depend on Azure's scale approximations or null ordering.

### Pagination

- Continuation tokens embed a `state_version` that is bumped on every document mutation; following a token after the index changed returns `400` (stale token) and the search must be restarted. Azure tokens remain valid across mutations (results may shift).
- The pinned Python SDK drops the opaque `continuation` property when re-POSTing `@search.nextPageParameters`, so SDK paging advances via `skip` and does not get staleness detection; direct HTTP clients that preserve `continuation` do.
- **Rationale:** fail-fast staleness beats silently shifted pages in tests; SDK paging still terminates correctly via `skip`.

### Index update semantics

- `PUT /indexes('{name}')` (create or update) **replaces the index and discards all its documents**. Azure updates the definition in place and preserves documents when the schema change is compatible.
- **Rationale:** replacement is simpler and deterministic; test suites recreate indexes from scratch rather than relying on in-place schema evolution.

### Authentication

- Any non-empty `api-key` header is accepted; Azure validates real admin/query keys and also supports Azure AD bearer tokens. The emulator accepts only the `api-key` header.
- **Rationale:** authentication is a compatibility mechanism, not a security boundary (initial design).

### API versions

- Only the versions listed in `EMULATOR_API_VERSIONS` (default `2024-07-01`) are accepted; Azure accepts a wide range of versions with version-specific behaviour. There is no version adapter: behaviour is identical across all accepted versions.
- **Rationale:** one supported version is sufficient for the pinned SDKs; unsupported versions fail explicitly rather than guessing.

### Synonym maps

- Synonym maps are stored, echoed, and managed (create/update/get/list/delete) but **inert**: they do not affect search results. Azure rewrites queries using the map's rules; the emulator never applies them.
- Etags are opaque counter strings, not Azure's hex entity tags.
- **Rationale:** the CRUD surface is implemented so samples and clients that manage maps work unchanged; applying Solr synonym rules to the query pipeline is out of scope for a test double. Test assertions must not expect synonym expansion in search results.

### Index aliases

- Aliases are stored, echoed, and managed (create/update/get/list/delete) but **do not resolve**: search and document routes do not accept an alias name in place of an index name. Azure resolves aliases to their target index; the emulator returns `404` for an alias name on those routes.
- Etags are opaque counter strings, not Azure's hex entity tags.
- **Rationale:** the CRUD surface is implemented so samples and clients that manage aliases work unchanged; alias resolution in the query path is out of scope for a test double.

### Collection-of-complex fields

- `Edm.Collection(Edm.ComplexType)` fields accept arrays of objects and full-text index searchable string subfields across all elements. Filters use the same `Address/State` path syntax as single complex types; OData lambda shapes (`Address/any(a: ...)`) are not specially handled for complex collections.
- **Rationale:** the schema and document surface is implemented so indexes with collection-of-complex fields round-trip; complex-collection lambda evaluation is out of scope.

### Knowledge sources, knowledge bases, and agentic retrieval

- Knowledge sources and bases are stored, echoed, and managed (create/update/get/list/delete) but **inert**: no ingestion, synchronization, or model inference runs. `POST /knowledgebases('{name}')/retrieve` returns an empty response (`{"response": [], "activity": [], "references": []}`) when the base exists.
- Etags are opaque counter strings, not Azure's hex entity tags.
- **Rationale:** the CRUD surface is implemented so samples and clients that manage these resources work unchanged; model inference and agentic generation are initial-design non-goals.

### Service surface

- No indexers, data sources, skillsets, or other admin resources — those routes are not registered (`404`).
- No asynchronous index operations: everything completes synchronously.
- `GET /health` and `POST /admin/reset` exist only in the emulator (the latter gated by `EMULATOR_ENABLE_ADMIN`).
- **Rationale:** the emulator implements the surface our applications use (see `docs/supported_operations.md`), not the full Azure service.

### Scale and durability

- In-memory only by default; state is lost when the process exits. `EMULATOR_STORAGE__MODE=file` fails fast at startup (not implemented).
- Search matches are capped at 1,000,000 documents per query.
- **Rationale:** local development and test isolation are the targets; production-scale behaviour is a non-goal.

## Not differences (deliberately Azure-compatible)

- Error structure: `{"error": {"code", "message"}}` with Azure status codes (`401`, `400`, `404`, `409`, `500`).
- Response envelopes: `@odata.context`, `@odata.count` (with `count=true`), `@search.facets: null`, `@search.score` per document, `{"value": [...]}` lists, per-document indexing results (`key`/`status`/`statusCode`/`errorMessage`).
- SDK wire format: routes (`/docs/search.index`, `/docs/search.post.search`), the `{"value": [...]}` batch envelope, and top-level-spread document actions as sent by the pinned Python SDK (fixtures in `source/tests/python/fixtures/`).
- Index-not-found (`404 ResourceNotFound`) is distinguished from an empty index (successful search with zero results).
