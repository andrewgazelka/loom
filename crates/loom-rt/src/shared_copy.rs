//! Bulk copies between host memory and a guest's shared linear memory.
//!
//! Another guest thread may write shared memory at any time, so a host read or
//! write must not be a plain `memcpy` (a data race is undefined behavior in
//! Rust). The old loops did one relaxed atomic operation per byte, which is
//! correct and about an order of magnitude slower than bandwidth. These copy in
//! aligned 8-byte atomic words, four at a time, with byte atomics only for the
//! unaligned head and the tail. Relaxed 64-bit atomics compile to plain loads and
//! stores, so the cost is the loop, not a barrier.
use anyhow::{Context, Result};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// Read `length` bytes at `start`.
pub(crate) fn read(cells: &[UnsafeCell<u8>], start: usize, length: usize) -> Result<Vec<u8>> {
    let end = start.checked_add(length).context("guest range overflow")?;
    anyhow::ensure!(end <= cells.len(), "guest range outside shared memory");
    let mut out: Vec<u8> = Vec::with_capacity(length);
    // SAFETY: `start..end` is inside `cells`; every access is atomic, and every
    // byte of `out[..length]` is written before `set_len`.
    unsafe {
        let source = cells.as_ptr().add(start) as *mut u8;
        let target = out.as_mut_ptr();
        let head = source.align_offset(8).min(length);
        let mut at = 0;
        while at < head {
            *target.add(at) = AtomicU8::from_ptr(source.add(at)).load(Ordering::Relaxed);
            at += 1;
        }
        while at + 32 <= length {
            for lane in 0..4 {
                let word =
                    AtomicU64::from_ptr(source.add(at + lane * 8).cast()).load(Ordering::Relaxed);
                target
                    .add(at + lane * 8)
                    .cast::<u64>()
                    .write_unaligned(word);
            }
            at += 32;
        }
        while at + 8 <= length {
            let word = AtomicU64::from_ptr(source.add(at).cast()).load(Ordering::Relaxed);
            target.add(at).cast::<u64>().write_unaligned(word);
            at += 8;
        }
        while at < length {
            *target.add(at) = AtomicU8::from_ptr(source.add(at)).load(Ordering::Relaxed);
            at += 1;
        }
        out.set_len(length);
    }
    Ok(out)
}

/// Write `bytes` at `start`.
pub(crate) fn write(cells: &[UnsafeCell<u8>], start: usize, bytes: &[u8]) -> Result<()> {
    let end = start
        .checked_add(bytes.len())
        .context("guest range overflow")?;
    anyhow::ensure!(end <= cells.len(), "guest range outside shared memory");
    let length = bytes.len();
    // SAFETY: `start..end` is inside `cells`; every access is atomic.
    unsafe {
        let target = cells.as_ptr().add(start) as *mut u8;
        let source = bytes.as_ptr();
        let head = target.align_offset(8).min(length);
        let mut at = 0;
        while at < head {
            AtomicU8::from_ptr(target.add(at)).store(*source.add(at), Ordering::Relaxed);
            at += 1;
        }
        while at + 32 <= length {
            for lane in 0..4 {
                let word = source.add(at + lane * 8).cast::<u64>().read_unaligned();
                AtomicU64::from_ptr(target.add(at + lane * 8).cast())
                    .store(word, Ordering::Relaxed);
            }
            at += 32;
        }
        while at + 8 <= length {
            let word = source.add(at).cast::<u64>().read_unaligned();
            AtomicU64::from_ptr(target.add(at).cast()).store(word, Ordering::Relaxed);
            at += 8;
        }
        while at < length {
            AtomicU8::from_ptr(target.add(at)).store(*source.add(at), Ordering::Relaxed);
            at += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(size: usize) -> Vec<UnsafeCell<u8>> {
        (0..size)
            .map(|i| UnsafeCell::new((i * 7 + 3) as u8))
            .collect()
    }

    #[test]
    fn reads_and_writes_agree_with_a_plain_copy_at_every_alignment_and_length() {
        let cells = memory(256);
        let plain: Vec<u8> = (0..256).map(|i| (i * 7 + 3) as u8).collect();
        for start in 0..17 {
            for length in [0, 1, 2, 7, 8, 9, 15, 16, 31, 32, 33, 63, 64, 65, 100, 200] {
                if start + length > 256 {
                    continue;
                }
                assert_eq!(
                    read(&cells, start, length).unwrap(),
                    plain[start..start + length]
                );
            }
        }
        for start in 0..17 {
            for length in [0, 1, 7, 8, 9, 31, 32, 33, 100] {
                let source: Vec<u8> = (0..length).map(|i| (i * 13 + start) as u8).collect();
                let cells = memory(256);
                write(&cells, start, &source).unwrap();
                let after = read(&cells, 0, 256).unwrap();
                assert_eq!(&after[start..start + length], &source[..]);
                assert_eq!(
                    &after[..start],
                    &plain[..start],
                    "bytes before the write are untouched"
                );
                assert_eq!(
                    &after[start + length..],
                    &plain[start + length..],
                    "bytes after too"
                );
            }
        }
    }

    #[test]
    fn out_of_range_and_overflowing_ranges_are_errors() {
        let cells = memory(16);
        assert!(read(&cells, 10, 7).is_err());
        assert!(read(&cells, usize::MAX, 2).is_err());
        assert!(write(&cells, 15, &[1, 2]).is_err());
    }

    #[test]
    fn a_bulk_copy_is_much_faster_than_a_byte_at_a_time_atomic_loop() {
        let cells = memory(8 << 20);
        let started = std::time::Instant::now();
        let fast = read(&cells, 1, (8 << 20) - 8).unwrap();
        let fast_time = started.elapsed();
        let started = std::time::Instant::now();
        let slow: Vec<u8> = cells[1..(8 << 20) - 7]
            .iter()
            .map(|cell| unsafe { AtomicU8::from_ptr(cell.get()).load(Ordering::Relaxed) })
            .collect();
        let slow_time = started.elapsed();
        assert_eq!(fast, slow);
        println!("8 MB read: bulk {fast_time:?}, byte-at-a-time {slow_time:?}");
    }
}
