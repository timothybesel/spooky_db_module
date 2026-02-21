# SSP – Full Project Context

> This file is the canonical reference for AI agents and engineers working on the SSP codebase.
> It covers every layer: semantics, data structures, module contracts, known issues, and design tensions.

---

## 1. What SSP Is

SSP (Spooky Stream Processor) is a **Rust library** that implements incremental materialized view
maintenance using DBSP (Database Stream Processing) semantics. It is one package inside the
`spooky` monorepo and is used exclusively by the **Spooky Sidecar** (`apps/ssp`).

The sidecar sits between SurrealDB and clients. SurrealDB emits LIVE SELECT change events; the
sidecar pipes them through SSP, which maintains the derived views and produces minimal diffs
(`ViewUpdate`) that the sidecar persists back into SurrealDB as graph edges.

```
SurrealDB (LIVE SELECT)
  → Sidecar: sanitize + blake3-hash
  → Circuit::ingest_*()     [this library]
  → Vec<ViewUpdate>
  → Sidecar: persist to _spooky_query / _spooky_list_ref
```

SSP is a **pure computation library** — it has no I/O, no networking, no SurrealDB dependency.
It serializes to/from JSON for state persistence (`Circuit::load_from_json`).

---

## 2. DBSP Semantics and the Membership Model

### 2.1 ZSet (the foundation)

A ZSet is `FastMap<SmolStr, i64>` — a map from record keys to integer weights.

- `weight > 0` → record is present (multiplicity = weight in standard DBSP)
- `weight = 0` → record absent (key removed from map, never stored as 0)
- `weight < 0` → transient delta (record being removed)

### 2.2 SSP's Membership Model (deviation from standard DBSP)

SSP **normalizes all weights to {0, 1}** after every operation. Standard DBSP allows weights > 1
(multiset). SSP does not. This means:

- One view-edge per (view, record) pair — no multiplicity tracking needed downstream
- Re-ingesting an existing Create is a no-op (idempotent fast path)
- DeltaEvent::Deleted is only emitted when weight transitions 1→0, not on every decrement

**Concrete example (multi-reference join):**
```
Thread 1 → User A  (weight becomes 1, not 2)
Thread 2 → User A  (weight stays 1)

Delete Thread 1 → weight stays 1 (no delete event)
Delete Thread 2 → weight becomes 0 → DeltaEvent::Deleted emitted
```

### 2.3 Delta propagation

Changes flow through the operator tree:
```
ΔInput → Filter(ΔInput) → Project(…) → ΔOutput
```

Incremental eval (`eval_delta_batch`) can propagate Scan and Filter deltas without a full scan.
Join, Limit, and Subquery projections always fall back to a full scan (`compute_full_diff`).

---

## 3. Full Data Flow (per ingest call)

```
ingest_single(BatchEntry) / ingest_batch(Vec<BatchEntry>)
  1. Table::apply_mutation(op, key, data)
       → updates rows: FastMap<RowKey, SpookyValue>
       → updates zset: FastMap<SmolStr, i64>
       → returns (zset_key, weight_delta)

  2. Build BatchDeltas { membership, content_updates }
       membership:      table → ZSet (weight deltas)
       content_updates: table → HashSet<key> (Create + Update ops)

  3. Dependency routing: dependency_list[table] → [view_index, ...]

  4. For each impacted View:
       view.process_delta(delta, db)   ← single-record path
       view.process_batch(batch, db)   ← batch path

  5. View evaluation (see §4)

  6. Return Vec<ViewUpdate> / SmallVec<[ViewUpdate; 2]>
```

---

## 4. View Evaluation (view.rs)

### 4.1 Three dispatch paths

```
process_delta(delta, db)
  ├─ is_simple_scan or is_simple_filter?
  │     └─ try_fast_single() ← zero-allocation, direct cache mutation
  └─ fallback: wrap delta in BatchDeltas → process_batch()

process_batch(batch_deltas, db)
  ├─ is_first_run?  → compute_full_diff() (full table scan + diff vs empty cache)
  ├─ has subqueries or joins? → compute_full_diff() (can't do incremental)
  └─ else → eval_delta_batch() (incremental, filter-only)
```

### 4.2 compute_full_diff

1. `eval_snapshot(root, db, params)` → `Cow<ZSet>` — evaluates the full operator tree
2. `expand_with_subqueries(&mut target_set, db)` — evaluates nested subqueries per parent
3. `cache.membership_diff_into(&target_set, &mut diff_set)` — computes ±1 diff vs current cache
4. Returns `diff_set: ZSet`

### 4.3 eval_snapshot (operator evaluation)

| Operator | Strategy |
|---|---|
| `Scan` | Zero-copy borrow of `table.zset` |
| `Filter` | SIMD numeric fast path (NumericFilterConfig) or per-record predicate |
| `Project` | Passes through to input (subqueries tracked separately) |
| `Limit` | Full sort then truncate (O(n log n)) |
| `Join` | Hash-join: build index on right side, probe with left side |

### 4.4 UniCache model

`view.cache: ZSet` is the single source of truth for all output formats.

- **Flat/Tree**: `build_result_data()` collects all cache keys, sorts, hashes with BLAKE3
- **Streaming**: only the delta records (additions/removals/updates) are emitted — no sort

### 4.5 First-run flag

`view.has_run: bool` (serialized) controls first-run detection. On first run, `process_batch`
treats all current records as additions and emits a full initial delta. This is required so the
sidecar can RELATE all initial edges.

### 4.6 Cached flags (not serialized — must be recomputed after deserialization)

| Field | Computed from | Controls |
|---|---|---|
| `has_subqueries_cached` | `plan.root.has_subquery_projections()` | skip expand_with_subqueries |
| `referenced_tables_cached` | `plan.root.referenced_tables()` | early-exit if table not relevant |
| `is_simple_scan` | `matches!(root, Scan)` | try_fast_single eligibility |
| `is_simple_filter` | `Filter { Scan }` only | try_fast_single eligibility |

**Critical**: Call `view.initialize_after_deserialize()` after loading from JSON. `Circuit::load_from_json` does this automatically. Direct deserialization of individual Views does not.

---

## 5. All Core Data Structures

### 5.1 Type aliases (zset.rs)

```rust
type Weight = i64;
type RowKey = SmolStr;
type ZSet = FastMap<RowKey, Weight>;               // FastMap<SmolStr, i64>
type FastMap<K, V> = HashMap<K, V, FxHasher>;      // rustc_hash FxHasher
type FastHashSet<T> = HashSet<T, FxHasher>;
type VersionMap = FastMap<SmolStr, u64>;
```

### 5.2 ZSet key format

All ZSet keys follow `"table:id"` format (e.g., `"user:abc123"`).

`make_zset_key(table, id)` — inlines into SmolStr (no heap) if total ≤ 23 bytes, otherwise heap.
It strips any existing table prefix from `id` before combining.

`parse_zset_key(key)` — splits on first `:`, returns `(table, id)`.

### 5.3 SpookyValue (spooky_value.rs)

JSON-compatible enum, stored as SmolStr (not String) for small string optimization:

```rust
pub enum SpookyValue {
    Null, Bool(bool), Number(f64), Str(SmolStr),
    Array(Vec<SpookyValue>), Object(FastMap<SmolStr, SpookyValue>),
}
```

Converted from `serde_json::Value` via `From`. Accessors: `as_str()`, `as_f64()`, `as_bool()`,
`as_object()`, `as_array()`, `is_null()`, `get(key)`.

Helper macro: `spooky_obj!({ "key" => value, ... })` for test data construction.

### 5.4 Operation and Delta (circuit_types.rs)

```rust
pub enum Operation { Create, Update, Delete }
// Create → weight +1, changes_content
// Update → weight  0, changes_content
// Delete → weight -1, no content change

pub struct Delta {
    pub table: SmolStr,
    pub key: SmolStr,           // ZSet key ("table:id")
    pub weight: i64,
    pub content_changed: bool,  // true for Create + Update
}
```

`Delta::from_operation(table, key, op)` is the canonical constructor in the single-record path.
`Delta::new(table, key, weight)` derives `content_changed` from `weight >= 0` — subtle difference.

### 5.5 BatchDeltas (batch_deltas.rs)

```rust
pub struct BatchDeltas {
    pub membership: FastMap<String, ZSet>,             // weight deltas, by table
    pub content_updates: FastMap<String, FastHashSet<SmolStr>>, // updated keys, by table
}
```

Note: `membership` and `content_updates` use `String` keys (not `SmolStr`). This is a known
inconsistency (see §8 Known Issues).

`BatchDeltas::add(table, key, op)` accumulates deltas and handles cancellation (Create+Delete=0).

### 5.6 Table (circuit.rs)

```rust
pub struct Table {
    pub name: TableName,                      // SmolStr
    pub zset: ZSet,                           // membership tracking
    pub rows: FastMap<RowKey, SpookyValue>,   // actual record data
}
```

**Row key format is inconsistent** (see §8 Known Issues #2). Rows may be stored with bare IDs
(`"abc123"`) or prefixed IDs (`"user:abc123"`). The `get_row_value` method in `view.rs:1115`
handles both via a two-attempt lookup, with a `format!()` allocation on the second attempt.

### 5.7 Database (circuit.rs)

```rust
pub struct Database {
    pub tables: FastMap<String, Table>,  // Note: String keys, not SmolStr
}
```

Uses `String` keys due to a compatibility regression (comment at circuit.rs:252). This causes
an allocation in every `ensure_table()` call.

### 5.8 View (view.rs)

```rust
pub struct View {
    pub plan: QueryPlan,
    pub cache: ZSet,              // serialized — required for correct delta computation
    pub last_hash: String,        // serialized — BLAKE3 hex of last flat result
    pub has_run: bool,            // serialized — false = first run
    pub params: Option<SpookyValue>, // query parameters (e.g., $clientId)
    pub format: ViewResultFormat, // serialized

    // NOT serialized — recompute with initialize_after_deserialize()
    pub has_subqueries_cached: bool,
    pub referenced_tables_cached: Vec<String>,
    pub is_simple_scan: bool,
    pub is_simple_filter: bool,
}
```

### 5.9 Circuit (circuit.rs — the active implementation)

```rust
pub struct Circuit {
    pub db: Database,
    pub views: Vec<View>,                              // serialized
    pub dependency_list: FastMap<TableName, DependencyList>, // NOT serialized, rebuilt lazily
}
// DependencyList = SmallVec<[ViewIndex; 4]>   ← zero heap for ≤4 views per table
// ViewIndex = usize
```

`dependency_list` maps table names to the indices of views that reference that table.
It is rebuilt lazily on first ingest after construction/deserialization.

### 5.10 QueryPlan and Operator

```rust
pub struct QueryPlan { pub id: String, pub root: Operator }

pub enum Operator {
    Scan { table: String },
    Filter { input: Box<Operator>, predicate: Predicate },
    Join { left: Box<Operator>, right: Box<Operator>, on: JoinCondition },
    Project { input: Box<Operator>, projections: Vec<Projection> },
    Limit { input: Box<Operator>, limit: usize, order_by: Option<Vec<OrderSpec>> },
}
```

`JoinCondition { left_field: Path, right_field: Path }` — equi-join on field equality.
`OrderSpec { field: Path, direction: String }` — "ASC" or "DESC".

### 5.11 Projection

```rust
pub enum Projection {
    All,
    Field { name: Path },
    Subquery { alias: String, plan: Box<Operator> },
}
```

`Subquery` causes a fallback to full scan during eval. The subquery operator is evaluated per
parent record using `evaluate_subqueries_for_parent_into()`.

### 5.12 Predicate

```rust
pub enum Predicate {
    Prefix { field: Path, prefix: String },
    Eq  { field: Path, value: serde_json::Value },
    Neq { field: Path, value: serde_json::Value },
    Gt  { field: Path, value: serde_json::Value },
    Gte { field: Path, value: serde_json::Value },
    Lt  { field: Path, value: serde_json::Value },
    Lte { field: Path, value: serde_json::Value },
    And { predicates: Vec<Predicate> },
    Or  { predicates: Vec<Predicate> },
}
```

Note: `value` fields are `serde_json::Value`, not `SpookyValue`. Conversion happens at evaluation
time in `resolve_predicate_value()`, which allocates on every call.

`$param` reference syntax: `{"$param": "parent.field_name"}` — looks up `field_name` in the
parent context (strips the `"parent."` prefix automatically).

`Prefix { field: "id" }` fast path: uses the ZSet key directly (no row lookup needed).

### 5.13 Path (path.rs)

```rust
pub struct Path(pub Vec<SmolStr>);
// Path::new("a.b.c") → Path(["a", "b", "c"])
// Path::new("") → Path([""])  — empty string segment, resolves to root value
```

### 5.14 ViewUpdate (update.rs)

```rust
pub enum ViewUpdate {
    Flat(MaterializedViewUpdate),
    Tree(MaterializedViewUpdate),   // placeholder, same as Flat currently
    Streaming(StreamingUpdate),
}

pub struct MaterializedViewUpdate {
    pub query_id: String,
    pub result_hash: String,       // BLAKE3 hex of sorted record IDs
    pub result_data: Vec<SmolStr>, // sorted record IDs (ZSet keys)
}

pub struct StreamingUpdate {
    pub view_id: String,
    pub records: Vec<DeltaRecord>,
}

pub struct DeltaRecord { pub id: SmolStr, pub event: DeltaEvent }

pub enum DeltaEvent { Created, Updated, Deleted }
```

---

## 6. Module Contracts

### 6.1 src/lib.rs

Public API re-exports + sets `MiMalloc` as global allocator (non-WASM only).

### 6.2 src/converter.rs

`convert_surql_to_dbsp(query: &str) -> Result<Operator>`

Parses SurrealQL using `nom` combinators. Handles:
- `SELECT field, nested.field, (SELECT ... WHERE id = $parent.field) AS alias`
- `FROM table`
- `WHERE field OP value` (Eq, Prefix, Gt, Gte, Lt, Lte, And, Or)
- `JOIN table ON left = right` (implicit join syntax)
- `ORDER BY field ASC/DESC`
- `LIMIT n`

### 6.3 src/sanitizer.rs

Input normalization before ingestion:
- Tokenizes SurrealQL (quoted strings, backticks, keywords)
- Converts `{tb: "user", id: "abc"}` record IDs to string `"user:abc"`
- Filters comments
- Validates query safety

### 6.4 src/service.rs

High-level helpers used by the sidecar:

```rust
// Ingest module
prepare(record: Value) -> (SpookyValue, String)        // normalize + blake3 hash
prepare_batch(records: Vec<Value>) -> Vec<...>          // parallel with rayon
prepare_fast(record: Value) -> (SpookyValue, String)   // skip normalization

// View module
prepare_registration(config: Value) -> (QueryPlan, Option<Value>, ViewResultFormat)
```

### 6.5 src/engine/circuit.rs (ACTIVE Circuit implementation)

Public API:

| Method | Description |
|---|---|
| `Circuit::new()` | Empty circuit |
| `Circuit::load_from_json(json)` | Deserialize + call `initialize_after_deserialize` on all views |
| `ingest_single(entry: BatchEntry)` | Single record → `SmallVec<[ViewUpdate; 2]>` |
| `ingest_batch(entries: Vec<BatchEntry>)` | Batch → `Vec<ViewUpdate>` (rayon parallel) |
| `init_load(records)` | Bulk load without view evaluation |
| `init_load_grouped(by_table)` | Bulk load, pre-grouped, with `reserve()` |
| `register_view(plan, params, format)` | Add view, run initial scan, return first update |
| `unregister_view(id)` | Remove view + rebuild dependency list |
| `rebuild_dependency_list()` | Recompute table→view-index mapping |

**BatchEntry** (the ingest DTO):
```rust
BatchEntry { table: SmolStr, op: Operation, id: SmolStr, data: SpookyValue }
// Constructors: BatchEntry::create(), ::update(), ::delete()
```

### 6.6 src/engine/circuit_indexmap.rs (EXPERIMENTAL — NOT the active implementation)

An alternative Circuit that uses `IndexMap<String, View>` instead of `Vec<View>`, with a
different API (`process_single`/`process_ingest` instead of `process_delta`/`process_batch`).
Different `Database` key type (`SmolStr` instead of `String`). Contains an unfinished
`unregister_view` that claims O(1) swap_remove but falls back to a full `rebuild_dependency_graph`
anyway. **Not currently used by the sidecar — status: experimental.**

### 6.7 src/engine/eval/filter.rs

Core evaluation utilities:

| Function | Description |
|---|---|
| `resolve_nested_value(root, path)` | Dot-notation field navigation, zero-copy |
| `compare_spooky_values(a, b)` | Total ordering: Null < Bool < Number < Str < Array < Object |
| `hash_spooky_value(v)` | FxHash for join key hashing |
| `extract_number_column(zset, path, db)` | Vectorized column extraction for SIMD filter |
| `filter_f64_batch(values, target, op)` | Chunked scalar filter (not true SIMD — see §8 #7) |
| `apply_numeric_filter(zset, config, db)` | Dispatch: lazy for <64 rows, batch for ≥64 |
| `NumericFilterConfig::from_predicate(pred)` | Extract SIMD config from numeric predicate |
| `normalize_record_id(val)` | `{tb, id}` object → `"table:id"` string |

### 6.8 src/engine/update.rs

```rust
compute_flat_hash(data: &[SmolStr]) -> String
// Sorts data, hashes with BLAKE3, returns hex string
// SmallVec optimization: stack-allocates sort buffer for ≤16 records
// ⚠ Empty sentinel "e3b0c44298fc1c14" is SHA-256(""), not BLAKE3 (see §8 #6)

build_update(raw: RawViewResult, format: ViewResultFormat) -> ViewUpdate
// Strategy pattern: routes to Flat/Tree/Streaming formatter

build_update_with_hash(raw, format, precomputed_hash) -> ViewUpdate
// For Streaming, falls back to build_update (hash not used)
```

---

## 7. Performance Design

| Decision | Rationale |
|---|---|
| `FxHasher` everywhere | Better cache performance than SipHash for short string keys |
| `SmolStr` (≤23 bytes inline) | Most table names and record IDs fit inline — zero heap |
| `SmallVec<[ViewIndex; 4]>` for dependency lists | Typical views touch ≤4 tables |
| `SmallVec<[ViewUpdate; 2]>` return from `ingest_single` | Most ingests affect ≤2 views |
| `Cow<'a, ZSet>` in `eval_snapshot` | Scan returns borrowed table.zset without copying |
| `membership_diff_into` writes to pre-allocated map | Avoids intermediate Vec allocations |
| Streaming views skip sort in `build_result_data` | Deltas don't need ordering |
| `apply_numeric_filter` threshold at 64 rows | Below 64: lazy row-by-row; above: column extraction |
| `mimalloc` global allocator | System allocator replaced for better multi-thread perf |
| Release: `lto="fat"`, `codegen-units=1`, `panic="abort"` | Maximizes inter-crate optimization |
| Rayon batch parallelism (feature-gated) | Parallel table mutation and view propagation |

---

## 8. Known Issues and Open TODOs

### #1 [CRITICAL] Two Circuit implementations with incompatible APIs

`circuit.rs` (Vec-based, active) and `circuit_indexmap.rs` (IndexMap-based, experimental) cannot
both be compiled as `Circuit`. The IndexMap version has a different `Database` struct (SmolStr
keys vs String keys) and different method names. Needs a decision: adopt IndexMap version or
discard it.

### #2 [DESIGN] Row key format inconsistency

`Table.rows` stores records with inconsistent key formats. Some paths insert with bare ID
(`"abc123"`), others with prefixed ID (`"user:abc123"`). This causes a double-lookup in the hot
path with a `format!()` heap allocation on miss:
- `view.rs:1133` — `let prefixed = format!("{}:{}", table_name, id);`
- `filter.rs:140` — `t.rows.get(format!("{}:{}", table_name, id).as_str())`

The fix is to normalize row key format at ingestion. The TODO at `view.rs:1125` acknowledges this.

### #3 [DESIGN] Predicate stores serde_json::Value not SpookyValue

Every call to `check_predicate` → `resolve_predicate_value()` converts `serde_json::Value` to
`SpookyValue`. For non-param predicates this always succeeds but allocates. The fix is to store
`SpookyValue` in `Predicate` variants at parse time.

### #4 [DESIGN] Database uses String keys instead of SmolStr

`Database.tables: FastMap<String, Table>` — comment at `circuit.rs:252` explains it was reverted
from SmolStr due to lookup compatibility issues. Every `ensure_table()` call does `name.to_string()`.

### #5 [PERF] process_delta fallback allocates BatchDeltas + ZSet

When a single-record delta doesn't hit the fast path (non-Scan/Filter view), `process_delta` at
`view.rs:134-149` allocates a fresh `BatchDeltas` and `ZSet` to wrap the single delta and call
`process_batch`. A dedicated `process_single_batch` that takes the delta directly would avoid this.

### #6 [CORRECTNESS] Wrong empty-hash sentinel in compute_flat_hash

`update.rs:177` returns `"e3b0c44298fc1c14"` for empty data. This is the first 16 hex characters
of SHA-256("") — the wrong algorithm. The correct BLAKE3("") hex is
`af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7f9c1a4a1d2a1f5e` (first 16:
`af1349b9f5f9a1a6`). If clients verify empty-view hashes, they will fail.

### #7 [PERF] filter_f64_batch is not auto-vectorizable

The `match op { ... }` inside the inner loop at `filter.rs:175` prevents the compiler from
auto-vectorizing. Each match arm is a different comparison — the compiler needs a single branch-free
loop per op variant to emit SIMD instructions. Current code is manual scalar chunking, not SIMD.

### #8 [DESIGN] unregister_view in circuit_indexmap.rs falls back to full rebuild

The `unregister_view` implementation claims O(1) via `swap_remove` with dependency graph patching,
but at line 498 it calls `rebuild_dependency_graph()` anyway because the patching logic is
incomplete. The O(1) claim in the comment is false.

### #9 [DESIGN] categorize_changes uses std HashSet not FastHashSet

`view.rs:880`: `let removal_set: std::collections::HashSet<&str> = ...` — uses SipHash instead
of FxHasher. Inconsistent with the rest of the codebase.

### #10 [COSMETIC] Dead code

- `sum_f64_simd` (`filter.rs:210`) — `#[allow(dead_code)]`, never called
- `build_streaming_delta` (`update.rs:265`) — exported but never called internally
- `build_update_with_hash` streaming branch (`update.rs:311`) — delegates to `build_update`,
  making the pre-computed hash unused for streaming

---

## 9. Test Coverage Map

| File | What it tests |
|---|---|
| `tests/e2e_communication_test.rs` | Full pipeline: ingest → ViewUpdate, multi-view |
| `tests/delta_edge_test.rs` | Delta propagation edge cases |
| `tests/weight_correction_test.rs` | Membership normalization, weight invariants |
| `tests/streaming_subquery_edge_test.rs` | Subquery correctness under streaming mode |
| `tests/converter_ultimate_test.rs` | SurrealQL → Operator tree parsing |
| `src/engine/types/zset.rs` (inline) | ZSet operations, make/parse key, SmolStr boundary |
| `src/engine/eval/filter.rs` (inline) | resolve_nested_value, compare_spooky_values, hash |
| `src/engine/operators/predicate.rs` (inline) | All predicate types, param context, nested paths |
| `src/engine/operators/operator.rs` (inline) | referenced_tables deduplication |
| `src/engine/types/circuit_types.rs` (inline) | Operation weights, Delta constructors |
| `src/engine/types/batch_deltas.rs` (inline) | BatchDeltas accumulation, cancellation |
| `src/engine/view.rs` (inline) | First-run emission, fast-path idempotency, serialization roundtrip |
| `src/engine/update.rs` (inline) | Hash determinism, format builders |
| `src/engine/circuit.rs` (inline) | apply_mutation, apply_delta, get_version |
| `benches/memory_bench.rs` | Divan benchmarks: ingest_single (0/10k/100k rows), register_view |

---

## 10. Serialization Contracts

`Circuit` serializes to JSON via `serde_json`. Fields marked `#[serde(skip)]` are NOT stored:
- `dependency_list` — rebuilt lazily on first ingest
- All four cached View flags — must call `initialize_after_deserialize()` after loading

`Circuit::load_from_json(json)` is the **only safe way** to deserialize a Circuit. It handles
the `initialize_after_deserialize` call automatically.

`ViewResultFormat` serializes as lowercase string: `"flat"`, `"tree"`, `"streaming"`.
`ViewUpdate` serializes with `tag = "format"`: `{"format": "streaming", ...}`.
`DeltaEvent` serializes as lowercase: `"created"`, `"updated"`, `"deleted"`.

---

## 11. Dependency Map

```toml
serde / serde_json   — serialization throughout
blake3               — result hash in compute_flat_hash (std feature only, no SIMD)
nom 7.1              — SurrealQL parser in converter.rs
rustc-hash 2.1       — FxHasher for all FastMap/FastHashSet
smol_str 0.3         — 23-byte inline string optimization
smallvec 1.15        — stack-allocated Vec for small collections
indexmap 2.13        — used in circuit_indexmap.rs (experimental)
rayon (optional)     — parallel batch processing, feature "parallel" (default on)
tracing 0.1          — structured debug/info/warn logging throughout
mimalloc 0.1         — global allocator (non-WASM only)
anyhow 1.0           — error handling in load_from_json
regex 1.12           — used in sanitizer/converter
lazy_static 1.4      — used in converter for regex compilation
ulid 1.1.0           — pinned version, used in service.rs
```

---

## 12. Build Targets

| Target | Notes |
|---|---|
| Native (default) | mimalloc, rayon parallel feature |
| WASM (wasm32) | getrandom with js feature, web-sys console, no rayon, no mimalloc |

WASM target disables parallelism with `#[cfg(any(target_arch = "wasm32", not(feature = "parallel")))]`
fallback blocks throughout circuit.rs and view.rs.
