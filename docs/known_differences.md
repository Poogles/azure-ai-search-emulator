---
status: complete
status_last_reviewed: 2026-09-09
---

# Known Differences from Azure AI Search

Accepted behavioural differences between the emulator and a real Azure AI Search service, with rationale. This list is the reference for "is this an emulator bug or expected?" — anything not listed here that diverges from Azure is a defect.

Differences fall into two categories:

- **Explicitly rejected** — the emulator returns `400` with a clear code instead of approximating the behaviour. Tests that need the real behaviour must run against Azure (Phase 3 comparison suite).
- **Silently different** — the operation succeeds, but the result may differ from Azure. Application code and test assertions must not depend on the Azure-specific behaviour.

## Explicitly rejected (fail with `400 UnsupportedQuery` / `400 UnsupportedAction`)

| Azure behaviour | Emulator behaviour | Rationale |
|-----------------|--------------------|-----------|
| `searchMode` (`any` vs `all`) | Rejected | Search always uses AND semantics; `searchMode` is rejected explicitly rather than silently ignored. |
| Highlighting (`highlight`, pre/post tags) | Rejected | No highlight fragments are produced. |
| Scoring profiles, parameters, statistics | Rejected | Scoring is a constant placeholder (see below). |
| Semantic and vector queries | Rejected | Out of scope for the emulator (initial design non-goal). |
| `queryType` other than `simple` (e.g. `full`/Lucene) | Rejected | Only simple-query semantics are implemented. |
| Suggest, autocomplete | Route not registered; `404` | Not part of the supported matrix. |

## Silently different (operation succeeds, result may differ from Azure)

### Relevance and scoring

- `@search.score` is `1.0` for every result. Azure computes a relevance score.
- Results are ordered by the index key field, not by relevance. Azure orders by score (then by its own tie-breaks).
- `sessionId` is accepted but inert: Azure uses it to maintain consistent scoring across a user session; the emulator's deterministic ordering and constant scoring make it irrelevant.
- **Rationale:** deterministic ordering makes tests stable and independent of index contents; real relevance ranking is not needed for a test double. Test assertions must not assume relevance ordering or score values.

### Query matching

- **Simple** query semantics with `+`/`-` modifiers and `"quoted phrases"`: a multi-term search matches when every required analyzer token matches at least one searchable string field (AND). Azure simple queries additionally support proximity behaviour and more modifiers.
- Tokenization uses Tantivy's default English analyzer: lowercasing and punctuation splitting, **no stemming and no stopword removal**. Azure uses its own analyzers (e.g. the basic English analyzer stems and can drop stopwords), so `running` does not match `run` in the emulator, and `the` is a matchable term.
- `POST /search.analyze` always uses Tantivy's default analyzer regardless of the `analyzerName` or `field` parameters (accepted but inert). Token offsets and positions are accurate for the default analyzer.
- **Rationale:** whole-token, case-insensitive matching covers the assertions our tests make; e2e assertions deliberately use whole-token search terms so they would also pass against Azure.

### Filter matching

- Only the documented operator set (`and`/`or`/`not`, parentheses, `eq`/`ne`/`gt`/`ge`/`lt`/`le`, `any`/`all`); anything else (e.g. `in`, string functions, `search.ismatch`) is rejected with `400 InvalidQuery` rather than approximated.
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

### Service surface

- No indexers, data sources, skillsets, synonym maps, or other admin resources — those routes are not registered (`404`).
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
