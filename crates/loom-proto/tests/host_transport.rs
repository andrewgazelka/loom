use loom_proto::{DirEntry, EntryKind, Value, decode, decode_host, encode, encode_host, encode_host_array, reference, DAG_CBOR_CODEC};
use serde::{Deserialize, Serialize};

#[test]
fn filesystem_entries_are_named_rust_fields_and_exact_wire_arrays() {
    let entry = DirEntry { name: "a.rs".into(), size: 42, kind: EntryKind::File };
    let bytes = encode_host(&entry).unwrap();
    assert_eq!(bytes, hex::decode("8364612e7273182a6466696c65").unwrap());
    assert_eq!(bytes, encode(&entry).unwrap());
    assert_eq!(decode_host::<DirEntry>(&bytes).unwrap(), entry);
    assert_eq!(decode::<DirEntry>(&bytes).unwrap(), entry);
    assert_eq!(decode::<Value>(&bytes).unwrap(), serde_json::json!(["a.rs",42,"file"]));
    for kind in [EntryKind::Directory, EntryKind::Symlink, EntryKind::Other] {
        let entry = DirEntry { name: "entry".into(), size: 0, kind };
        assert_eq!(decode_host::<DirEntry>(&encode_host(&entry).unwrap()).unwrap(), entry);
    }
}

#[test]
fn typed_entry_shape_and_numeric_admission_are_checked() {
    for value in [
        serde_json::json!(["a",0]), serde_json::json!(["a",0,"file",false]),
        serde_json::json!(["a",0,"unknown"]), serde_json::json!(["a",-1,"file"]),
        serde_json::json!({"name":"a","size":0,"kind":"file"}),
    ] { assert!(decode_host::<DirEntry>(&encode(&value).unwrap()).is_err()); }
    let entry = DirEntry { name: "huge".into(), size: 9_007_199_254_740_992, kind: EntryKind::File };
    assert!(encode_host(&entry).is_err());
    let bytes = hex::decode("8361611b00200000000000006466696c65").unwrap();
    assert!(decode_host::<DirEntry>(&bytes).is_err());
}

#[test]
fn host_decode_preserves_links_nulls_enums_and_newtypes_without_a_tree() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Choice { Empty, Fields { entries: Vec<DirEntry>, link: Value } }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Message { choice: Choice, optional: Option<String>, count: u64 }
    let message = Message {
        choice: Choice::Fields {
            entries: vec![DirEntry { name: "a".into(), size: 1, kind: EntryKind::File }],
            link: reference(&"ab".repeat(32), DAG_CBOR_CODEC).unwrap(),
        }, optional: None, count: 4,
    };
    let bytes = encode(&message).unwrap();
    assert_eq!(decode_host::<Message>(&bytes).unwrap(), message);
    assert_eq!(decode_host::<Value>(&bytes).unwrap(), decode::<Value>(&bytes).unwrap());
    assert_eq!(decode_host::<Choice>(&encode(&Choice::Empty).unwrap()).unwrap(), Choice::Empty);
}

#[test]
fn scheduler_aggregates_canonical_bytes_without_rebuilding_children() {
    for count in [0,1,23,24,255,256] {
        let values: Vec<Value> = (0..count).map(|index| serde_json::json!([index,"item"])).collect();
        let encoded: Vec<Vec<u8>> = values.iter().map(|value| encode(value).unwrap()).collect();
        let joined = encode_host_array(encoded.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(joined, encode(&values).unwrap());
        assert_eq!(decode_host::<Vec<Value>>(&joined).unwrap(), values);
    }
}

#[test]
fn strict_foreign_admission_remains_strict() {
    for invalid in ["1800", "a2616100616101", "a2616200616100", "fb8000000000000000", "1b0020000000000000", "a1642472656663616263"] {
        assert!(decode::<Value>(&hex::decode(invalid).unwrap()).is_err(), "accepted {invalid}");
    }
    let mut bytes = encode(&serde_json::json!({"x":[1,2,3]})).unwrap();
    bytes.push(0);
    assert!(decode_host::<Value>(&bytes).is_err());
    let mut deep = vec![0x81;257]; deep.push(0xf6);
    assert!(decode_host::<Value>(&deep).is_err());
    assert!(decode_host::<Value>(&[0x41,0]).is_err());
}
