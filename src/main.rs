mod spooky_record;
mod spooky_record_mut;
mod spooky_value;

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

    // Step 2: Serialize to hybrid binary
    println!("\n--- Serializing to binary ---");
    let binary = spooky_record::SpookyRecord::serialize(&spooky).unwrap();
    println!("Hybrid binary: {} bytes\n", binary.len());

    println!("=== READ PATH: Zero-copy field access ===\n");

    // Step 3: Wrap binary as SpookyRecord (zero-copy, no parsing)
    let record = spooky_record::SpookyRecord::from_bytes(&binary).unwrap();
    println!("Field count: {}\n", record.field_count());

    // Typed accessors — no SpookyValue allocation
    println!("--- Typed zero-alloc reads ---");
    println!("  age  (i64): {:?}", record.get_i64("age"));
    println!("  name (str): {:?}", record.get_str("name"));
    println!("  email(str): {:?}", record.get_str("email"));
    println!("  id   (str): {:?}", record.get_str("id"));

    // SpookyValue accessor — allocates only for    // 2. Serialize to hybrid binary
    // Using the new static method on SpookyRecord
    println!("\n--- Serializing to binary ---");
    let binary = spooky_record::SpookyRecord::serialize(&spooky).unwrap();
    println!("Binary size: {} bytes", binary.len());

    // 3. Read back (zero-copy)
    println!("\n--- Reading from SpookyRecord (zero-copy) ---");
    let record = spooky_record::SpookyRecord::from_bytes(&binary).unwrap();
    println!("Field count: {}", record.field_count());
    println!("id: {:?}", record.get_str("id")); // Some("user:123")
    println!("age: {:?}", record.get_i64("age")); // Some(30)
    println!("active: {:?}", record.get_bool("active")); // Some(true)

    // Demonstrate new parity method
    println!("Type of 'age': {:?}", record.field_type("age")); // Some(2) -> TAG_I64

    // 4. Mutation
    println!("\n--- Mutating with SpookyRecordMut ---");
    let mut mut_record = spooky_record_mut::SpookyRecordMut::from_vec(binary).unwrap();

    // Modify in-place
    mut_record.set_i64("age", 31).unwrap();
    println!("Updated age: {:?}", mut_record.get_i64("age"));

    // Modify string (splice if length changes)
    mut_record
        .set_str("name", "Alice Modified")
        .unwrap();
    println!("Updated name: {:?}", mut_record.get_str("name"));

    // Add new field
    mut_record
        .add_field("new_field", &SpookyValue::from(12345))
        .unwrap();
    println!(
        "Added field 'new_field': {:?}",
        mut_record.get_i64("new_field")
    );

    // Verify parity method in Mut
    println!("get_number_as_f64('score'): {:?}", mut_record.get_number_as_f64("score"));

    println!("Final size: {} bytes", mut_record.byte_len());
}
