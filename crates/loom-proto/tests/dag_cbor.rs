use loom_proto::{
    DAG_CBOR_CODEC, RAW_CODEC, Value, cid_for_hash, decode, encode, parse_reference, reference,
};
use serde_json::json;

#[test]
fn tool_completed_matches_exact_dag_cbor_golden() {
    let hash = format!("77c1{}", "00".repeat(30));
    let event =
        json!({"t":"ToolCompleted","id":"a1","output":reference(&hash,DAG_CBOR_CODEC).unwrap()});
    let golden = hex::decode(format!(
        "a361746d546f6f6c436f6d706c65746564626964626131666f7574707574d82a58250001711e2077c1{}",
        "00".repeat(30)
    ))
    .unwrap();
    assert_eq!(encode(&event).unwrap(), golden);
    assert_eq!(decode::<Value>(&golden).unwrap(), event);
    let cid = event["output"]["$ref"].as_str().unwrap();
    let address = parse_reference(cid).unwrap();
    assert_eq!(address.hash, hash);
    assert_eq!(address.codec, DAG_CBOR_CODEC);
    assert_ne!(cid_for_hash(&hash, RAW_CODEC).unwrap(), cid);
}

#[test]
fn map_order_and_integral_number_representation_do_not_change_identity() {
    let first = json!({"aa":1.0,"z":{"long":2,"a":3},"a":4});
    let second = json!({"a":4,"z":{"a":3,"long":2},"aa":1});
    assert_eq!(encode(&first).unwrap(), encode(&second).unwrap());
    assert_eq!(encode(&1.0f64).unwrap(), vec![1]);
    assert_eq!(
        decode::<Value>(&hex::decode("fb3ff0000000000000").unwrap()).unwrap(),
        json!(1.0)
    );
    assert_eq!(encode(&1e20f64).unwrap()[0], 0xfb);
    assert_eq!(decode::<f64>(&encode(&1e20f64).unwrap()).unwrap(), 1e20);
    assert_eq!(
        encode(&1.5f64).unwrap(),
        hex::decode("fb3ff8000000000000").unwrap()
    );
}

#[test]
fn malformed_and_noncanonical_inputs_are_rejected() {
    for invalid in [
        "1800",               // nonminimal integer
        "a2616200616100",     // wrong map order
        "a2616100616101",     // duplicate key
        "9f01ff",             // indefinite list
        "bf616101ff",         // indefinite map
        "f93c00",             // half float
        "fa3f800000",         // single float
        "fb8000000000000000", // negative zero
        "fb7ff0000000000000", // infinity
        "fb7ff8000000000000", // NaN
        "1b0020000000000000", // unsafe integer
        "3b0020000000000000", // unsafe negative integer
        "0000",               // trailing data
        "d82b00",             // forbidden tag
        "d82a4100",           // invalid CID
        "d82a58250101711e2077c100000000000000000000000000000000000000000000000000000000000000", // wrong identity prefix
        "a1642472656663616263", // untagged ref
    ] {
        assert!(
            decode::<Value>(&hex::decode(invalid).unwrap()).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn reserved_references_and_numeric_limits_are_enforced() {
    for value in [
        json!({"$ref":"abc"}),
        json!({"$ref":1}),
        json!({"$ref":"abc","extra":true}),
        json!({"nested":[{"$ref":"bad"}]}),
    ] {
        assert!(encode(&value).is_err());
    }
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0] {
        assert!(encode(&invalid).is_err());
    }
    assert!(encode(&9_007_199_254_740_992u64).is_err());
    assert!(encode(&-9_007_199_254_740_992i64).is_err());
    assert!(cid_for_hash("abcd", RAW_CODEC).is_err());
}

#[test]
fn nested_links_roundtrip_without_restricting_general_cid_codecs() {
    let link = reference(&"12".repeat(32), 0x0129).unwrap();
    let value = json!({"children":[link.clone(),{"again":link}]});
    assert_eq!(decode::<Value>(&encode(&value).unwrap()).unwrap(), value);
    let bytes = hex::decode(
        "a16178d82a58250001551e201212121212121212121212121212121212121212121212121212121212121212",
    )
    .unwrap();
    assert_eq!(
        decode::<Value>(&bytes).unwrap()["x"],
        reference(&"12".repeat(32), RAW_CODEC).unwrap()
    );
}

#[test]
fn json_null_and_optional_struct_fields_roundtrip() {
    let value = json!({"null":null,"items":[null,{"inner":null}]});
    assert_eq!(decode::<Value>(&encode(&value).unwrap()).unwrap(), value);
    assert_eq!(encode(&Value::Null).unwrap(), vec![0xf6]);
}

#[test]
fn every_json_primitive_and_depth_boundary_is_preserved() {
    for value in [
        Value::Null,
        json!(true),
        json!(false),
        json!("hello"),
        json!(42),
        json!(-42),
        json!(1.5),
        json!([]),
        json!({}),
        json!([null, true, "s", 42, {}, []]),
    ] {
        assert_eq!(decode::<Value>(&encode(&value).unwrap()).unwrap(), value);
    }
    let mut nested = vec![0x81; 257];
    nested.push(0xf6);
    assert!(decode::<Value>(&nested).is_err());
    let mut boundary = vec![0x81; 64];
    boundary.push(0xf6);
    assert!(decode::<Value>(&boundary).is_ok());
}

#[test]
fn raw_byte_strings_are_rejected_but_integer_arrays_remain_values() {
    struct RawBytes {
        bytes: Vec<u8>,
    }
    impl serde::Serialize for RawBytes {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(&self.bytes)
        }
    }
    assert!(encode(&RawBytes { bytes: vec![1, 2] }).is_err());
    assert!(decode::<Vec<u8>>(&[0x42, 1, 2]).is_err());
    assert!(decode::<Value>(&[0xa1, 0x61, b'x', 0x40]).is_err());
    let bytes = vec![1u8, 2];
    assert_eq!(encode(&bytes).unwrap(), vec![0x82, 1, 2]);
    assert_eq!(decode::<Vec<u8>>(&[0x82, 1, 2]).unwrap(), bytes);
}
