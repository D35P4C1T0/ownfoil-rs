//! Read extended-CTR counter buckets without loading the NCA or patch table.
//! Layout: NSZ's Fs/Bktr.py and Fs/BaseFs.py; counters replace bytes 4..8.
use super::{Entry, Section, crypt_ctr, reader, u32_at, u64_at};
use anyhow::{Context, ensure};
use std::{fs::File, io::Read};

const PAGE: u64 = 0x4000;
const MAX_SECTIONS: usize = 99_990;

fn page(file: &File, entry: &Entry, base: &Section, offset: u64) -> anyhow::Result<Vec<u8>> {
    ensure!(offset <= base.size && PAGE <= base.size - offset, "BKTR page exceeds section");
    let absolute = base.offset.checked_add(offset).context("BKTR offset overflow")?;
    let region = Entry {
        name: String::new(),
        offset: entry.offset.checked_add(absolute).context("BKTR offset overflow")?,
        size: PAGE,
    };
    let mut bytes = vec![0; usize::try_from(PAGE)?];
    reader(file, &region)?.read_exact(&mut bytes)?;
    crypt_ctr(&mut bytes, absolute, &base.key, base.counter);
    Ok(bytes)
}

pub(super) fn sections(
    file: &File,
    entry: &Entry,
    base: &Section,
    patch: &[u8],
) -> anyhow::Result<Vec<Section>> {
    let offset = u64_at(patch, 0x20)?;
    let size = u64_at(patch, 0x28)?;
    if size == 0 {
        return Ok(vec![base.clone()]);
    }
    ensure!(patch.get(0x30..0x34) == Some(b"BKTR"), "Invalid BKTR magic");
    ensure!(u32_at(patch, 0x34)? == 1, "Unsupported BKTR version");
    ensure!(
        offset <= base.size && size <= base.size - offset && size >= 2 * PAGE,
        "BKTR table exceeds section"
    );
    let expected = usize::try_from(u32_at(patch, 0x38)?)?;
    ensure!((1..=MAX_SECTIONS).contains(&expected), "Invalid BKTR entry count");
    let root = page(file, entry, base, offset)?;
    let buckets = u32_at(&root, 4)?;
    ensure!(
        buckets > 0
            && u64::from(buckets) <= (PAGE - 16) / 8
            && (u64::from(buckets) + 1) * PAGE <= size,
        "Invalid BKTR bucket count"
    );
    let data_end = u64_at(&root, 8)?;
    ensure!(data_end > 0 && data_end <= offset, "Invalid BKTR data boundary");
    let mut result = Vec::new();
    let mut previous_end = 0;
    for bucket in 0..buckets {
        let bytes = page(file, entry, base, offset + (u64::from(bucket) + 1) * PAGE)?;
        let count = u32_at(&bytes, 4)?;
        let end = u64_at(&bytes, 8)?;
        ensure!(
            count > 0 && u64::from(count) <= (PAGE - 16) / 16,
            "Invalid BKTR bucket entry count"
        );
        ensure!(result.len() + usize::try_from(count)? <= expected, "BKTR entry count mismatch");
        ensure!(previous_end < end && end <= data_end, "Invalid BKTR bucket boundary");
        let first = u64_at(&bytes, 16)?;
        ensure!(
            first == previous_end && first == u64_at(&root, 16 + usize::try_from(bucket)? * 8)?,
            "Noncontiguous BKTR buckets"
        );
        for index in 0..count {
            let at = 16 + usize::try_from(index)? * 16;
            let start = u64_at(&bytes, at)?;
            let next = if index + 1 == count { end } else { u64_at(&bytes, at + 16)? };
            ensure!(
                start >= previous_end && start < next && next <= end,
                "Invalid BKTR subsection bounds"
            );
            let mut counter = base.counter;
            counter[4..8].copy_from_slice(&u32_at(&bytes, at + 12)?.to_be_bytes());
            result.push(Section {
                offset: base.offset + start,
                size: next - start,
                crypto: 4,
                key: base.key,
                counter,
            });
        }
        previous_end = end;
    }
    ensure!(result.len() == expected, "BKTR entry count/boundary mismatch");
    // Bucket tables and any trailing metadata use the filesystem's normal counter.
    result.push(Section {
        offset: base.offset + previous_end,
        size: base.size - previous_end,
        ..base.clone()
    });
    Ok(result)
}
