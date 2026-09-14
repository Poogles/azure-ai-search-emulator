---
status: draft
status_last_reviewed: 2026-09-14
---

# Rust Core Debt Follow-up

## Purpose

Working checklist derived from Section 1 (Rust emulator core) of the 2026-09-14
technical-debt review (`source/rust/src/`). Pure refactoring: **no behaviour
changes**. Picks up where `simplification_refactor.md` left off — items already
done there (service/filter/query/vector splits, `ResourceKind`, `ResourceStore`,
`FacetValue`, RRF, `batch_items`, `url::form_urlencoded`, etc.) are **not**
repeated here unless debt remains.

Every item must land with `make rust` green (fmt, clippy `-D warnings`,
`cargo test --all-targets`) and, for items touching API-visible paths, the e2e
suite (`make test`) green.

## Ground rules

- One checkbox = one commit (or a small commit series).
- Do not change wire format, error codes/messages, or scoring semantics.
- `docs/supported_operations.md` and `docs/known_differences.md` must not need
  edits for any item here; if one does, stop and re-review the item.
- Line numbers below are from the pre-refactor tree and drift as items land;
  use the symbol names as the source of truth.
- Prefer helpers over macros; prefer tables/`enum`s over string matches.

## 1. Shrink remaining god-functions

- [x] **1.1 `SearchService::search` plan struct.** `service/mod.rs:898-979`
  does paging-resolve + full-text prep + doc-map + vector side + text side +
  merge + sort + facets + paginate + project (~80 lines). Extract
  `SearchPlan { skip, filter, orderby, full_text, vector_lists }` so `search()`
  becomes `resolve -> execute -> paginate/project`.
- [x] **1.2 `SearchPage` struct.** `service/mod.rs:1030-1067` `project_page`
  returns a 3-tuple with `#[allow(clippy::type_complexity)]`. Introduce
  `struct SearchPage { documents, scores, highlights }`; deletes the `allow`.
- [x] **1.3 `suggest`/`autocomplete` shared core.** `service/mod.rs:1252-1360`
  (~45 lines each, identical filter/trim/limit prologue + field walk, differ
  only dedupe-vs-first-match). Extract
  `suggester_candidates(index, suggester, filter)` plus a single
  word-match iterator; each public fn becomes ~15 lines.
- [x] **1.4 `search_response` split.** `api/mod.rs:487-566` mixes `@odata.*`,
  scoring, highlights, select, `nextLink` + `nextPageParameters` shim
  (`547-553`). Split `build_page_entries()`, `build_next_link()`,
  `build_next_page_params()`; give the compat shim its own tested fn.
- [x] **1.5 `upload_documents` thin handler.** `api/mod.rs:208-270` (60+ lines:
  batch-shape + action-kind + two wire shapes + result mapping). Move parsing
  to `DocumentAction::from_value(&Value)` in `service/types.rs`; handler is
  parse -> `index_documents` -> `to_value`.
- [x] **1.6 `document_by_key` split.** `api/mod.rs:276-328` handles KB-`retrieve`
  routing + 405-vs-404 mapping + index-doc GET in one handler. Split
  `knowledge_base_retrieve()` vs `get_document_by_key()`; route on path prefix
  in `build_router` instead of `if raw_name.starts_with(prefix)` (the prefix
  dispatch stays in the handler: the OData segment `knowledgebases('name')`
  is a single dynamic path segment, so the router's static patterns cannot
  express it).
- [x] **1.7 `clause_query` per-field helper.** `query/mod.rs:448-532`
  `Term`/`FuzzyTerm`/`Phrase` arms each re-loop searchable fields with
  `maybe_boost`. Extract
  `per_field_queries(fields, boosts, Fn(&SearchableField) -> Vec<Box<dyn Query>>)`.
- [x] **1.8 Date/search fn parser splits.** `filter/parser.rs:405-492`
  `parse_date_compare` (~90 lines, 4-way match) -> `parse_datepart_args()`,
  `parse_dateadd_args()`, `parse_datediff_args()` + 10-line dispatcher.
  `filter/parser.rs:564-647` `parse_search_function` -> `parse_ismatch_args()`
  vs `parse_isempty_isnull()` + 5-line `Or` fan-out. Fold the triple
  `utcdatetime`-peek (`225-337`, `497-522`) into one `try_parse_utcdatetime()`.
- [x] **1.9 Vector option parsers.** `service/parsing.rs:447-521`
  `parse_vector_query` inlines `k`/`exhaustive`/`weight` (~75 lines). Extract
  `parse_vector_k()`, `parse_vector_weight()`; reuse the weight check in
  `parse_search_fields:623-686` (identical finite-positive-`f32` check).
  Group `parse_vector_query_fields:527-570` + `parse_vector_query_vector:572-617`
  plumbing (`expected: usize`, `first_field` for messages) into
  `struct VectorFieldSet { names, dim }` with `parse()` + `check_vector_len()`.

## 2. Deduplication (remaining)

- [x] **2.1 Synonym-map prelude.** `service/mod.rs:295-340`
  `create_synonym_map` vs `create_or_update_synonym_map` share validate +
  `parse_synonym_rules` + `map_err`. Extract
  `parse_validated_map(name, format, synonyms)`; removes `rules.clone()` at
  `:310`.
- [x] **2.2 Finite-`f32` array parse.** Third copy of array-of-finite-`f64` ->
  `Vec<f32>`: `service/mod.rs:688-699`, `validation.rs:434-453`
  `check_vector_value`, `parsing.rs:598-616`. Single
  `validation::parse_finite_f32_array(items, field_name)` used by all three.
  (Implemented as `parse_finite_f32_array(items) -> Result<Vec<f32>,
  FiniteF32ArrayError>`; each caller maps the two variants to its own message.)
- [x] **2.3 Null-ordering helper.** `filter/mod.rs:252-295`
  `compare_field_values` + `null_matches` special-case `Null` in 3 places.
  Single `null_ordering_result(op, has_null: bool) -> Option<bool>`.
  (Implemented as `null_comparison(op, actual_null, expected_null) -> bool`,
  which covers the `Null`-vs-`Null` and value-vs-`Null` cases the three sites
  needed.)
- [x] **2.4 OData segment parser.** `api/mod.rs:663-687`
  `parse_index_name`/`parse_document_key` +
  `named_resources.rs:170-216` segment parsers + `extract_index:773-778` all do
  strip-prefix/suffix/unquote/empty-check -> `bad_request`. One generic
  `parse_odata_segment(raw, prefix, kind_label, code)`; thin wrappers only.
  (Plus `unquote_name`/`split_odata_segment` helpers in `named_resources.rs`.)
- [x] **2.5 Route-match table.** `api/mod.rs:576-589` +
  `:783-826` both iterate `ResourceKind::ALL` for collection/named checks and
  can drift from the router (`:56-107`). Single
  `match_azure_route(path) -> Option<(ResourceKind, bool)>` shared by router,
  auth/version guard, and `operation_for`. Add
  `const AZURE_COLLECTIONS: &[&str]` + `named_collection!` macro for the five
  `post(create).get(list)` pairs.
  (Done for the guard + `operation_for`. The router itself stays explicit:
  axum handlers are per-collection closures, so a data-driven router /
  `named_collection!` macro is not feasible without a large router rewrite —
  out of scope for a dedup pass.)
- [x] **2.6 Storage path walk.** `storage/mod.rs:312-384` `field_path` vs
  `resolve_field_path` walk `split('/')` twice (schema vs docs, incl.
  copy-pasted docstring `:335-353`). Share segment-iteration skeleton or a
  generic `resolve_path_segments`.
  (Implemented as `path_segments(path) -> (&str, Split)` shared by both; the
  copy-pasted docstring on `Document::resolve_path` now points at
  `resolve_field_path`.)
- [x] **2.7 Hnsw/vector grouping.** `vector/mod.rs:77-101` Cosine/Euclidean arms
  differ only by `DistCosine`/`DistL2` (extend the existing `with_hnsw!` macro
  at `:59-66`). `vector/mod.rs:390-449` groups by field 3x
  (`upsert_documents`/`remove_entries`/`delete_documents`) -> generic
  `group_by_field`. `vector/mod.rs:243-280` `hnsw_search` vs `brute_force`
  share `sort -> truncate(k)` -> `top_k` helper.
  (The build arms use a generic `build_graph<D>` fn rather than extending the
  `with_hnsw!` macro — a typed fn is cleaner for the `Hnsw::new` + param setup;
  the macro is kept for the insert/search/get_nb_point dispatch.)
- [x] **2.8 Small uniqueness/bool helpers.** `validation.rs:51-59,240-337`
  3x `BTreeSet::insert` + `bad_request("Duplicate ...")` ->
  `ensure_unique(seen, name, what)`. `storage/mod.rs:92-112`
  (+ `Suggester::from_json:207-222`, `IndexDefinition::from_json:258-283`) 6x
  `get(..).and_then(as_bool).unwrap_or(..)` -> `get_bool(obj, key, default)`
  (+ `get_opt_string`). `filter/parser.rs:381-400`
  `expect_comma`/`expect_rparen` -> single `expect_token(tok, ctx)`.
  (`ensure_unique` takes a `message: impl FnOnce() -> String` closure since the
  three duplicate messages differ; `get_opt_string` applied to `analyzer`.)
- [x] **2.9 Config parse helpers.** `config.rs:96-144` `from_values` repeats
  empty-means-unset + parse + `map_err` 5x. Add `get_nonempty()`,
  `parse_with(key, raw, parse, err_ctor)`, `parse_bool_extended()`; uniform
  messages (~30% smaller file).
  (`parse_with` takes the already-looked-up `Option<String>` rather than a key,
  since the empty-means-unset lookup is the local `get_nonempty` closure.)

## 3. Stringly-typed surfaces (compiler-checked replacements)

- [x] **3.1 `FieldType` enum.** `storage/mod.rs:12-36`
  `field_type: String` compared as raw strings in >=8 places
  (`validation.rs:115-118`, `parsing.rs:668`, `query/mod.rs:356`,
  `is_vector_field:125-130`, `is_complex_type:147-152`,
  `is_collection:156-158`, `SUPPORTED_FIELD_TYPES:25-44`, `type_ok:530-538`).
  Add `enum FieldType` with `FromStr`; keep raw string only for echo.
- [x] **3.2 `ErrorCode` enum.** `error.rs:11-16` `code: String` + ~100
  `bad_request("InvalidQuery"/...)` call sites. Add
  `enum ErrorCode { InvalidQuery, InvalidIndex, ... }` with `as_str()`;
  `bad_request(code: ErrorCode, ...)`.
- [x] **3.3 String-enum uniformity.** `service/types.rs:114-131`
  `VectorFilterMode::as_str` + `vector/distance.rs:36-44` `Metric::as_str` +
  `filter/mod.rs:37-53` `FilterOp`/`StringFunc::parse` hand-rolled pairs
  (`Metric::parse` vs `as_str` already disagree on `cosineSimilarity`).
  `strum(EnumString, Display)` or local `string_enum!` macro.
- [x] **3.4 `ConfigError`/`StorageError`/`QueryError` display.** `config.rs:39-69`
  (+ `storage/mod.rs:387-402`, `query/mod.rs:93-103`) manual `Display`.
  `thiserror` derive; uniform messages (fixes `InvalidBool:61` dropping the
  var name).
- [x] **3.5 Filter dispatcher table.** `filter/parser.rs:225-337`
  `parse_comparison` is an order-dependent 110-line `peek` chain
  (lambda -> any/all -> date-fn -> `search.*` -> plain-fn -> `in` -> op).
  Table of prefix -> parse-fn.
- [x] **3.6 Ordering sort keys.** `service/ordering.rs:87-143`
  `compare_field` + `compare_values`/`compare_numbers` nested `Option` x
  type-tag x `i64`/`u64`/`f64` ladder. Comparable `sort_key(Option<&Value>)`
  tuple + `sort_by_key`.
- [x] **3.7 Analyzer registry.** `query/analyzers.rs:91-121`
  `analyzer_tokenizer_name` 20-arm match + `None | Some(_)` catch-all mapping
  unknown -> English. `phf::Map` or generated table + explicit `Unknown`
  variant so new analyzers are a data change.

## 4. Error-handling / clone hygiene

- [ ] **4.1 Storage/error conversions.** 6x
  `.map_err(|e| ApiError::not_found(e.to_string()))?` in `service/mod.rs`
  (`199,479,496,546,551,1387`) -> `storage_not_found()` or
  `impl From<StorageError> for ApiError`. 5x
  `map_err(|e| QueryError::Engine(e.to_string()))?` in `query/mod.rs`
  (`146-155,218-226,249-257`) -> `engine_err()`. ~40x inline
  `ApiError::bad_request("InvalidQuery", format!(...))` in `parsing.rs`
  (+ `invalid_index!` in `validation.rs`) -> `invalid_query!()` macro or fns.
- [ ] **4.2 `to_value_or_null`.** `service/types.rs:25-32`,
  `synonyms.rs:99-107`, `continuation.rs:24-31`, `resources.rs:25-30` all do
  `to_value(self).unwrap_or(Value::Null)`. One
  `to_value_or_null<T: Serialize>()` in `error.rs` or `sync_util.rs`.
- [ ] **4.3 `string_items` extension.** `service/parsing.rs:21-53`
  `string_items` exists but each caller (`parse_orderby`, `parse_select`,
  `parse_facets`, `parse_search_fields`) re-does trim/filter loops. Add
  `string_items_or(value, name, per_item)` mapping helper.
- [ ] **4.4 Owned-clone paths.** `service/mod.rs:1401-1406` `require_index()`
  clones the whole definition per request; `service/mod.rs:953-960` clones
  every hit twice; `service/mod.rs:1555-1590` rebuilds synonym expansions per
  clause; `service/mod.rs:1607-1610` + `parsing.rs:440` clone-then-parse;
  `api/mod.rs:240-256` per-action `document.clone()`; `storage/mod.rs:508-559`
  whole-doc clones under read lock. Prefer `Arc<IndexDefinition>`, borrow hit
  refs through facet/paginate (clone only the page), precompute
  `lowered -> expansions` once per search, consume `Value` in
  `DocumentAction::from_value(Value)`. Document as intentional if kept.
- [ ] **4.5 Continuation-token failure mode.** `service/continuation.rs:29`
  `unwrap_or_default()` on serialize yields a decodable-but-stateless token
  that silently restarts paging. Make `decode` reject it (or
  `debug_assert` + empty string that `decode` rejects; `expect_used` is
  denied so no `expect`).
- [ ] **4.6 Lossy/dead arms.** `vector/distance.rs:58-68` unreachable
  `DotProduct` score kept "total" -> `#[cfg(test)]` or explicit `NAN` +
  comment. `service/resources.rs:140-147` `conflict_kind()` collapses kinds to
  `"resource"` -> reuse `label()`; delete method. `service/mod.rs:1423`
  `KNOWN_ANALYZERS` alias -> inline canonical path. `filter/date.rs:205-235`
  two `#[allow(cast_precision_loss)]` -> `u64` parts + `i64` diffs at the
  boundary, cast once in `evaluate()`.

## Verification

- [ ] **V.1** After each section: `make rust` (fmt, clippy `-D warnings`,
  `cargo test --all-targets`).
- [ ] **V.2** After sections 1, 2, 4 (API-visible paths): `make test` (Python
  e2e) and `make test-csharp` (C# suite incl. fixture replay).
- [ ] **V.3** Final: `make all` (rust + docker + e2e) and confirm the image
  size is still under 20 MB.
