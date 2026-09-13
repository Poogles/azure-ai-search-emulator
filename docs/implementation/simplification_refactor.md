---
status: draft
status_last_reviewed: 2026-09-13
---

# Simplification and Refactor Plan

## Purpose

Working checklist for the simplification review of `source/rust/src/` (see the
review findings this document is derived from). Pure refactoring: **no
behaviour changes**. Every item must land with `make rust` green (fmt, clippy
`-D warnings`, `cargo test --all-targets`) and, for items touching API-visible
code paths, the e2e suite (`make test`) green.

## Ground rules

- One checkbox = one commit (or a small commit series).
- Do not change wire format, error codes/messages, or scoring semantics.
- `docs/supported_operations.md` and `docs/known_differences.md` must not need
  edits for any item here; if one does, stop and re-review the item.
- Line numbers below are from the pre-refactor tree and drift as items land;
  use the symbol names as the source of truth.

## 1. Deduplication (do first — shrinks later splits)

- [x] **1.1 Single field-path resolver.** Replace the three copies of
      "resolve `A/B` against a document field map, fanning out over arrays"
      with one shared function: `resolve_field_values`
      (`service/mod.rs:3056`), `resolve_paths` (`filter/mod.rs:341`),
      `resolve_doc_paths` (`query/mod.rs:1004`). Suggested home: a method on
      `Document` in `storage/mod.rs` (e.g. `Document::resolve_path(&self, path) -> Vec<&Value>`).
      Update all call sites; delete the two losers.
- [x] **1.2 Unify scalar type check.** `check_field_type`'s `else` branch
      (`service/mod.rs:3767-3774`) and `type_ok` (`service/mod.rs:3908-3917`)
      are the same `Edm.*` match. Make `check_field_type` call `type_ok`.
- [x] **1.3 Unify key extraction.** `key_display` (`service/mod.rs:3042`) and
      the key block in `validate_document` (`service/mod.rs:3726-3734`) share
      the string/number → `String` logic. One helper, two call sites.
- [x] **1.4 Shared `ok`/`err` test helpers.** Copy-pasted in `service/mod.rs`
      (3942/3949), `storage/mod.rs` (533/540), `config.rs` (177/184). Add a
      `#[cfg(test)] pub mod testutil` in `lib.rs` and import it.
- [x] **1.5 Poisoned-lock helper.** `.unwrap_or_else(std::sync::PoisonError::into_inner)`
      appears 34× across `storage`, `query`, `vector`, `service`. Add a small
      helper (e.g. free fns `read_unpoisoned`/`write_unpoisoned` in a shared
      spot, or a trait) and replace all occurrences.
- [x] **1.6 Unify score sorting.** `sort_scored` (`vector/mod.rs:512`) and the
      default branch of `order_scored` (`service/mod.rs:2150-2156`) are the
      same score-desc/key-asc sort. One shared fn.
- [x] **1.7 `FieldDefinition` type predicates.** Move the complex-type check
      (`is_complex_type`, `service/mod.rs:3375`; inline in `collect_searchable`
      `query/mod.rs:962-963`; the two `if`s in `check_field_type`
      `service/mod.rs:3748-3753`) to `FieldDefinition::is_complex_type()`, and
      `is_collection_type` (`filter/mod.rs:826`) to
      `FieldDefinition::is_collection()`.
- [x] **1.8 Single known-analyzer list.** `SearchService::KNOWN_ANALYZERS`
      (`service/mod.rs:1949-1989`) and the `analyzer_tokenizer_name` match
      (`query/mod.rs:127-157`) maintain the same set. Expose
      `query::known_analyzers()` (or similar) and derive the service list from
      it.
- [x] **1.9 Synonym maps onto `ResourceStore`.** `SearchService` hand-rolls
      `RwLock<BTreeMap>` + `AtomicU64` etag + CRUD for synonym maps
      (`service/mod.rs:758-887`) while aliases/knowledge sources/knowledge
      bases use `ResourceStore`. Generalize `ResourceStore` (e.g. generic over
      the stored value with a factory) and migrate synonym maps. Decide and
      unify the etag format (decimal counter vs quoted hex,
      `service/mod.rs:874-877` vs `465-468`) — check fixture replay and SDK
      round-trip tests before changing the format; if the format is
      SDK-observable, keep both formats and only share the store mechanics.
- [x] **1.10 Deduplicate index create/upsert.** `create_index`
      (`service/mod.rs:573-612`) and `create_or_update_index`
      (`service/mod.rs:626-690`) share the alias-collision check,
      vector-index creation, and rollback sequences (three rollback variants at
      585-601, 648-663, 667-688). Extract shared helpers
      (`create_vector_indexes` already exists; add e.g. `rollback_index`).

## 2. Data-driven named resources

- [x] **2.1 `ResourceKind` enum.** Model the four named resource kinds
      (synonym maps, aliases, knowledge sources, knowledge bases) as data:
      path prefix, kind label, conflict error code, not-found label.
- [x] **2.2 Collapse service CRUD.** Replace the 15 one-line delegations in
      `SearchService` (`service/mod.rs:889-1045`, plus the synonym-map methods
      if 1.9 landed) with generic methods keyed by `ResourceKind`. Keep the
      public method names stable (the API layer and tests call them) or update
      call sites in the same change.
- [x] **2.3 Collapse API dispatch.** `create_or_update_index`, `get_index`,
      `delete_index` (`api/mod.rs:131-298`) each repeat the same 4-prefix
      dispatch. One helper that maps a raw path segment to
      `(ResourceKind, name)` and dispatches generically.
- [x] **2.4 `operation_for` table.** Replace the 90-line if-chain
      (`api/mod.rs:1138-1231`) with a `(path-prefix, method) → name` lookup
      table.

## 3. Split `service/mod.rs` (5257 lines)

Move code only; no logic changes. Suggested target layout:

- [x] **3.1 `service/types.rs`** — `IndexingResultItem`, `DocumentAction`,
      `ActionKind`, `SearchQuery`, `SearchField`, `VectorQuery`,
      `VectorFilterMode`, `Facet`, `OrderBy`, `SearchOutcome`,
      `AutocompleteCompletion`, `Suggestion` (currently lines 45-221).
- [x] **3.2 `service/continuation.rs`** — `ContinuationToken` (479-520).
- [x] **3.3 `service/resources.rs`** — `NamedResource`, `ResourceStore`
      (327-477).
- [x] **3.4 `service/synonyms.rs`** — `SynonymRule`, `parse_synonym_rules`,
      `split_synonym_terms`, `SynonymMap`, `validate_synonym_map` (229-325,
      3686-3713).
- [x] **3.5 `service/parsing.rs`** — all search-option parsers:
      `parse_orderby`, `parse_select`, `parse_facets`, `split_facet_string`,
      `parse_facet_entry`, `parse_search_mode`, `parse_paging_options`,
      `parse_vector_options`, `parse_vector_filter_mode`, `parse_vector_queries`,
      `parse_vector_query`, `parse_vector_query_fields`,
      `parse_vector_query_vector`, `parse_search_fields`,
      `parse_highlight_options`, `string_items`, `parse_filter_option`
      (2320-3033).
- [x] **3.6 `service/validation.rs`** — `validate_schema`,
      `validate_vector_field`, `validate_vector_search_config`,
      `validate_suggesters`, `validate_subfields`, `validate_document`,
      `check_field_type`, `check_vector_value`, `check_complex_value`,
      `check_complex_collection_value`, `is_geography_point`, `type_ok`,
      `finite_f32`, `SUPPORTED_FIELD_TYPES` (3367-3934).
- [x] **3.7 `service/highlight.rs`** — `page_highlights`,
      `highlight_raw_terms`, `split_sentences`, `wrap_matched_words`,
      `highlight_fragments`, `highlight_document` (3134-3350).
- [x] **3.8 `service/facets.rs`** — `compute_facets`, `facet_key`,
      `facet_value` (2244-2318).
- [x] **3.9 `service/ordering.rs`** — `order_scored`, `compare_scored`,
      `compare_field`, `compare_values`, `type_tag`, `rrf_fuse_weighted`,
      `rrf_add_list` (2092-2242).
- [x] **3.10 Move tests with their code.** The `mod tests` block
      (3936-5257) splits across the new modules as the code moves.
- [x] **3.11 Shrink `SearchService::parse_search`** (1350-1454) and
      `SearchService::search` (1463-1591) into named helpers once the
      helpers live in their own modules: e.g. `merge_scores`, `paginate`,
      `project_page` for `search`; keep `parse_search` a thin checklist.

## 4. Split `filter/mod.rs` (2300 lines)

- [x] **4.1 `filter/parser.rs`** — `Token`, `tokenize`, `is_number_start`,
      `split_lambda_field`, `Parser`, `describe_token`, `parse_filter`
      (830-1575).
- [x] **4.2 `filter/date.rs`** — `DatePart`, `DateUnit`, `DateRef`,
      `DateExpr`, `DateOperand`, `parse_datetime`, `normalize_datetime`,
      `resolve_datetime`, `datepart_value`, `dateadd_value`,
      `datediff_value` (89-188, 499-680, 789-809).
- [x] **4.3 `filter/validate.rs`** — `validate`, `require_filterable`,
      `is_collection_type` (682-828).
- [x] **4.4 `filter/mod.rs` keeps** `FilterExpr`, `FilterOp`, `FilterValue`,
      `StringFunc`, evaluation, and its tests.

## 5. Split `query/mod.rs` (2225 lines)

- [x] **5.1 `query/analyzers.rs`** — the 26 `ANALYZER_*` consts,
      `analyzer_tokenizer_name`, `language_analyzer`, `build_analyzer`,
      `register_analyzers`, `CjkTokenizer`, `CjkTokenStream`, `is_cjk_char`,
      `text_options_for`, `analyze`, `analyze_with`, `AnalyzeToken`,
      `analyze_with_offsets`, `analyze_with_offsets_and_analyzer`,
      `emulator_tokenizer_manager` (58-468).
- [x] **5.2 `query/simple.rs`** — `Clause`, `SearchMode`, `QueryType`,
      `FullTextQuery`, `parse_search_text`, `read_clause`,
      `split_fuzzy_suffix` (470-695).
- [x] **5.3 `query/lucene.rs`** — `build_lucene_query`,
      `expand_trailing_wildcards`, `expand_unquoted_wildcards`,
      `split_token_affixes`, `expand_token_wildcard`, `quotes_balanced`,
      `regex_escaped_prefix`, `lucene_query_terms` (1036-1277).
- [x] **5.4 `query/mod.rs` keeps** `SearchEngine`, `EngineIndex`,
      `build_schema`, `collect_searchable`, `text_representation`,
      `build_query`, `field_boost`, `maybe_boost`, `clause_query`, and its
      tests.

## 6. Optional smaller splits

- [ ] **6.1 `vector/config.rs`** — `HnswParams`, `VectorAlgorithmKind`,
      `VectorAlgorithm`, `VectorSearchConfig`, `parse_vector_search`,
      `parse_algorithm`, `parse_hnsw_usize` (`vector/mod.rs:40-271`).
- [ ] **6.2 `api/mod.rs`** — leave as one file unless 2.2-2.4 leave it
      bloated; if the named-resource handlers still dominate, extract
      `api/named_resources.rs`.

## 7. Local simplifications (independent, low risk)

- [ ] **7.1 Facet values as an enum.** Replace the `"s:…"`/`"n:…"`/`"b:…"`
      string round-trip in `facet_key`/`facet_value`
      (`service/mod.rs:2297-2318`) with a `FacetValue` enum; removes the lossy
      `o:{other}` fallback and the `i64`→`f64` re-parse.
- [ ] **7.2 RRF rank as `usize`.** `rrf_add_list` (`service/mod.rs:2138-2148`)
      increments a `f32` rank; use `usize` with `1.0 / (K + rank as f32)`.
- [ ] **7.3 `compare_values` numeric precision.**
      (`service/mod.rs:2219-2224`) compares via `as_f64()`; try `as_i64`/
      `as_u64` before falling back to `f64` so large integer keys order
      exactly. Verify against existing orderby tests.
- [ ] **7.4 `split_sentences`.** (`service/mod.rs:3208-3231`) replace the
      manual char-index/offset bookkeeping with a `char_indices` loop.
- [ ] **7.5 Merge/MergeOrUpload arms.** `apply_document_action`
      (`service/mod.rs:1165-1200`): extract a shared
      `merge_or_fallback` helper for the two near-identical arms.
- [ ] **7.6 `upload_documents` batch parse.** (`api/mod.rs:399-479`) the
      `Array`/`Object`/`_` match duplicates the error message; extract a
      `batch_items(&Value) -> Result<&Vec<Value>, ApiError>` helper.
- [ ] **7.7 `document_count` response.** (`api/mod.rs:628-644`) use the
      `(StatusCode, [(header, value)], body)` `IntoResponse` form instead of
      hand-building `HeaderMap` + `Bytes`.
- [ ] **7.8 `query_param`.** (`api/mod.rs:1070-1077`) replace hand-rolled
      query-string parsing with `url::form_urlencoded` (add the `url` dep if
      not present) or axum's `Query` extractor.
- [ ] **7.9 No-op shadow.** `api/mod.rs:1090` `let key: &str = key;` is a
      leftover; remove.
- [ ] **7.10 `is_azure_surface_path` table.** (`api/mod.rs:789-801`) the
      `path + "("` column is derivable; store 5 strings.
- [ ] **7.11 `version_date` byte checks.** (`version/mod.rs:87-104`) the
      digit checks run `.chars().all(is_ascii_digit)` on already byte-checked
      slices; use byte-range checks.
- [ ] **7.12 `Config::supports_api_version`.** (`config.rs:157-161`) builds a
      throwaway `VersionAdapter` per call; store the adapter in `Config` or
      call the floor logic directly.
- [ ] **7.13 Healthcheck split.** (`main.rs:127-128`) `text.split(...).next()`
      + `.nth(1)` splits twice; use `split_once`.
- [ ] **7.14 `to_value` via serde.** `IndexingResultItem::to_value`
      (`service/mod.rs:56-73`), `SynonymMap::to_value` (315-324),
      `NamedResource::to_value` (343-347) hand-build `Map`s; derive
      `Serialize` with `rename` attributes where the wire shape is fixed.
      `NamedResource` echoes an arbitrary stored body, so it may stay manual.
- [ ] **7.15 `SearchableField` struct.** (`query/mod.rs:90`) replace the
      `(String, Field, Option<String>)` tuple with a named struct.
- [ ] **7.16 `HnswBackend` match arms.** (`vector/mod.rs:318-345`) `insert`,
      `search`, `len` each match two variants calling identical methods; add a
      single accessor or generic helper.
- [ ] **7.17 `parse_hnsw_usize` callers.** (`vector/mod.rs:205-241`) let
      `HnswParams` parse the three fields from the params object in one call.
- [ ] **7.18 `DateCompare` evaluation.** (`filter/mod.rs:293-303`) converts
      the evaluated `FilterValue` back to `Value` to call `compare`; add a
      `FilterValue`-native compare.
- [ ] **7.19 `element_matches` invariant.** (`filter/mod.rs:436-446`) the
      parser guarantees a `Compare` inner; make the invariant explicit
      (debug assert or dedicated inner type) instead of silent `false`.
- [ ] **7.20 `quotes_balanced`.** (`query/mod.rs:1205-1222`) simplify the
      manual index loop.
- [ ] **7.21 Wildcard whitespace handling.** (`query/mod.rs:1109-1158`)
      leading/trailing length via `take_while` + `len_utf8` sums; use
      `trim_start`/`trim_end`-based slicing.
- [ ] **7.22 `dateadd_value` month units.** (`filter/mod.rs:568-601`) the
      Year/Quarter/Month arms repeat the same block; compute
      `months_per_unit` once and share.
- [ ] **7.23 `SearchQuery` raw-state grouping.** Group `filter_raw`,
      `orderby_raw`, `vector_queries_raw` (token plumbing) into a
      `PagingState` sub-struct.

## 8. Style cleanup

- [ ] **8.1 Consistent collection imports.** `service/mod.rs` imports
      `BTreeMap`/`BTreeSet` at the top but uses fully-qualified
      `std::collections::…` in many fns (e.g. 369, 823, 2258, 3384, 3585,
      3643). Use the imports.
- [ ] **8.2 Redundant `new()` over `Default`.** `InMemoryStorage::new`
      (`storage/mod.rs:405-410`), `SearchEngine::new` (`query/mod.rs:721-724`),
      `VectorEngine::new` (`vector/mod.rs:530-533`) just call `default()`;
      remove or keep deliberately (pick one convention).
- [ ] **8.3 `ApiError::unsupported` comment.** (`error.rs:51-57`) identical to
      `bad_request`; add a comment that it is a deliberate semantic alias.
- [ ] **8.4 Inline `value_is_null`.** (`filter/mod.rs:420-422`) one-line
      `matches!` wrapper used twice; inline it.

## Verification

- [ ] **V.1** After each section: `make rust` (fmt, clippy `-D warnings`,
      `cargo test --all-targets`).
- [ ] **V.2** After sections 2, 3, 7 (API-visible paths): `make test` (Python
      e2e) and `make test-csharp` (C# suite incl. fixture replay).
- [ ] **V.3** Final: `make all` (rust + docker + e2e) and confirm the image
      size is still under 20 MB.
