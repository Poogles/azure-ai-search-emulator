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
| `filter` expressions (`$filter` DSL) | Rejected | No filter parser is implemented; approximating filter semantics would produce silently wrong result sets. |
| `orderby` on sortable fields | Rejected | Results are always ordered by key; pretending to sort would mask ordering assumptions in tests. |
| `select` (projection) | Rejected | Full documents are always returned. |
| `facets` | Rejected | `@search.facets` is always `null`. |
| `searchFields` / `searchMode` | Rejected | Search always spans all `searchable` string fields with AND semantics. |
| Highlighting (`highlight`, pre/post tags) | Rejected | No highlight fragments are produced. |
| Scoring profiles, parameters, statistics | Rejected | Scoring is a constant placeholder (see below). |
| Semantic and vector queries | Rejected | Out of scope for the emulator (initial design non-goal). |
| `queryType` other than `simple` (e.g. `full`/Lucene) | Rejected | Only simple-query semantics are implemented. |
| `merge` / `mergeOrUpload` / `delete` document actions | Rejected (`400 UnsupportedAction`) | Only `upload` is implemented; merge semantics (field-level merge, collection behaviour) are not. |
| `count_documents()` (`/docs/$count`), suggest, autocomplete | Route not registered; `404` | Not part of the supported matrix. |

## Silently different (operation succeeds, result may differ from Azure)

### Relevance and scoring

- `@search.score` is `1.0` for every result. Azure computes a relevance score.
- Results are ordered by the index key field, not by relevance. Azure orders by score (then by its own tie-breaks).
- **Rationale:** deterministic ordering makes tests stable and independent of index contents; real relevance ranking is not needed for a test double. Test assertions must not assume relevance ordering or score values.

### Query matching

- Only **simple** query semantics: a multi-term search matches when every analyzer token matches at least one searchable string field (AND). Azure simple queries additionally support quoted phrases, `+`/`-` modifiers, and proximity behaviour.
- Tokenization uses Tantivy's default English analyzer: lowercasing and punctuation splitting, **no stemming and no stopword removal**. Azure uses its own analyzers (e.g. the basic English analyzer stems and can drop stopwords), so `running` does not match `run` in the emulator, and `the` is a matchable term.
- **Rationale:** whole-token, case-insensitive matching covers the assertions our tests make; e2e assertions deliberately use whole-token search terms so they would also pass against Azure.

### Searchable field coverage

- Only `searchable: true` **string** fields are full-text indexed. A search term that appears only in a numeric, boolean, or collection field matches nothing. Azure indexes and matches across more field types.
- **Rationale:** string full-text search is the behaviour exercised by our applications; numeric matching belongs to filtering, which is rejected.

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
