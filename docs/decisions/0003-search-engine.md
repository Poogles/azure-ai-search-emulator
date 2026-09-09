# 0003 — Search engine backend: Tantivy

## Decision

- Full-text search and indexing backend: **Tantivy** (https://github.com/quickwit-oss/tantivy).
- Tantivy is used as an embedded, in-process library behind the emulator's query-engine abstraction. It is not a separate service.
- The Azure-compatible HTTP/API layer and the internal query representation remain the compatibility boundary. Tantivy is an implementation detail of the query engine and storage layers.

## Rationale

Tantivy is a full-text search engine library written in Rust, modelled on Apache Lucene. It provides the indexing, inverted-index, and query primitives that would otherwise have to be implemented from scratch, while remaining a native Rust dependency with no external process, JVM, or network dependency.

Using Tantivy gives us a mature, Lucene-equivalent search implementation inside the Rust ecosystem:

- **Lucene semantics in Rust** — inverted indexes, tokenisation/analyzers, boolean and phrase queries, and scoring, so we get realistic full-text behaviour without reimplementing the core.
- **Embedded and dependency-light** — a Cargo dependency, not a separate Elasticsearch/OpenSearch/Lucene service. This keeps the emulator a single static binary with fast startup and trivial CI deployment, matching the Phase 0/1 goals.
- **Deterministic local search** — in-process indexing supports the immediate-consistency and test-isolation requirements (fresh index per test, service reset).
- **Mature and maintained** — actively developed by the Quickwit team, with a stable API and a track record in production search systems.

The emulator still owns the Azure-specific concerns that Tantivy does not provide: the Azure HTTP contract, the Azure filter DSL, index/field schema mapping, continuation-token pagination, and Azure-compatible error structures. Tantivy handles the underlying text indexing and full-text matching; the compatibility layer maps Azure concepts onto it.

## Scope of use

- Tantivy is the default backend for full-text search and the document index.
- Structured filtering, ordering, projection, and pagination are applied around Tantivy results (or via Tantivy's own filter/sort facilities where they map cleanly).
- The `Storage` and query-engine abstractions are designed so the backend can be swapped if requirements outgrow Tantivy, but Tantivy is the committed default.

## Implementation notes

- **Cargo features:** default features are disabled. Only the pure-Rust features needed for the English text analyzer are enabled (`mmap`, `stopwords`, `stemmer`). In particular the C-based `zstd` compression (`columnar-zstd-compression`) is excluded: it fails to link under the Nix build environment (`ld: library not found for -liconv`) and is unnecessary for a local test double. Compression choice does not affect search correctness.
- **Phase 1 integration (`source/rust/src/query/`):** the `SearchEngine` owns one in-RAM Tantivy index per emulator index (built from the index schema at creation time). Document uploads replace-by-key and commit synchronously; the reader is explicitly reloaded after every commit so newly indexed documents are immediately searchable (the emulator's immediate-consistency guarantee). Search returns matching keys, which the service resolves back to the full stored documents and orders by key. Scores are not yet used (placeholder `1.0`); relevance ordering is a Phase 2 refinement.

## Alternatives considered

- **Rolling our own full-text search** — implementing tokenisation, inverted indexes, and scoring from scratch. Rejected: large effort, high risk, and unnecessary given a mature Rust library exists.
- **Apache Lucene (JVM)** — the reference implementation, but requires a JVM and a foreign-language boundary, which conflicts with the single-static-Rust-binary goal.
- **Elasticsearch / OpenSearch** — full server products; add a substantial external service, JVM, and deployment complexity disproportionate to a local test double.
- **Simpler in-memory matching (e.g. substring/regex over stored documents)** — insufficient for realistic full-text behaviour (tokenisation, relevance, boolean/phrase queries) that applications and tests may rely on.
