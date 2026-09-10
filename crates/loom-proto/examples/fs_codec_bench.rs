//! Measure the exact directory payloads consumed by all/fork scans of a fixture.
use loom_proto::{DirEntry, EntryKind, decode, decode_host, encode, encode_host};
use std::{collections::VecDeque, fs, hint::black_box, io, path::PathBuf, time::Instant};

struct Directory {
    entries: Vec<DirEntry>,
    bytes: Vec<u8>,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: fs_codec_bench FIXTURE")?,
    )
    .canonicalize()?;
    let mut pending = VecDeque::from([root.clone()]);
    let mut directories = Vec::new();
    let mut files = 0;
    let mut directory_count = 0;
    let mut max_name_bytes = 0;
    while let Some(path) = pending.pop_front() {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let kind = if metadata.is_file() {
                files += 1;
                EntryKind::File
            } else if metadata.is_dir() {
                directory_count += 1;
                pending.push_back(entry.path());
                EntryKind::Directory
            } else if metadata.file_type().is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::Other
            };
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| io::Error::other("non-UTF8 fixture name"))?;
            max_name_bytes = max_name_bytes.max(name.len());
            let size = if kind == EntryKind::File {
                metadata.len()
            } else {
                0
            };
            entries.push(DirEntry { name, size, kind });
        }
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let bytes = encode_host(&entries)?;
        assert_eq!(
            bytes,
            encode(&entries)?,
            "typed encoding differs from canonical encoder"
        );
        assert_eq!(
            decode::<Vec<DirEntry>>(&bytes)?,
            entries,
            "strict admission differs"
        );
        assert_eq!(
            decode_host::<Vec<DirEntry>>(&bytes)?,
            entries,
            "typed decoding differs"
        );
        directories.push(Directory { entries, bytes });
    }
    if !((files == 10000 && directory_count == 256) || (files == 10001 && directory_count == 257)) {
        return Err(format!("expected scan fixture10000/256 (or active mutation10001/257), got {files}/{directory_count}").into());
    }
    if fs::symlink_metadata(root.join("nested/middle/deep/alpha.dat"))?.len() != 8193 {
        return Err("standard tied-winner fixture control failed".into());
    }
    let wire_bytes: usize = directories
        .iter()
        .map(|directory| directory.bytes.len())
        .sum();
    const ROUNDS: usize = 200;
    for _ in 0..10 {
        for directory in &directories {
            black_box(encode_host(&directory.entries)?);
        }
    }
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for directory in &directories {
            black_box(encode_host(black_box(&directory.entries))?);
        }
    }
    let encode_ms = start.elapsed().as_secs_f64() * 1000.0 / ROUNDS as f64;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for directory in &directories {
            black_box(decode_host::<Vec<DirEntry>>(black_box(&directory.bytes))?);
        }
    }
    let decode_ms = start.elapsed().as_secs_f64() * 1000.0 / ROUNDS as f64;
    let checks = [encode_ms < 1.0, decode_ms < 1.0, wire_bytes < 200000];
    let passed = checks.into_iter().filter(|pass| *pass).count();
    println!(
        "{}",
        serde_json::json!({"fixture":root,"files":files,"directories":directory_count,"list_results":directories.len(),"max_basename_bytes":max_name_bytes,"wire_bytes":wire_bytes,"encode_ms":encode_ms,"decode_ms":decode_ms,"rounds":ROUNDS,"canonical_and_typed_roundtrip_controls":true})
    );
    println!("{passed}/3 typed codec gates pass");
    if passed != 3 {
        return Err("typed codec budget failed".into());
    }
    Ok(())
}
