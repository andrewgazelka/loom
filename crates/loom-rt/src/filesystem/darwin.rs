use super::{DirEntry, EntryKind, File, Result};
use anyhow::{Context, ensure};
use std::os::fd::AsRawFd;

fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated getattrlistbulk field")?
            .try_into()
            .unwrap(),
    ))
}

fn record(bytes: &[u8]) -> Result<DirEntry> {
    // Packed attrlist fields have four-byte alignment, including the u64 size.
    // RETURNED_ATTRS is first; requested common attributes follow bit order.
    ensure!(bytes.len() >= 36, "truncated getattrlistbulk record");
    let common = word(bytes, 4)?;
    let file = word(bytes, 16)?;
    ensure!(
        common & (libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE)
            == libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE,
        "getattrlistbulk omitted name or type"
    );
    let name_offset = i32::from_ne_bytes(bytes[24..28].try_into().unwrap());
    let name_length = word(bytes, 28)? as usize;
    let name_start = 24_i64
        .checked_add(i64::from(name_offset))
        .context("name reference overflow")?;
    let name_start = usize::try_from(name_start).context("negative name reference")?;
    let name_end = name_start
        .checked_add(name_length)
        .context("name length overflow")?;
    let fixed_length = if file & libc::ATTR_FILE_DATALENGTH != 0 {
        44
    } else {
        36
    };
    ensure!(
        name_start >= fixed_length && name_length > 1,
        "invalid getattrlistbulk name reference"
    );
    let name = bytes
        .get(name_start..name_end)
        .context("getattrlistbulk name outside record")?;
    ensure!(
        name.last() == Some(&0) && !name[..name.len() - 1].contains(&0),
        "invalid getattrlistbulk name terminator"
    );
    let name =
        std::str::from_utf8(&name[..name.len() - 1]).context("directory entry name is not UTF8")?;
    ensure!(
        !name.contains('/') && name != "." && name != "..",
        "invalid getattrlistbulk name"
    );
    // vnode.h's stable vtype enum: VREG=1, VDIR=2, VLNK=5.
    let kind = match word(bytes, 32)? {
        1 => EntryKind::File,
        2 => EntryKind::Directory,
        5 => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    let size = if kind == EntryKind::File {
        ensure!(
            file & libc::ATTR_FILE_DATALENGTH != 0,
            "getattrlistbulk omitted file length"
        );
        let size = i64::from_ne_bytes(
            bytes
                .get(36..44)
                .context("truncated getattrlistbulk file length")?
                .try_into()
                .unwrap(),
        );
        u64::try_from(size).context("negative getattrlistbulk file length")?
    } else {
        0
    };
    Ok(DirEntry {
        name: name.into(),
        size,
        kind,
    })
}

pub(super) fn list(directory: &File, limit: usize) -> Result<Vec<DirEntry>> {
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut entries = Vec::new();
    loop {
        // SAFETY: attrlist is initialized and buffer is writable for its length.
        let count = unsafe {
            libc::getattrlistbulk(
                directory.as_raw_fd(),
                (&mut attributes as *mut libc::attrlist).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::FSOPT_PACK_INVAL_ATTRS as u64,
            )
        };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count == 0 {
            break;
        }
        let mut offset = 0_usize;
        for _ in 0..count {
            ensure!(entries.len() < limit, "directory entry limit exceeded");
            let length = word(&buffer, offset)? as usize;
            ensure!(
                length >= 36 && length % 4 == 0,
                "invalid getattrlistbulk record length"
            );
            let end = offset
                .checked_add(length)
                .context("getattrlistbulk record overflow")?;
            entries.push(record(
                buffer
                    .get(offset..end)
                    .context("getattrlistbulk record outside buffer")?,
            )?);
            offset = end;
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_record_omits_file_attribute_group() {
        // Native getattrlistbulk returns a 36-byte prefix for directories even
        // with FSOPT_PACK_INVAL_ATTRS; file records have a 44-byte prefix.
        let mut bytes = vec![0_u8; 40];
        bytes[4..8].copy_from_slice(&(libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE).to_ne_bytes());
        bytes[24..28].copy_from_slice(&12_i32.to_ne_bytes());
        bytes[28..32].copy_from_slice(&4_u32.to_ne_bytes());
        bytes[32..36].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[36..40].copy_from_slice(b"sub\0");
        let entry = record(&bytes).unwrap();
        assert_eq!(entry.name, "sub");
        assert_eq!(entry.kind, EntryKind::Directory);
        assert_eq!(entry.size, 0);
    }

    #[test]
    fn packed_record_checks_name_bounds_and_signed_offsets() {
        let mut bytes = vec![0_u8; 48];
        bytes[4..8].copy_from_slice(&(libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE).to_ne_bytes());
        bytes[16..20].copy_from_slice(&libc::ATTR_FILE_DATALENGTH.to_ne_bytes());
        bytes[24..28].copy_from_slice(&20_i32.to_ne_bytes());
        bytes[28..32].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[32..36].copy_from_slice(&1_u32.to_ne_bytes());
        bytes[36..44].copy_from_slice(&7_i64.to_ne_bytes());
        bytes[44..46].copy_from_slice(b"x\0");
        assert_eq!(record(&bytes).unwrap().size, 7);
        bytes[24..28].copy_from_slice(&(-25_i32).to_ne_bytes());
        assert!(record(&bytes).is_err());
        bytes[24..28].copy_from_slice(&100_i32.to_ne_bytes());
        assert!(record(&bytes).is_err());
        assert!(record(&bytes[..43]).is_err());
    }
}
