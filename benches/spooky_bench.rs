use criterion::{black_box, criterion_group, criterion_main, Criterion};
use spooky_db_module::spooky_record::SpookyRecord;
use spooky_db_module::spooky_record_mut::SpookyRecordMut;
use spooky_db_module::spooky_value::SpookyValue;

// ─── Test Data ──────────────────────────────────────────────────────────────

/// A realistic CBOR payload (~6 fields, nested objects).
/// Represents a user document with profile metadata.
const BENCH_CBOR: &[u8] = &[
    166, 99, 97, 103, 101, 24, 28, 106, 99, 114, 101, 97, 116, 101, 100, 95,
    97, 116, 116, 50, 48, 50, 52, 45, 48, 49, 45, 49, 53, 84, 49, 48, 58, 51,
    48, 58, 48, 48, 90, 101, 101, 109, 97, 105, 108, 113, 97, 108, 105, 99,
    101, 64, 101, 120, 97, 109, 112, 108, 101, 46, 99, 111, 109, 98, 105, 100,
    107, 117, 115, 101, 114, 58, 97, 98, 99, 49, 50, 51, 100, 110, 97, 109,
    101, 101, 65, 108, 105, 99, 101, 103, 112, 114, 111, 102, 105, 108, 101,
    162, 102, 97, 118, 97, 116, 97, 114, 107, 104, 116, 116, 112, 115, 58, 47,
    47, 46, 46, 46, 99, 98, 105, 111, 105, 68, 101, 118, 101, 108, 111, 112,
    101, 114,
];

/// Parse the CBOR payload into a SpookyValue once.
fn make_spooky_value() -> SpookyValue {
    let cbor_val: ciborium::Value = ciborium::from_reader(BENCH_CBOR).unwrap();
    SpookyValue::from(cbor_val)
}

/// Get a pre-serialized binary buffer for reading/mutation benchmarks.
fn make_binary() -> Vec<u8> {
    SpookyRecord::serialize(&make_spooky_value()).unwrap()
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 1: Creating SpookyRecord
// ═══════════════════════════════════════════════════════════════════════════

fn bench_creating_spooky_record(c: &mut Criterion) {
    let mut group = c.benchmark_group("creating_spooky_record");

    // 1a. Full pipeline: CBOR → SpookyValue → SpookyRecord::serialize
    group.bench_function("SpookyRecord::serialize", |b| {
        b.iter(|| {
            let cbor_val: ciborium::Value =
                ciborium::from_reader(black_box(BENCH_CBOR)).unwrap();
            let sv = SpookyValue::from(cbor_val);
            SpookyRecord::serialize(black_box(&sv)).unwrap()
        })
    });

    // 1b. Full pipeline: CBOR → SpookyValue → SpookyRecordMut::from_spooky_value
    group.bench_function("SpookyRecordMut::from_spooky_value", |b| {
        b.iter(|| {
            let cbor_val: ciborium::Value =
                ciborium::from_reader(black_box(BENCH_CBOR)).unwrap();
            let sv = SpookyValue::from(cbor_val);
            SpookyRecordMut::from_spooky_value(black_box(&sv)).unwrap()
        })
    });

    // 2. SpookyRecordMut::new_empty
    group.bench_function("SpookyRecordMut::new_empty", |b| {
        b.iter(|| SpookyRecordMut::new_empty())
    });

    // 3. SpookyRecordMut::from_vec
    let binary = make_binary();
    group.bench_function("SpookyRecordMut::from_vec", |b| {
        b.iter(|| SpookyRecordMut::from_vec(black_box(binary.clone())).unwrap())
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 2: Reading Values
// ═══════════════════════════════════════════════════════════════════════════

fn bench_reading_values(c: &mut Criterion) {
    let mut group = c.benchmark_group("reading_values");

    let binary = make_binary();
    let record = SpookyRecord::from_bytes(&binary).unwrap();
    let mut_record = SpookyRecordMut::from_vec(binary.clone()).unwrap();

    // ── SpookyRecord (immutable) getters ──

    group.bench_function("SpookyRecord::get_field", |b| {
        b.iter(|| record.get_field(black_box("name")))
    });

    group.bench_function("SpookyRecord::get_str", |b| {
        b.iter(|| record.get_str(black_box("name")))
    });

    group.bench_function("SpookyRecord::get_i64", |b| {
        b.iter(|| record.get_i64(black_box("age")))
    });

    group.bench_function("SpookyRecord::get_bool", |b| {
        b.iter(|| record.get_bool(black_box("active")))
    });

    // ── SpookyRecordMut getters ──

    group.bench_function("SpookyRecordMut::get_field", |b| {
        b.iter(|| mut_record.get_field(black_box("name")))
    });

    group.bench_function("SpookyRecordMut::get_str", |b| {
        b.iter(|| mut_record.get_str(black_box("name")))
    });

    group.bench_function("SpookyRecordMut::get_i64", |b| {
        b.iter(|| mut_record.get_i64(black_box("age")))
    });

    group.bench_function("SpookyRecordMut::get_u64", |b| {
        b.iter(|| mut_record.get_u64(black_box("age")))
    });

    group.bench_function("SpookyRecordMut::get_f64", |b| {
        b.iter(|| mut_record.get_f64(black_box("age")))
    });

    group.bench_function("SpookyRecordMut::get_bool", |b| {
        b.iter(|| mut_record.get_bool(black_box("active")))
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 3: Set Values
// ═══════════════════════════════════════════════════════════════════════════

fn bench_set_values(c: &mut Criterion) {
    let mut group = c.benchmark_group("set_values");

    let binary = make_binary();

    // Helper: create a record with extra typed fields for benchmarking
    let make_typed_record = || -> SpookyRecordMut {
        let mut rec = SpookyRecordMut::from_vec(binary.clone()).unwrap();
        // Add fields of each type so we can benchmark set_* on matching types
        rec.add_field("bench_u64", &SpookyValue::from(100u64)).unwrap();
        rec.add_field("bench_f64", &SpookyValue::from(3.14f64)).unwrap();
        rec.add_field("bench_bool", &SpookyValue::from(true)).unwrap();
        rec
    };

    group.bench_function("set_i64", |b| {
        let mut rec = make_typed_record();
        b.iter(|| rec.set_i64(black_box("age"), black_box(42)).unwrap())
    });

    group.bench_function("set_u64", |b| {
        let mut rec = make_typed_record();
        b.iter(|| rec.set_u64(black_box("bench_u64"), black_box(42)).unwrap())
    });

    group.bench_function("set_f64", |b| {
        let mut rec = make_typed_record();
        b.iter(|| rec.set_f64(black_box("bench_f64"), black_box(42.5)).unwrap())
    });

    group.bench_function("set_bool", |b| {
        let mut rec = make_typed_record();
        b.iter(|| rec.set_bool(black_box("bench_bool"), black_box(true)).unwrap())
    });

    group.bench_function("set_str (same len)", |b| {
        let mut rec = make_typed_record();
        // "Alice" → "Bobby" (5 bytes each)
        b.iter(|| rec.set_str(black_box("name"), black_box("Bobby")).unwrap())
    });

    group.bench_function("set_str (diff len)", |b| {
        let mut rec = make_typed_record();
        let long = "Alice Modified Name";
        let short = "Al";
        let mut toggle = false;
        b.iter(|| {
            let val = if toggle { short } else { long };
            toggle = !toggle;
            rec.set_str(black_box("name"), black_box(val)).unwrap()
        })
    });

    group.bench_function("set_str_exact", |b| {
        let mut rec = make_typed_record();
        b.iter(|| {
            rec.set_str_exact(black_box("name"), black_box("Bobby"))
                .unwrap()
        })
    });

    group.bench_function("set_field", |b| {
        let mut rec = make_typed_record();
        let val = SpookyValue::from(99i64);
        b.iter(|| rec.set_field(black_box("age"), black_box(&val)).unwrap())
    });

    group.bench_function("set_null", |b| {
        let mut rec = make_typed_record();
        b.iter(|| rec.set_null(black_box("age")).unwrap())
    });

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 4: Field Migration (add/remove)
// ═══════════════════════════════════════════════════════════════════════════

fn bench_field_migration(c: &mut Criterion) {
    let mut group = c.benchmark_group("field_migration");

    let binary = make_binary();

    group.bench_function("add_field", |b| {
        b.iter_batched(
            || SpookyRecordMut::from_vec(binary.clone()).unwrap(),
            |mut rec| {
                rec.add_field(
                    black_box("new_bench_field"),
                    black_box(&SpookyValue::from(12345i64)),
                )
                .unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("remove_field", |b| {
        b.iter_batched(
            || SpookyRecordMut::from_vec(binary.clone()).unwrap(),
            |mut rec| {
                rec.remove_field(black_box("name")).unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ─── Criterion Main ─────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_creating_spooky_record,
    bench_reading_values,
    bench_set_values,
    bench_field_migration,
);
criterion_main!(benches);
