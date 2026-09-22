//! CARv1 framing for Loom bundles: one DAG-CBOR header naming the root CID,
//! then `varint(len) || CID bytes || block bytes` per block. Hashing lives in
//! `loom-store`; this module only frames and parses bytes.
use crate::{Value, decode, encode};
use ipld_core::cid::Cid;

/// CAR container version written in the header.
pub const CAR_VERSION: u64 = 1;
/// Longest accepted unsigned varint: nine bytes carry sixty-three bits.
const MAX_VARINT_BYTES: usize = 9;

/// One framed block: the CID as printed by `Cid::to_string` and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub cid: String,
    pub bytes: Vec<u8>,
}

/// Header plus blocks, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Car {
    pub roots: Vec<String>,
    pub frames: Vec<Frame>,
}

/// Frame `frames` after a header naming `root`. Frames are written in the
/// order given; callers fix the order so equal inputs produce equal bytes.
pub fn encode_car(root: &str, frames: &[Frame]) -> Result<Vec<u8>, String> {
    parse_cid(root)?;
    let header = encode(&serde_json::json!({"roots":[{"$ref":root}],"version":CAR_VERSION}))?;
    let mut out = Vec::new();
    write_varint(&mut out, header.len() as u64);
    out.extend_from_slice(&header);
    for frame in frames {
        let cid = parse_cid(&frame.cid)?.to_bytes();
        let length = cid.len() + frame.bytes.len();
        write_varint(&mut out, length as u64);
        out.extend_from_slice(&cid);
        out.extend_from_slice(&frame.bytes);
    }
    Ok(out)
}

/// Parse a CARv1 file. Every frame must be complete, every varint minimal,
/// and every CID must re-encode to the bytes it was read from.
pub fn decode_car(bytes: &[u8]) -> Result<Car, String> {
    let mut position = 0usize;
    let header = read_frame(bytes, &mut position)?.ok_or("bundle has no header")?;
    let header: Value = decode(header)?;
    if header.get("version") != Some(&Value::from(CAR_VERSION)) {
        return Err(format!("bundle header version must be {CAR_VERSION}"));
    }
    let roots = header
        .get("roots")
        .and_then(Value::as_array)
        .ok_or("bundle header has no roots")?
        .iter()
        .map(|root| {
            root.get("$ref")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "bundle header root must be a CID link".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if header.as_object().is_some_and(|fields| fields.len() != 2) {
        return Err("bundle header carries unknown fields".into());
    }
    let mut frames = Vec::new();
    while let Some(frame) = read_frame(bytes, &mut position)? {
        let mut reader = frame;
        let cid = Cid::read_bytes(&mut reader).map_err(|error| error.to_string())?;
        let consumed = frame.len() - reader.len();
        if cid.to_bytes() != frame[..consumed] {
            return Err(format!(
                "bundle block {cid} has a noncanonical CID encoding"
            ));
        }
        frames.push(Frame {
            cid: cid.to_string(),
            bytes: reader.to_vec(),
        });
    }
    Ok(Car { roots, frames })
}

fn parse_cid(cid: &str) -> Result<Cid, String> {
    cid.parse::<Cid>()
        .map_err(|error: ipld_core::cid::Error| format!("invalid CID {cid}: {error}"))
}

fn read_frame<'a>(bytes: &'a [u8], position: &mut usize) -> Result<Option<&'a [u8]>, String> {
    if *position == bytes.len() {
        return Ok(None);
    }
    let length = read_varint(bytes, position)?;
    if length == 0 {
        return Err("bundle frame is empty".into());
    }
    let length = usize::try_from(length).map_err(|_| "bundle frame length overflow")?;
    let end = position
        .checked_add(length)
        .ok_or("bundle frame length overflow")?;
    if end > bytes.len() {
        return Err(format!(
            "bundle frame at byte {position} claims {length} bytes but {} remain",
            bytes.len() - *position
        ));
    }
    let frame = &bytes[*position..end];
    *position = end;
    Ok(Some(frame))
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for index in 0..MAX_VARINT_BYTES {
        let byte = *bytes.get(*position).ok_or("bundle varint is truncated")?;
        *position += 1;
        let group = u64::from(byte & 0x7f);
        if index == MAX_VARINT_BYTES - 1 && group > 1 {
            return Err("bundle varint exceeds 63 bits".into());
        }
        value |= group << (7 * index);
        if byte & 0x80 == 0 {
            if index > 0 && group == 0 {
                return Err("bundle varint is not minimal".into());
            }
            return Ok(value);
        }
    }
    Err("bundle varint exceeds nine bytes".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DAG_CBOR_CODEC, RAW_CODEC, cid_for_hash};

    fn cid(seed: u8, codec: u64) -> String {
        cid_for_hash(&format!("{seed:02x}").repeat(32), codec).unwrap()
    }

    #[test]
    fn frames_round_trip_in_order() {
        let frames = vec![
            Frame {
                cid: cid(1, DAG_CBOR_CODEC),
                bytes: encode(&serde_json::json!({"loom_bundle":1})).unwrap(),
            },
            Frame {
                cid: cid(2, RAW_CODEC),
                bytes: b"pub fn main() {}".to_vec(),
            },
            Frame {
                cid: cid(3, RAW_CODEC),
                bytes: vec![0; 300],
            },
        ];
        let bytes = encode_car(&frames[0].cid, &frames).unwrap();
        let car = decode_car(&bytes).unwrap();
        assert_eq!(car.roots, vec![frames[0].cid.clone()]);
        assert_eq!(car.frames, frames);
        // varint(len) for the 300-byte frame spans two bytes.
        assert_eq!(read_varint(&[0xac, 0x02], &mut 0).unwrap(), 300);
    }

    #[test]
    fn truncated_and_malformed_files_are_rejected() {
        let frames = vec![Frame {
            cid: cid(2, RAW_CODEC),
            bytes: b"payload".to_vec(),
        }];
        let bytes = encode_car(&cid(1, DAG_CBOR_CODEC), &frames).unwrap();
        let truncated = &bytes[..bytes.len() - 1];
        assert!(decode_car(truncated).unwrap_err().contains("claims"));
        assert!(decode_car(&[]).unwrap_err().contains("no header"));
        assert!(decode_car(&[0x00]).unwrap_err().contains("empty"));
        // A non-minimal varint (0x80 0x00 == 0) is refused before its frame.
        assert!(
            read_varint(&[0x80, 0x00], &mut 0)
                .unwrap_err()
                .contains("minimal")
        );
        assert!(read_varint(&[0xff; 10], &mut 0).is_err());
        assert!(encode_car("not-a-cid", &frames).is_err());
        let mut wrong_version = bytes.clone();
        // Header version byte is the last byte of the header map value.
        let header_length = wrong_version[0] as usize;
        wrong_version[header_length] = 2;
        assert!(decode_car(&wrong_version).is_err());
    }
}
