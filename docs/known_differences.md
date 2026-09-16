---
status: complete
status_last_reviewed: 2026-09-11
---

# Known Differences from Azure AI Search

Accepted behavioural differences between the emulator and a real Azure AI Search service, with rationale. This list is the reference for "is this an emulator bug or expected?" — anything not listed here that diverges from Azure is a defect.

Differences fall into two categories:

- **Explicitly rejected** — the emulator returns `400` with a clear code instead of approximating the behaviour. Tests that need the real behaviour must run against Azure (Phase 3 comparison suite).
- **Silently different** — the operation succeeds, but the result may differ from Azure. Application code and test assertions must not depend on the Azure-specific behaviour.

## Explicitly rejected (fail with `400 UnsupportedQuery` / `400 UnsupportedAction`)

| Azure behaviour                                       | Emulator behaviour                | Rationale                                                                                           |
|:------------------------------------------------------|:----------------------------------|:----------------------------------------------------------------------------------------------------|
| Scoring profiles, parameters, statistics              | Rejected                          | Scoring is BM25 (see below); custom scoring profiles are not applied.                               |
| Semantic queries                                      | Rejected                          | No model inference in the emulator (initial design non-goal).                                       |
| Vectorizer (`kind: "text"`) queries                   | Rejected (`400 UnsupportedQuery`) | No vectorizer in the emulator; callers must supply raw vectors.                                     |

## Silently different (operation succeeds, result may differ from Azure)

### Relevance and scoring

- `@search.score` is the Tantivy BM25 relevance score (higher = more relevant). The formula differs from Azure's internal scoring, so test assertions must check ordering and relative ranking, not exact score equality.
- Full-text results are ordered by score descending, with the index key field as the deterministic tie-breaker. Azure orders by score (then by its own tie-breaks).
- `sessionId` is accepted but inert: Azure uses it to maintain consistent scoring across a user session; the emulator's deterministic tie-breaking makes it irrelevant.
- **Rationale:** BM25 ranking matches Azure's ordering direction (most relevant first) while staying deterministic; real Azure score values are an implementation detail tests must not depend on.

### Highlighting

- `highlight` (a searchable field or list of fields) returns an `@search.highlights` object per matched document: each requested field with a query-term match maps to its highlighted fragments. Only searchable fields may be highlighted; unknown or non-searchable fields are rejected with `400 InvalidQuery`.
- Fragments are sentence-window excerpts (matching Azure's approach): each sentence containing a match is one fragment (matched terms wrapped in `highlightPreTag`/`highlightPostTag`, default `<em>`/`</em>`), in order of appearance, up to 3 per field. A sentence longer than 200 characters instead yields a ±100-character window around each match. A short field (a single sentence under the limit) returns its whole value as the fragment.
- Exact fragment boundaries are implementation-defined and may differ from Azure's; phrase queries wrap their individual terms within a single fragment. Test assertions should check for the wrapped term and fragment count, not exact fragment text.

### Vector search scoring and ranking (Phase 2.1)

- `@search.score` for vector results uses emulator-defined formulas: cosine similarity for `cosine`, the raw inner product for `dotProduct`, `1/(1+l2)` for `euclidean`. Same direction as Azure (higher = more similar) and, for cosine, the same [0,1] range — but exact values differ from Azure's internal scoring. Test assertions must check ordering and recall, not exact score equality.
- The HNSW path (cosine/euclidean on indexes larger than the `ef` window) is approximate; recall may differ slightly from Azure. Everything else is exact: the brute-force path (`exhaustiveKnn` profiles, per-query `exhaustive: true`, `preFilter`, small indexes) and `dotProduct`, which always scans (raw inner products cannot back an HNSW graph — see `docs/decisions/0004-vector-index.md`).
- Hybrid (vector + full-text) ranking fuses the two sides with **Reciprocal Rank Fusion** (RRF, `k=60`): each side is ranked by its own score and a document's fused score is the sum of `1/(k+rank)` over the sides in which it appears, so a document ranked well on either side surfaces and a document present on both sides outranks one present on only one. This matches Azure's RRF-based fusion in approach (recall is the union, not the intersection). When only one side is active (vector-only or full-text-only) the native scores are kept via a plain union. Exact fused score values differ from Azure's ranking model, so test assertions must check ordering and recall, not exact score equality.
- Per-query `weight` scales that vector query's contribution to the hybrid RRF fusion (a heavier query lifts its matches); the index-schema `stored` property and `sessionId` are accepted but inert. A missing vector-query `kind` defaults to `"vector"` (emulator-only leniency).
- Algorithm-config leniencies (emulator-only): a missing algorithm `kind` defaults to `"hnsw"`; a top-level `parameters` object is accepted as an alias for the kind-specific parameters object; `m` is validated as 1-256 (Azure restricts it to 4-100).
- Paging with vectors binds `vectorQueries` + `vectorFilterMode` into the continuation token; changing them mid-paging is `400` (Azure tokens tolerate broader reuse). Fail-fast beats silently shifted pages.
- At most 5 vector queries per search and at most 16 vector fields per index (both match Azure); max dimension 3072 (or `EMULATOR_VECTOR__MAX_DIMENSION`).

### Query matching

- **Simple** query semantics with `+`/`-` modifiers, `"quoted phrases"`, and Lucene-style fuzzy terms (`term~` for the default edit distance 2 like Azure, `term~N` for an explicit distance 0-2; distances above 2 are rejected explicitly). A multi-term search combines required clauses with OR (`searchMode=any`, the default, matching Azure) or AND (`searchMode=all`).
- **Full** (`queryType=full`) query semantics are parsed by Tantivy's Lucene query parser: `AND`/`OR`/`NOT`, `field:term`, `term~N` fuzzy, `term*` wildcards, ranges, and `^N` boosts. Azure's `full` type is also Lucene syntax, but the parsers are not identical, so exotic Lucene constructs may behave differently.
- Fuzzy terms are lowercased only (no stemming, stopword removal, or punctuation splitting), matching Azure. Fuzzy matches do not produce highlight fragments (highlighting matches analyzed query terms exactly).
- Tokenization uses an English analyzer: lowercasing, punctuation splitting, English stopword removal, and English stemming — approximating Azure's basic English analyzer. `running` matches `run`; `the` is a stopword and matches nothing (a stopword-only query returns no documents).
- `searchFields` weights (`field^N`, with a finite positive `N`) scale the field's BM25 contribution to `@search.score`.
- `POST /search.analyze` uses the English analyzer, except `keyword` (the whole input as one verbatim token) and `whitespace` (whitespace split without lowercasing or stemming), which tokenize as Azure documents them. An explicit `analyzer` (`analyzerName` alias accepted) must be a known analyzer name and `field` (`fieldName` alias accepted) must exist in the index schema. Unknown analyzers/fields are rejected with `400 InvalidRequest`.
- **Rationale:** whole-token, case-insensitive matching covers the assertions our tests make; e2e assertions deliberately use whole-token search terms so they would also pass against Azure.

### Autocomplete and suggest

- Autocomplete and suggest use **case-insensitive infix matching** of each whitespace-separated search term against the whitespace-separated words of the suggester's search fields (or the `searchFields` subset). A document matches a suggestion when every term matches some word. Azure uses its full suggester algorithm (analyzing infix matching with scoring, fuzzy matching, and `searchMode` behaviour).
- Autocomplete returns distinct completed terms (`text` + `queryPlusText`); suggest returns the matching documents (projected to `select` when given, otherwise all fields) plus an `@search.text` field carrying the first word matching the first term (with highlight tags applied when both are given).
- Results are ordered by index key (deterministic), not by relevance, unless `orderby` overrides the order; `top` (default 5) limits the count.
- `autocompleteMode`: `oneTerm` completes the last term; `twoTerms` suggests matching consecutive two-word index phrases; `oneTermWithContext` additionally requires the preceding terms to appear in the candidate document. Single-term input behaves as `oneTerm` in every mode.
- `fuzzy` (`useFuzzyMatching`) adds single-edit (Levenshtein ≤ 1) typo tolerance to the infix match.
- A `filter` narrows the candidate documents (like the search route): only documents matching the filter are considered for suggestions/completions. `minimumCoverage` is accepted but inert (the matching algorithm is prefix/infix, not term-coverage-based).
- **Rationale:** substring matching covers the assertions the reference samples make (a term that prefixes a field word); real suggester scoring is out of scope for a test double.

### Filter matching

- The documented operator set (`and`/`or`/`not`, parentheses, `eq`/`ne`/`gt`/`ge`/`lt`/`le`, `in` with a parenthesized value list, the string functions `startswith`/`endswith`/`contains`, `any`/`all` in both the space-separated and `field/any(var: body)` lambda forms); anything else (e.g. other string/date functions, `search.ismatch`) is rejected with `400 InvalidQuery` rather than approximated.
- String comparisons and string functions are ordinal and case-sensitive; Azure can be configured otherwise. Type mismatches and missing fields never match.
- **Rationale:** explicit rejection beats silently wrong result sets; test filters stay within the supported set.

### Facets, ordering, projection

- Facet counts are exact (computed over the in-memory result set); Azure returns approximate counts at scale.
- Missing values sort first in ascending order and last in descending order (matching Azure); the key field is the final tie-breaker for determinism.
- **Rationale:** deterministic, exact behaviour suits a test double; assertions must not depend on Azure's scale approximations.

### Pagination

- Continuation tokens remain valid across document mutations (like Azure; results may shift). A token issued before a mutation can still be followed; the `skip` offset applies to the current result set.
- A continuation token encapsulates the result-set state: its `skip`/`filter`/`orderby` win over request parameters, so later pages may send only `{continuation}` (plus a page size) and stay on the same result set. `top` remains a per-request page size. An empty page (e.g. `top=0`) ends the sequence with no further token. Tokens are URL-safe base64 so `@odata.nextLink` survives query-string transport.
- The pinned Python SDK drops the opaque `continuation` property when re-POSTing `@search.nextPageParameters`, so SDK paging advances via `skip`; direct HTTP clients that preserve `continuation` bind the result-set state (filter/orderby) across pages.
- **Rationale:** matching Azure's token semantics keeps paging behaviour portable; SDK paging still terminates correctly via `skip`.

### Index update semantics

- `PUT /indexes('{name}')` (create or update) updates the definition **in place and preserves documents** when the schema change is compatible: every field in the previous schema is kept with the same name and type (and, for vector fields, the same dimensions). Adding fields or changing field attributes (`filterable`, `facetable`, `searchable`, analyzers, ...) preserves the documents, which are re-indexed into the updated schema.
- When the change is **incompatible** — a field is removed, or a field's type or vector dimensions change — the index is replaced and its documents are discarded (matching Azure, which cannot preserve documents it can no longer validate).
- **Rationale:** in-place preservation matches Azure's common update path so tests that evolve a schema keep their data; incompatible changes fail safe by replacing rather than silently keeping documents the new schema cannot index.

### Authentication

- Any non-empty `api-key` header is accepted; Azure validates real admin/query keys and also supports Azure AD bearer tokens. The emulator accepts only the `api-key` header.
- **Rationale:** authentication is a compatibility mechanism, not a security boundary (initial design).

### API versions

- Acceptance is floor-based: any well-formed version on or after the earliest version in `EMULATOR_API_VERSIONS` (default `2024-07-01`) is accepted, so newer SDK defaults keep working without reconfiguration. Versions below the floor, malformed ones, and well-shaped but impossible dates (e.g. `2024-13-01`) are rejected explicitly. There is no version adapter: behaviour is identical across all accepted versions.
- **Rationale:** floor acceptance keeps the pinned and reference-sample SDKs working as Azure ships new versions; unsupported versions fail explicitly rather than guessing.

### Synonym maps

- Synonym maps are stored, echoed, and managed (create/update/get/list/delete) and **applied to full-text search**: Solr equivalence and directional rules expand analyzed query tokens for indexes referencing the map (index-level or per-field `synonymMaps`). Fuzzy terms are not expanded.
- Etags are opaque counter strings, not Azure's hex entity tags.

### Index aliases

- Aliases are stored, echoed, and managed (create/update/get/list/delete), and **resolve on both the data plane and the management plane**: search, document upload/lookup/count, suggest, autocomplete, and analyze-text accept an alias name anywhere an index name is accepted, operating on the alias target (the first entry of its `indexes` array). Index management routes (`GET`/`PUT`/`DELETE /indexes('name')`) also resolve aliases: `GET` returns the target index's definition, `PUT` updates the target index, and `DELETE` deletes the target index (the alias itself is not modified). An alias pointing at a missing index behaves like the missing index (`404 ResourceNotFound`).
- Index and alias names share one namespace: creating an index named like an existing alias (or an alias named like an existing index) is `409`, since the alias would otherwise shadow the index on the data plane.
- Etags are opaque counter strings, not Azure's hex entity tags.
- **Rationale:** resolution is a name substitution before dispatch, so alias-backed tests exercise the same code paths as direct index tests.

### Collection-of-complex fields

- `Edm.Collection(Edm.ComplexType)` fields accept arrays of objects and full-text index searchable string subfields across all elements, at any nesting depth. Filters use the same `Address/State` path syntax as single complex types, with any-element semantics (a direct path matches when any element satisfies the comparison, including ordering operators); `orderby` through a nested path uses the first resolved value. Lambda bodies may address (possibly nested) subfields of the element (e.g. `Rooms/any(r: r/Type eq 'x')`, `Rooms/any(r: r/Room/Floor gt 2)`).
- **Rationale:** the schema and document surface is implemented so indexes with collection-of-complex fields round-trip; deep-collection lambda evaluation follows the same any-element semantics as direct paths.

### Additional `Edm` types

- `Edm.Int8` accepts the SDK wire spelling `Edm.SByte` as an alias (both spellings, including collection forms); `Edm.Byte` (unsigned) is rejected as unsupported.
- `Edm.Time` values must be zero-padded `"HH:MM:SS"` with an optional fractional-seconds suffix; ordering comparisons are lexicographic on that form (chronological for well-formed values).
- `Edm.Duration` values must be ISO 8601 durations (weeks cannot be combined with other components; fractional seconds allowed); only `eq`/`ne` comparisons apply, and `sortable` is rejected at schema creation.
- `Edm.Binary` values must be base64-decodable strings (standard or URL-safe alphabet); only `eq`/`ne` comparisons apply, and `sortable` is rejected at schema creation.

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
- Response envelopes: `@odata.context`, `@odata.count` (with `count=true`), `@search.facets` (present only when facets requested; omitted, not null, otherwise), `@search.score` per document, `@search.highlights` per document (only when highlighting was requested and the document matched), `{"value": [...]}` lists, per-document indexing results (`key`/`status`/`statusCode`/`errorMessage`).
- Ranking direction: BM25 results order by score descending (most relevant first), like Azure; nulls sort first ascending / last descending, like Azure.
- `searchMode` (`all`/`any`, defaulting to `any`/OR like Azure), field boosts (`field^N`), fuzzy terms (`term~`, lowercased only like Azure), stemming/stopwords, `highlight`, `in`, and string functions behave as Azure documents them (subject to the single-analyzer note above).
- Alias names resolve to their target index on data-plane routes.
- Quantized vector fields (`Collection(Edm.Half)`) are accepted and searched like `Collection(Edm.Single)` (values are dequantized to `f32`).
- API versions on or after the configured floor are accepted.
- SDK wire format: routes (`/docs/search.index`, `/docs/search.post.search`), the `{"value": [...]}` batch envelope, and top-level-spread document actions as sent by the pinned Python SDK (fixtures in `source/tests/python/fixtures/`).
- Index-not-found (`404 ResourceNotFound`) is distinguished from an empty index (successful search with zero results).
- `PUT /indexes('{name}')` updates a compatible schema in place, preserving and re-indexing its documents (incompatible changes replace the index).
