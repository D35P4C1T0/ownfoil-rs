//! Bounded PFS0/HFS0 readers and streaming container rebuilding.
use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub offset: u64,
    pub size: u64,
}
#[derive(Clone, Debug)]
pub struct Archive {
    pub offset: u64,
    pub hashed: bool,
    pub entries: Vec<Entry>,
}

pub fn number<const N: usize>(bytes: &[u8], at: usize) -> anyhow::Result<[u8; N]> {
    bytes.get(at..at + N).context("Truncated container header")?.try_into().map_err(Into::into)
}
pub fn u32_at(bytes: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(number(bytes, at)?))
}
pub fn u64_at(bytes: &[u8], at: usize) -> anyhow::Result<u64> {
    Ok(u64::from_le_bytes(number(bytes, at)?))
}

pub fn parse(file: &mut File, offset: u64, size: u64) -> anyhow::Result<Archive> {
    ensure!(size >= 16, "Truncated archive");
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 16];
    file.read_exact(&mut header)?;
    ensure!(&header[..4] == b"PFS0" || &header[..4] == b"HFS0", "Invalid archive magic");
    let hashed = &header[..4] == b"HFS0";
    let count = u32_at(&header, 4)? as usize;
    let strings = u32_at(&header, 8)? as usize;
    ensure!(count <= 100_000 && strings <= 16 * 1024 * 1024, "Archive header exceeds limits");
    let stride = if hashed { 64 } else { 24 };
    let header_size = 16 + count * stride + strings;
    ensure!(header_size as u64 <= size, "Archive header exceeds file size");
    let mut rest = vec![0; header_size - 16];
    file.read_exact(&mut rest)?;
    let names = &rest[count * stride..];
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for i in 0..count {
        let row = &rest[i * stride..(i + 1) * stride];
        let start = u64_at(row, 0)?;
        let length = u64_at(row, 8)?;
        let name_at = u32_at(row, 16)? as usize;
        let tail = names.get(name_at..).context("Invalid archive name offset")?;
        let end = tail.iter().position(|byte| *byte == 0).context("Unterminated archive name")?;
        let name = std::str::from_utf8(&tail[..end])?.to_owned();
        ensure!(
            !name.is_empty()
                && !name.contains(['/', '\\'])
                && name != "."
                && name != ".."
                && seen.insert(name.clone()),
            "Unsafe or duplicate archive member"
        );
        let relative =
            (header_size as u64).checked_add(start).context("Archive offset overflow")?;
        ensure!(
            relative <= size && length <= size - relative,
            "Archive member exceeds container bounds"
        );
        entries.push(Entry {
            name,
            offset: offset.checked_add(relative).context("Archive offset overflow")?,
            size: length,
        });
    }
    let mut ranges = entries.iter().map(|e| (e.offset, e.offset + e.size)).collect::<Vec<_>>();
    ranges.sort_unstable();
    ensure!(ranges.windows(2).all(|p| p[0].1 <= p[1].0), "Overlapping archive entries");
    Ok(Archive { offset, hashed, entries })
}

pub fn root(path: &Path) -> anyhow::Result<(File, Archive)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut prefix = [0; 4];
    file.read_exact(&mut prefix)?;
    let offset = if &prefix == b"PFS0" {
        0
    } else {
        ensure!(size >= 0x200, "Truncated XCI header");
        file.seek(SeekFrom::Start(0x100))?;
        file.read_exact(&mut prefix)?;
        ensure!(&prefix == b"HEAD", "Unsupported container");
        file.seek(SeekFrom::Start(0x130))?;
        let mut raw = [0; 8];
        file.read_exact(&mut raw)?;
        u64::from_le_bytes(raw)
    };
    ensure!(offset < size, "Invalid XCI partition offset");
    let archive = parse(&mut file, offset, size - offset)?;
    Ok((file, archive))
}

pub fn leaves(file: &mut File, archive: &Archive) -> anyhow::Result<Vec<Entry>> {
    let mut result = Vec::new();
    for entry in &archive.entries {
        if archive.offset > 0
            && matches!(entry.name.as_str(), "secure" | "normal" | "update" | "logo")
        {
            let child = parse(file, entry.offset, entry.size)?;
            result.extend(child.entries);
        } else {
            result.push(entry.clone());
        }
    }
    Ok(result)
}

pub fn reader(file: &File, entry: &Entry) -> anyhow::Result<std::io::Take<File>> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(entry.offset))?;
    Ok(file.take(entry.size))
}
pub fn copy(file: &File, entry: &Entry, out: &mut File) -> anyhow::Result<()> {
    ensure!(std::io::copy(&mut reader(file, entry)?, out)? == entry.size, "Truncated member");
    Ok(())
}
pub fn hash(file: &File, entry: &Entry) -> anyhow::Result<[u8; 32]> {
    let mut input = reader(file, entry)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    ensure!(input.limit() == 0, "Truncated member");
    Ok(hasher.finalize().into())
}

pub fn rebuild<F>(
    source: &mut File,
    archive: &Archive,
    out: &mut File,
    convert: &mut F,
) -> anyhow::Result<u64>
where
    F: FnMut(&File, &Entry, &mut File) -> anyhow::Result<String>,
{
    let start = out.stream_position()?;
    // Extensions retain their length, so the table size is known before conversion.
    let stride = if archive.hashed { 64 } else { 24 };
    let strings = archive.entries.iter().map(|e| e.name.len() + 1).sum::<usize>();
    let header_size = 16 + stride * archive.entries.len() + strings;
    out.write_all(&vec![0; header_size])?;
    let mut records = Vec::new();
    for entry in &archive.entries {
        let pos = out.stream_position()?;
        let name = if archive.offset > 0
            && matches!(entry.name.as_str(), "secure" | "normal" | "update" | "logo")
        {
            let child = parse(source, entry.offset, entry.size)?;
            rebuild(source, &child, out, convert)?;
            entry.name.clone()
        } else {
            convert(source, entry, out)?
        };
        ensure!(name.len() == entry.name.len(), "Unexpected renamed member length");
        let size = out.stream_position()? - pos;
        records.push((name, pos - start - header_size as u64, size, pos));
    }
    let end = out.stream_position()?;
    let mut header = Vec::with_capacity(header_size);
    header.extend_from_slice(if archive.hashed { b"HFS0" } else { b"PFS0" });
    header.extend_from_slice(&u32::try_from(records.len())?.to_le_bytes());
    header.extend_from_slice(&u32::try_from(strings)?.to_le_bytes());
    header.extend_from_slice(&[0; 4]);
    let mut name_offset = 0u32;
    for (name, offset, size, pos) in &records {
        header.extend_from_slice(&offset.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&name_offset.to_le_bytes());
        if archive.hashed {
            let count = (*size).min(0x200);
            header.extend_from_slice(&(count as u32).to_le_bytes());
            header.extend_from_slice(&[0; 8]);
            header.extend_from_slice(&hash(
                out,
                &Entry { name: String::new(), offset: *pos, size: count },
            )?);
        } else {
            header.extend_from_slice(&[0; 4]);
        }
        name_offset += u32::try_from(name.len() + 1)?;
    }
    for (name, _, _, _) in records {
        header.extend_from_slice(name.as_bytes());
        header.push(0);
    }
    out.seek(SeekFrom::Start(start))?;
    out.write_all(&header)?;
    out.seek(SeekFrom::Start(end))?;
    Ok(header_size as u64)
}

#[cfg(test)]
// Synthetic fixtures are bounded to a few bytes.
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    fn container(hashed: bool, members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(if hashed { b"HFS0" } else { b"PFS0" });
        header.extend_from_slice(&(members.len() as u32).to_le_bytes());
        let strings: usize = members.iter().map(|(name, _)| name.len() + 1).sum();
        header.extend_from_slice(&(strings as u32).to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        let mut offset = 0u64;
        let mut name_offset = 0u32;
        for (name, bytes) in members {
            header.extend_from_slice(&offset.to_le_bytes());
            header.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            header.extend_from_slice(&name_offset.to_le_bytes());
            header.extend_from_slice(&[0; 4]);
            if hashed {
                header.extend_from_slice(&[0; 40]);
            }
            offset += bytes.len() as u64;
            name_offset += name.len() as u32 + 1;
        }
        for (name, _) in members {
            header.extend_from_slice(name.as_bytes());
            header.push(0);
        }
        for (_, bytes) in members {
            header.extend_from_slice(bytes);
        }
        header
    }

    #[test]
    fn rebuild_preserves_nsp_and_nested_xci_members() -> anyhow::Result<()> {
        for xci in [false, true] {
            let dir = tempfile::tempdir()?;
            let source = dir.path().join("source");
            let payload = container(xci, &[("content.nca", b"content"), ("ticket.tik", b"ticket")]);
            let bytes = if xci {
                let mut header = vec![0; 0x200];
                header[0x100..0x104].copy_from_slice(b"HEAD");
                header[0x130..0x138].copy_from_slice(&0x200u64.to_le_bytes());
                header.extend(container(true, &[("secure", &payload)]));
                header
            } else {
                payload
            };
            std::fs::write(&source, bytes)?;
            let (mut input, archive) = root(&source)?;
            let target = dir.path().join("target");
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&target)?;
            if xci {
                copy(&input, &Entry { name: String::new(), offset: 0, size: 0x200 }, &mut output)?;
            }
            rebuild(&mut input, &archive, &mut output, &mut |input, entry, output| {
                copy(input, entry, output)?;
                Ok(entry.name.replace(".nca", ".ncz"))
            })?;
            let (mut output, archive) = root(&target)?;
            let entries = leaves(&mut output, &archive)?;
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "content.ncz");
            let mut bytes = Vec::new();
            reader(&output, &entries[0])?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"content");
            bytes.clear();
            reader(&output, &entries[1])?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"ticket");
        }
        Ok(())
    }

    #[test]
    fn rejects_unsafe_names_overlaps_and_out_of_bounds_members() -> anyhow::Result<()> {
        let mut file = tempfile::tempfile()?;
        for members in [
            vec![("../escape", b"a".as_slice())],
            vec![("same", b"a".as_slice()), ("same", b"b".as_slice())],
        ] {
            let bytes = container(false, &members);
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(&bytes)?;
            assert!(parse(&mut file, 0, bytes.len() as u64).is_err());
        }
        let mut bytes = container(false, &[("one", b"a"), ("two", b"b")]);
        bytes[40..48].copy_from_slice(&0u64.to_le_bytes());
        file.rewind()?;
        file.write_all(&bytes)?;
        assert!(parse(&mut file, 0, bytes.len() as u64).is_err());
        bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        file.rewind()?;
        file.write_all(&bytes)?;
        assert!(parse(&mut file, 0, bytes.len() as u64).is_err());
        Ok(())
    }
}
