# 0004 — Vector index backend: hnsw_rs

## Decision

- Vector similarity search backend: **hnsw_rs 0.3** (https://crates.io/crates/hnsw_rs), with the `simdeez_f` feature enabled, paired with its re-exported `anndists 0.1.5` distance kernels.
- `anndists` is **not** a direct dependency: it is consumed through `hnsw_rs`'s re-export (`hnsw_rs::prelude`), so the two crates can never drift apart.
- The vector index lives in a new `source/rust/src/vector/` module (`VectorEngine` / `VectorIndex` / `HnswBackend`), sitting alongside the Tantivy `SearchEngine`. The service layer orchestrates vector-only and hybrid (union + max-score) search.

## Rationale

hnsw_rs is a pure-Rust HNSW approximate-nearest-neighbour library: no C++ toolchain, no external service, consistent with the single-static-binary goal and the Tantivy integration pattern (see `0003-search-engine.md`). It provides L2 and cosine distances first-class plus a custom-distance trait used for the inner product.

The alternative considered was **usearch** (C++ core, thin Rust bindings). Rejected: a C++ build dependency repeats the class of problem that excluded Tantivy's `zstd` feature in the Nix build environment. hnsw_rs is the safer fit for the existing build constraints.

## Pinned API facts (hnsw_rs 0.3.4, verified against crate source)

- `Hnsw::new(max_nb_connection, max_elements, max_layer, ef_construction, f)` — no `dim` parameter; `m` must be ≤ 256 (the constructor calls `std::process::exit(1)` otherwise, so the emulator validates `m` at schema time and never constructs with an out-of-range value).
- `NB_LAYER_MAX = 16` (max usable layer 15); the emulator passes `max_layer = 16` and lets the constructor clamp.
- `insert(&self, (&[f32], usize))` — internally synchronized; the external id type `DataId` is `usize` (not `u64` as an early draft of the Phase 2.1 doc assumed).
- `search(&self, &[f32], knbn, ef_arg) -> Vec<Neighbour>`; `Neighbour { d_id: DataId, distance: f32, p_id }` where `d_id` is the external id supplied at insert.
- `search_filter(&self, &[f32], knbn, ef_arg, Option<&dyn FilterT>)` exists, with a blanket `FilterT` impl for `Fn(&DataId) -> bool`, but the emulator does **not** use it (see preFilter below).
- `get_point_indexation(&self) -> &PointIndexation` (iterable to `Arc<Point>` with `get_v()` / `get_origin_id()`), `get_nb_point(&self) -> usize`. No reliable point-deletion API exists.

## Implementation notes

- **One graph per (index, field).** hnsw_rs graphs have a fixed dimension, so fields cannot share. The `vectors: BTreeMap<String, Vec<f32>>` map (document key → raw vector) is the source of truth; the HNSW graph is a derived cache rebuilt from it after every mutation batch. External `DataId`s are assigned in key order at rebuild, so rebuilds are deterministic.
- **Deletes via rebuild (O(n)).** Document delete — and vector-field removal on full-replace upload — removes the key from the raw map and rebuilds the affected per-field graph. Acceptable at emulator scale; covered by a correctness test (insert → delete → search must not return the deleted key).
- **Exact path bypasses HNSW.** `exhaustiveKnn` profiles never build a graph (`hnsw: None`); per-query `exhaustive: true` forces the linear scan. Exactness is by construction, not by tuning `efSearch`.
- **Cosine uses `DistCosine` directly, no pre-normalization.** `DistCosine` evaluates `1 - dot/(|a||b|)` with `f64` accumulation, so it is well-defined for unnormalized SDK input. Score `= 1 - distance =` cosine similarity. (An early draft of the Phase 2.1 doc assumed normalization-on-insert; it is unnecessary.)
- **Dot product never touches the graph: it always scans.** Three facts force this: `hnsw_rs` requires non-negative distances (it asserts `dist_to_ref >= 0` on the search path, so a `-dot` wrapper panics on positive dots — found by test); `anndists`'s `DistDot` computes `1 - dot` and asserts `dot <= 1`, so it panics on unnormalized vectors with large magnitudes; and no fixed shift (`C - dot`) can cover unbounded dots without catastrophic float cancellation. `dotProduct` therefore executes as an exact brute-force scan over the raw inner product regardless of algorithm kind (`HnswBackend::build` returns `None` for it). Score `=` raw dot product, supporting negative dots and arbitrary magnitudes.
- **Euclidean uses `DistL2` directly** (`sqrt(Σ(a-b)²)`). Score `= 1/(1+distance)`.
- **`efSearch` is raised per-query to satisfy `k`** (`ef = max(configured, k)`), never lowered silently.
- **`preFilter` is a constrained brute-force scan** over filter-matching candidates (exact). The HNSW predicate path (`search_filter` over point IDs via the `DataId` → key side-map) is documented as a future optimization; at emulator scale the candidate set is always small enough that the scan is correct and fast.
- **Small indexes scan: the graph engages only above the `ef` window.** `hnsw_rs` search is genuinely approximate — randomized level assignment plus pruning can miss neighbours (even exact members) on tiny indexes, which made contract tests RNG-flaky. Since a scan costs no more than graph traversal when the whole index fits inside the effective `ef` (`max(efSearch, k)`) candidate window, `VectorIndex::search` scans whenever `len <= ef`. Small indexes (the emulator norm) are therefore exact and deterministic; the graph engages only for larger ones, where approximation is the documented behaviour.
- **Scores are emulator-defined approximations** of Azure's internal scoring (same direction, same cosine range); tests assert ordering and recall, never exact equality.
- **SIMD (`simdeez_f`) is opt-in and correctness-neutral.** The feature enables SIMD distance kernels when built with `RUSTFLAGS=-C target-cpu=native`, with runtime dispatch and scalar fallback (including aarch64 without AVX2). Correctness tests must not depend on SIMD being active.
- **Hybrid is union + max-score** (deterministic), not RRF/ranking-model fusion; recall matches Azure, ordering may differ.
- **Continuation tokens bind `vectorQueries` + `vectorFilterMode`** via an inline FNV-1a 64-bit hash of the canonical pair; a mid-paging change is `400 InvalidQuery`.

## Alternatives considered

- **usearch** — rejected (C++ build dependency; see Rationale).
- **HNSW predicate path for preFilter** — deferred as a future optimization; the brute-force constrained scan is exact and simpler.
- **Pre-normalizing cosine vectors on insert** — unnecessary; `DistCosine` normalizes internally.
- **`DistDot` for dotProduct** — rejected; panics on unnormalized input.
