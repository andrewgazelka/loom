
// ---- Loom glue: packed-bytes entry. Inputs and the output travel as store blobs (`StoreRef`), laid out as
// containers of little-endian arrays: u32 magic, u32 sections, u64 byte length per section, then each
// section padded to 8 bytes.
use loom::{Element, Packed, StoreRef};

const SKIN_MAGIC: u32 = 0x314e_4b53;
const WELD_MAGIC: u32 = 0x3144_4c57;

fn pack(magic: u32, sections: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(magic.to_le_bytes());
    out.extend((sections.len() as u32).to_le_bytes());
    for section in sections {
        out.extend((section.len() as u64).to_le_bytes());
    }
    for section in sections {
        out.extend(section);
        while out.len() % 8 != 0 {
            out.push(0);
        }
    }
    out
}

fn unpack(bytes: &[u8], magic: u32) -> Result<Vec<&[u8]>, String> {
    let word = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    if word(0) != Some(magic) {
        return Err("bad container magic".into());
    }
    let count = word(4).ok_or("short container")? as usize;
    let mut lengths = Vec::new();
    for i in 0..count {
        let at = 8 + i * 8;
        let raw = bytes.get(at..at + 8).ok_or("short container")?;
        lengths.push(u64::from_le_bytes(raw.try_into().unwrap()) as usize);
    }
    let mut at = 8 + count * 8;
    let mut sections = Vec::new();
    for length in lengths {
        sections.push(bytes.get(at..at + length).ok_or("section past the end")?);
        at += length.div_ceil(8) * 8;
    }
    Ok(sections)
}

fn cast<T: Element>(bytes: &[u8]) -> Result<Vec<T>, String> {
    Packed::<T>::from_bytes(bytes).map(|p| p.0).ok_or_else(|| "section is not whole elements".to_string())
}

fn bytes_of<T: Element>(values: &[T]) -> Vec<u8> {
    Packed::new(values.to_vec()).as_bytes().to_vec()
}

fn skin_from(bytes: &[u8]) -> Result<Skin, String> {
    let s = unpack(bytes, SKIN_MAGIC)?;
    if s.len() < 6 {
        return Err("a skin has six sections and then its targets".into());
    }
    Ok(Skin {
        positions: cast(s[0])?,
        normals: cast(s[1])?,
        uvs: cast(s[2])?,
        joints: cast(s[3])?,
        weights: cast(s[4])?,
        triangles: cast(s[5])?,
        targets: s[6..].iter().map(|t| cast(t)).collect::<Result<_, _>>()?,
    })
}

fn skin_to(skin: &Skin) -> Vec<u8> {
    let mut sections = vec![
        bytes_of(&skin.positions),
        bytes_of(&skin.normals),
        bytes_of(&skin.uvs),
        bytes_of(&skin.joints),
        bytes_of(&skin.weights),
        bytes_of(&skin.triangles),
    ];
    for target in &skin.targets {
        sections.push(bytes_of(target));
    }
    pack(SKIN_MAGIC, &sections)
}

fn indices(values: &[Option<u32>]) -> Vec<u8> {
    bytes_of(&values.iter().map(|v| v.unwrap_or(u32::MAX)).collect::<Vec<u32>>())
}

fn read_blob(reference: &StoreRef) -> Result<Vec<u8>, String> {
    let bytes = loom::kernel::get(&reference.hash).map_err(|e| e.to_string())?;
    if bytes.len() as u64 != reference.len {
        return Err(format!("blob has {} bytes, reference says {}", bytes.len(), reference.len));
    }
    Ok(bytes)
}

/// Weld the head skin onto the body skin. `params` is `Params` in field order (nine numbers).
/// Returns a reference to the packed result blob (see `pack_weld` in the native harness for the layout).
pub fn weld_blobs(head: StoreRef, body: StoreRef, params: Vec<f64>) -> Result<StoreRef, String> {
    let [uv_eps, twin_max_m, max_ring, blend_m, shape_fade_m, normal_fade_m, bridge_margin_m, bridge_cut_m, bridge_clear_m] =
        <[f64; 9]>::try_from(params).map_err(|_| "params takes nine numbers".to_string())?;
    let p = Params {
        uv_eps,
        twin_max_m,
        max_ring: max_ring as u32,
        blend_m,
        shape_fade_m,
        normal_fade_m,
        bridge_margin_m,
        bridge_cut_m,
        bridge_clear_m,
    };
    let (head, body) = (skin_from(&read_blob(&head)?)?, skin_from(&read_blob(&body)?)?);
    let w = weld(&head, &body, &p).map_err(|e| format!("{e:?}"))?;
    let r = &w.report;
    let report = [
        r.seam_ring as f64, r.seam_nodes as f64, r.gap_m[0], r.gap_m[1], r.gap_m[2], r.crossing_deg[0],
        r.crossing_deg[1], r.collar_m, r.head_moved_m, r.head_blended as f64, r.head_dropped_vertices as f64,
        r.head_dropped_triangles as f64, r.body_dropped_vertices as f64, r.body_dropped_triangles as f64,
        r.strip_triangles as f64, r.head_stray_nodes as f64, r.head_peel_rounds as f64,
    ];
    let out = pack(
        WELD_MAGIC,
        &[
            skin_to(&w.head),
            skin_to(&w.body),
            indices(&w.head_vertices),
            indices(&w.head_triangles),
            indices(&w.body_vertices),
            indices(&w.body_triangles),
            bytes_of(&report),
        ],
    );
    let handle = loom::kernel::put(&[&out]).map_err(|e| e.to_string())?;
    Ok(StoreRef { hash: handle, len: out.len() as u64 })
}
