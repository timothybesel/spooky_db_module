mod spooky_record;
mod spooky_record_mut;
mod spooky_value;

use spooky_record::serialize_record;
use spooky_value::SpookyValue;

const TEST_CBOR: &[u8] = &[
    166, 99, 97, 103, 101, 24, 28, 106, 99, 114, 101, 97, 116, 101, 100, 95, 97, 116, 116, 50, 48,
    50, 52, 45, 48, 49, 45, 49, 53, 84, 49, 48, 58, 51, 48, 58, 48, 48, 90, 101, 101, 109, 97, 105,
    108, 113, 97, 108, 105, 99, 101, 64, 101, 120, 97, 109, 112, 108, 101, 46, 99, 111, 109, 98,
    105, 100, 107, 117, 115, 101, 114, 58, 97, 98, 99, 49, 50, 51, 100, 110, 97, 109, 101, 101, 65,
    108, 105, 99, 101, 103, 112, 114, 111, 102, 105, 108, 101, 162, 102, 97, 118, 97, 116, 97, 114,
    107, 104, 116, 116, 112, 115, 58, 47, 47, 46, 46, 46, 99, 98, 105, 111, 105, 68, 101, 118, 101,
    108, 111, 112, 101, 114,
];

fn main() {
    println!("=== WRITE PATH: CBOR → SpookyValue → Hybrid Binary ===\n");

    // Step 1: Parse CBOR bytes (simulates SurrealDB input)
    let cbor_val: ciborium::Value = ciborium::from_reader(TEST_CBOR).unwrap();
    let spooky = SpookyValue::from(cbor_val);
    println!("Parsed SpookyValue:\n{:#?}\n", spooky);

    // Step 2: Serialize to hybrid binary format
    let binary = serialize_record(&spooky).expect("serialize_record failed");
    println!("Hybrid binary: {} bytes\n", binary.len());

    println!("=== READ PATH: Zero-copy field access ===\n");

    // Step 3: Wrap binary as HybridRecord (zero-copy, no parsing)
    let record = spooky_record::SpookyRecord::from_bytes(&binary).unwrap();
    println!("Field count: {}\n", record.field_count());

    // Typed accessors — no SpookyValue allocation
    println!("--- Typed zero-alloc reads ---");
    println!("  age  (i64): {:?}", record.get_i64("age"));
    println!("  name (str): {:?}", record.get_str("name"));
    println!("  email(str): {:?}", record.get_str("email"));
    println!("  id   (str): {:?}", record.get_str("id"));

    // SpookyValue accessor — allocates only for the requested field
    println!("\n--- Selective SpookyValue reads ---");
    println!("  profile: {:#?}", record.get_field("profile"));
    println!("  age:     {:#?}", record.get_field("age"));

    // Iterate all raw fields (zero-copy)
    println!("\n--- Raw field iteration ---");
    for field in record.iter_fields() {
        println!(
            "  hash={:016x} tag={} len={}",
            field.name_hash,
            field.type_tag,
            field.data.len()
        );
    }

    println!("\n=== MUTATE PATH: Zero-alloc updates ===\n");

    // Step 4: Create mutable record from the same binary
    // Note: We clone binary because we used it immutably above.
    let mut rec_mut = spooky_record_mut::SpookyRecordMut::from_vec(binary.clone()).unwrap();

    // Modify existing field (same type, same size -> fast path)
    rec_mut.set_i64("age", 31).unwrap();
    println!("Updated age: {:?}", rec_mut.get_i64("age"));

    // Modify string (different size -> splice path)
    // "Alice" -> "Alice Modified"
    rec_mut.set_str("name", "Alice Modified").unwrap();
    println!("Updated name: {:?}", rec_mut.get_str("name"));

    // Add new field
    rec_mut.add_field("new_field", &SpookyValue::from(12345u64)).unwrap();
    println!("Added field 'new_field': {:?}", rec_mut.get_u64("new_field"));

    println!("Final size: {} bytes", rec_mut.byte_len());
}
