//! Native NCZ section transform, solid Zstandard and independently compressed blocks.
use super::archive::{Entry, number, reader, u32_at, u64_at};
use crate::settings::CompressionSettings;
use aes::cipher::{BlockEncrypt, KeyInit};
use anyhow::{Context, ensure};
use nx_archive::formats::{Keyset, nca::Nca, title_keyset::TitleKeys};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};
const HEADER: u64 = 0x4000;
#[derive(Clone)]
struct Section {
    offset: u64,
    size: u64,
    crypto: u64,
    key: [u8; 16],
    counter: [u8; 16],
}

fn crypt(bytes: &mut [u8], offset: u64, section: &Section) {
    if !matches!(section.crypto, 3 | 4) {
        return;
    }
    crypt_ctr(bytes, offset, &section.key, section.counter);
}

pub(super) fn crypt_ctr(bytes: &mut [u8], offset: u64, key: &[u8; 16], counter: [u8; 16]) {
    let cipher = aes::Aes128::new(key.into());
    let mut at = 0;
    while at < bytes.len() {
        let position = offset + at as u64;
        let mut block_counter = counter;
        block_counter[8..].copy_from_slice(&(position >> 4).to_be_bytes());
        let mut block: aes::Block = block_counter.into();
        cipher.encrypt_block(&mut block);
        let skip = (position % 16) as usize;
        let count = (16 - skip).min(bytes.len() - at);
        for i in 0..count {
            bytes[at + i] ^= block[skip + i];
        }
        at += count;
    }
}

fn sections(
    file: &File,
    entry: &Entry,
    keys: &Keyset,
    title_keys: &TitleKeys,
) -> anyhow::Result<Vec<Section>> {
    let mut input = file.try_clone()?;
    input.seek(SeekFrom::Start(entry.offset))?;
    let mut header = vec![0; 0xC00];
    input.read_exact(&mut header)?;
    let nca = Nca::from_reader(std::io::Cursor::new(header), keys, Some(title_keys))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    ensure!(nca.header.content_size == entry.size, "NCA size mismatch");
    let key = nca.get_aes_ctr_decrypt_key().ok();
    let mut result = Vec::new();
    let mut cursor = HEADER;
    let entries = nca.header.fs_entries.iter().filter(|e| e.end_offset > e.start_offset);
    for (fs, header) in entries.zip(&nca.fs_headers) {
        let start = (u64::from(fs.start_offset) * 0x200).max(HEADER);
        let end = u64::from(fs.end_offset) * 0x200;
        if end <= HEADER {
            continue;
        }
        ensure!(start >= cursor && end <= entry.size, "Invalid NCA section bounds");
        if start > cursor {
            result.push(Section {
                offset: cursor,
                size: start - cursor,
                crypto: 1,
                key: [0; 16],
                counter: [0; 16],
            });
        }
        // Extended CTR has per-subsection counters. Preserve those encrypted bytes verbatim.
        // This remains a valid NCZ and avoids changing unsupported crypto layouts.
        let crypto = if header.encryption_type as u8 == 3 && key.is_some() { 3 } else { 1 };
        let mut counter = [0; 16];
        counter[..8].copy_from_slice(&header.ctr.to_be_bytes());
        result.push(Section {
            offset: start,
            size: end - start,
            crypto,
            key: key.unwrap_or([0; 16]),
            counter,
        });
        cursor = end;
    }
    if cursor < entry.size {
        result.push(Section {
            offset: cursor,
            size: entry.size - cursor,
            crypto: 1,
            key: [0; 16],
            counter: [0; 16],
        });
    }
    ensure!(!result.is_empty(), "NCA has no data sections");
    Ok(result)
}

struct PlainReader {
    input: std::io::Take<File>,
    sections: Vec<Section>,
    position: u64,
}
impl Read for PlainReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some(section) = self
            .sections
            .iter()
            .find(|s| self.position >= s.offset && self.position < s.offset + s.size)
        else {
            return Ok(0);
        };
        let count = buffer.len().min(
            usize::try_from(section.offset + section.size - self.position).unwrap_or(usize::MAX),
        );
        let n = self.input.read(&mut buffer[..count])?;
        crypt(&mut buffer[..n], self.position, section);
        self.position += n as u64;
        Ok(n)
    }
}
fn encoder<W: Write>(
    out: W,
    settings: &CompressionSettings,
) -> anyhow::Result<structured_zstd::encoding::StreamingEncoder<W>> {
    use structured_zstd::encoding::{CompressionLevel, CompressionParameters, StreamingEncoder};
    let level = CompressionLevel::from_level(settings.level);
    let parameters = CompressionParameters::builder(level)
        .enable_long_distance_matching(settings.long_distance)
        .build()
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let mut encoder = StreamingEncoder::new(out, level);
    encoder.set_parameters(&parameters)?;
    Ok(encoder)
}

pub fn compress(
    file: &File,
    entry: &Entry,
    out: &mut File,
    keys: &Keyset,
    title_keys: &TitleKeys,
    settings: &CompressionSettings,
    block: bool,
) -> anyhow::Result<()> {
    ensure!(entry.size > HEADER, "NCA too small to compress");
    ensure!((14..=32).contains(&settings.block_size_exponent), "Invalid NCZ block size");
    let sections = sections(file, entry, keys, title_keys)?;
    let mut input = reader(file, entry)?;
    let mut header = vec![0; 0x4000];
    input.read_exact(&mut header)?;
    out.write_all(&header)?;
    out.write_all(b"NCZSECTN")?;
    out.write_all(&(sections.len() as u64).to_le_bytes())?;
    for section in &sections {
        for n in [section.offset, section.size, section.crypto, 0] {
            out.write_all(&n.to_le_bytes())?;
        }
        out.write_all(&section.key)?;
        out.write_all(&section.counter)?;
    }
    let mut plain = PlainReader { input, sections, position: HEADER };
    if block {
        let block_size = 1u64 << settings.block_size_exponent;
        let total = entry.size - HEADER;
        let count = total.div_ceil(block_size);
        ensure!(count <= 1_000_000, "Too many NCZ blocks");
        out.write_all(b"NCZBLOCK")?;
        out.write_all(&[2, 1, 0, u8::try_from(settings.block_size_exponent)?])?;
        out.write_all(&u32::try_from(count)?.to_le_bytes())?;
        out.write_all(&total.to_le_bytes())?;
        let table = out.stream_position()?;
        out.write_all(&vec![
            0;
            usize::try_from(count)?
                .checked_mul(4)
                .context("NCZ block table too large")?
        ])?;
        let mut sizes = Vec::new();
        let mut left = total;
        let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let requested =
            if settings.threads == 0 { available } else { usize::try_from(settings.threads)? };
        // Bound simultaneous buffers to 128 MiB; a single larger configured block
        // still needs its own buffer. Read serially because cloned files share offsets.
        let memory_workers = usize::try_from((128 * 1024 * 1024u64 / block_size).max(1))?;
        let workers = requested.max(1).min(memory_workers).min(available);
        while left > 0 {
            let mut batch = Vec::new();
            for _ in 0..workers {
                if left == 0 {
                    break;
                }
                let length = usize::try_from(left.min(block_size))
                    .context("NCZ block exceeds address space")?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(length)?;
                bytes.resize(length, 0);
                plain.read_exact(&mut bytes)?;
                batch.push(bytes);
                left -= length as u64;
            }
            let compressed = std::thread::scope(|scope| -> anyhow::Result<Vec<Vec<u8>>> {
                #[allow(clippy::needless_collect)]
                // Spawn all jobs before joining; lazy iteration would serialize them.
                let handles = batch
                    .into_iter()
                    .map(|bytes| {
                        scope.spawn(move || -> anyhow::Result<Vec<u8>> {
                            let mut compressed = encoder(Vec::new(), settings)?;
                            compressed.write_all(&bytes)?;
                            let compressed = compressed.finish()?;
                            Ok(if compressed.len() < bytes.len() { compressed } else { bytes })
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .map_err(|_| anyhow::anyhow!("NCZ compression worker failed"))?
                    })
                    .collect()
            })?;
            for bytes in compressed {
                sizes.push(u32::try_from(bytes.len()).context("NCZ block too large")?);
                out.write_all(&bytes)?;
            }
        }
        let end = out.stream_position()?;
        out.seek(SeekFrom::Start(table))?;
        for size in sizes {
            out.write_all(&size.to_le_bytes())?;
        }
        out.seek(SeekFrom::Start(end))?;
    } else {
        let mut encoder = encoder(out, settings)?;
        ensure!(
            std::io::copy(&mut plain, &mut encoder)? == entry.size - HEADER,
            "Truncated NCA payload"
        );
        encoder.finish()?;
    }
    Ok(())
}

struct Blocks {
    file: File,
    position: u64,
    sizes: Vec<u32>,
    index: usize,
    remaining: u64,
    block_size: u64,
    current: Option<Box<dyn Read>>,
    current_left: u64,
}
impl Read for Blocks {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.current_left == 0 {
            if self.remaining == 0 {
                return Ok(0);
            }
            let size = u64::from(
                *self
                    .sizes
                    .get(self.index)
                    .ok_or_else(|| std::io::Error::other("Missing NCZ block"))?,
            );
            let length = self.remaining.min(self.block_size);
            self.file.seek(SeekFrom::Start(self.position))?;
            let input = self.file.try_clone()?.take(size);
            self.current = Some(if size < length {
                Box::new(
                    structured_zstd::decoding::StreamingDecoder::new(input)
                        .map_err(std::io::Error::other)?,
                )
            } else {
                Box::new(input)
            });
            self.position += size;
            self.index += 1;
            self.current_left = length;
            self.remaining -= length;
        }
        let length = buffer.len().min(usize::try_from(self.current_left).unwrap_or(usize::MAX));
        let n = self
            .current
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Missing NCZ stream"))?
            .read(&mut buffer[..length])?;
        if n == 0 {
            return Err(std::io::Error::other("Truncated NCZ block"));
        }
        self.current_left -= n as u64;
        if self.current_left == 0 {
            let mut extra = [0];
            if self
                .current
                .as_mut()
                .ok_or_else(|| std::io::Error::other("Missing NCZ stream"))?
                .read(&mut extra)?
                != 0
            {
                return Err(std::io::Error::other("NCZ block exceeds declared size"));
            }
        }
        Ok(n)
    }
}

pub fn decompress(file: &File, entry: &Entry, out: &mut dyn Write) -> anyhow::Result<u64> {
    ensure!(entry.size > HEADER + 16, "Truncated NCZ");
    let mut input = reader(file, entry)?;
    let mut header = vec![0; 0x4000];
    input.read_exact(&mut header)?;
    out.write_all(&header)?;
    let mut prefix = [0; 16];
    input.read_exact(&mut prefix)?;
    ensure!(&prefix[..8] == b"NCZSECTN", "Invalid NCZ magic");
    let count = u64_at(&prefix, 8)?;
    ensure!((1..=100_000).contains(&count), "Invalid NCZ section count");
    let mut sections = Vec::new();
    let mut end = HEADER;
    for _ in 0..count {
        let mut raw = [0; 64];
        input.read_exact(&mut raw)?;
        let section = Section {
            offset: u64_at(&raw, 0)?,
            size: u64_at(&raw, 8)?,
            crypto: u64_at(&raw, 16)?,
            key: number(&raw, 32)?,
            counter: number(&raw, 48)?,
        };
        ensure!(matches!(section.crypto, 1 | 3 | 4), "Unsupported NCZ section crypto");
        let section_end = section.offset.checked_add(section.size).context("NCZ size overflow")?;
        let start = section.offset.max(HEADER);
        ensure!(start >= end && section_end >= start, "Overlapping NCZ sections");
        if start > end {
            sections.push(Section {
                offset: end,
                size: start - end,
                crypto: 1,
                key: [0; 16],
                counter: [0; 16],
            });
        }
        end = section_end;
        sections.push(section);
    }
    let payload = entry.offset + entry.size - input.limit();
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    let mut stream: Box<dyn Read> = if &magic == b"NCZBLOCK" {
        let mut raw = [0; 16];
        input.read_exact(&mut raw)?;
        ensure!(raw[0] == 2 && (14..=32).contains(&raw[3]), "Unsupported NCZ block header");
        let count = u32_at(&raw, 4)? as usize;
        let total = u64_at(&raw, 8)?;
        let block_size = 1u64 << raw[3];
        ensure!(
            count <= 1_000_000
                && total == end - HEADER
                && count as u64 == total.div_ceil(block_size),
            "Invalid NCZ block count/size"
        );
        let mut sizes = Vec::new();
        let mut sum = 0u64;
        for index in 0..count {
            let mut bytes = [0; 4];
            input.read_exact(&mut bytes)?;
            let size = u32::from_le_bytes(bytes);
            let length = (total - index as u64 * block_size).min(block_size);
            ensure!(size > 0 && u64::from(size) <= length, "Invalid NCZ block length");
            sum += u64::from(size);
            sizes.push(size);
        }
        ensure!(sum <= input.limit(), "NCZ blocks exceed archive member");
        Box::new(Blocks {
            file: file.try_clone()?,
            position: entry.offset + entry.size - input.limit(),
            sizes,
            index: 0,
            remaining: total,
            block_size,
            current: None,
            current_left: 0,
        })
    } else {
        let mut source = file.try_clone()?;
        source.seek(SeekFrom::Start(payload))?;
        Box::new(structured_zstd::decoding::StreamingDecoder::new(
            source.take(entry.offset + entry.size - payload),
        )?)
    };
    let mut buffer = vec![0; 1024 * 1024];
    for section in sections {
        let mut offset = section.offset.max(HEADER);
        while offset < section.offset + section.size {
            let count = buffer
                .len()
                .min(usize::try_from(section.offset + section.size - offset).unwrap_or(usize::MAX));
            stream.read_exact(&mut buffer[..count])?;
            crypt(&mut buffer[..count], offset, &section);
            out.write_all(&buffer[..count])?;
            offset += count as u64;
        }
    }
    let mut extra = [0];
    ensure!(stream.read(&mut extra)? == 0, "NCZ payload exceeds declared size");
    Ok(end)
}

#[cfg(test)]
// Synthetic fixtures have fixed, small sizes.
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;
    fn fixture(ctr: bool) -> anyhow::Result<(tempfile::NamedTempFile, Entry, Keyset, Vec<u8>)> {
        let mut keys = Keyset { header_key_cache: Some([0x17; 32]), ..Default::default() };
        keys.raw_keys.insert("key_area_key_application_00".into(), vec![0x11; 16]);
        let size = 0x24000usize;
        let mut bytes = vec![0; size];
        bytes[0x200..0x204].copy_from_slice(b"NCA3");
        bytes[0x208..0x210].copy_from_slice(&(size as u64).to_le_bytes());
        bytes[0x240..0x244].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[0x244..0x248].copy_from_slice(&((size / 0x200) as u32).to_le_bytes());
        bytes[0x400] = 2;
        bytes[0x402] = 1;
        bytes[0x403] = 2;
        bytes[0x404] = if ctr { 3 } else { 1 };
        // SHA256 filesystem header has a bounded layer count; no layers are needed by the compressor.
        bytes[0x540..0x548].copy_from_slice(&7u64.to_le_bytes());
        let aes = aes::Aes128::new((&[0x11; 16]).into());
        let mut key = ([0x22; 16]).into();
        aes.encrypt_block(&mut key);
        bytes[0x320..0x330].copy_from_slice(&key);
        for (i, byte) in bytes[HEADER as usize..].iter_mut().enumerate() {
            *byte = ((i / 79) % 251) as u8;
        }
        if ctr {
            let mut counter = [0; 16];
            counter[..8].copy_from_slice(&7u64.to_be_bytes());
            crypt(
                &mut bytes[HEADER as usize..],
                HEADER,
                &Section {
                    offset: HEADER,
                    size: size as u64 - HEADER,
                    crypto: 3,
                    key: [0x22; 16],
                    counter,
                },
            );
        }
        let encrypted =
            nx_archive::formats::nca::encrypt_with_header_key(&bytes[..0xC00], &keys, 0x200, 0);
        bytes[..0xC00].copy_from_slice(&encrypted);
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&bytes)?;
        Ok((file, Entry { name: "test.nca".into(), offset: 0, size: size as u64 }, keys, bytes))
    }
    /// Optional independent oracle; the application never invokes this executable.
    #[test]
    #[ignore = "requires the reference zstd CLI for interoperability validation"]
    fn reference_zstd_interoperability() -> anyhow::Result<()> {
        let (file, entry, keys, expected) = fixture(false)?;
        let dir = tempfile::tempdir()?;
        let encoded_path = dir.path().join("encoded.zst");
        let decoded_path = dir.path().join("decoded.bin");
        let plain_path = dir.path().join("plain.bin");
        std::fs::write(&plain_path, &expected[0x4000..])?;
        for level in [1, 18] {
            let mut out = tempfile::tempfile()?;
            compress(
                file.as_file(),
                &entry,
                &mut out,
                &keys,
                &TitleKeys::new(),
                &CompressionSettings { level, long_distance: true, ..Default::default() },
                false,
            )?;
            out.rewind()?;
            let mut ncz = Vec::new();
            out.read_to_end(&mut ncz)?;
            let count = u64_at(&ncz, 0x4008)?;
            let payload = 0x4010 + usize::try_from(count)? * 64;
            std::fs::write(&encoded_path, &ncz[payload..])?;
            let status = std::process::Command::new("zstd")
                .args(["-q", "-d", "-f"])
                .arg(&encoded_path)
                .arg("-o")
                .arg(&decoded_path)
                .status()?;
            ensure!(status.success(), "Reference decoder rejected Rust frame");
            assert_eq!(std::fs::read(&decoded_path)?, expected[0x4000..]);
            let status = std::process::Command::new("zstd")
                .args(["-q", "-f", "--check", &format!("-{level}")])
                .arg(&plain_path)
                .arg("-o")
                .arg(&encoded_path)
                .status()?;
            ensure!(status.success(), "Reference encoder failed");
            ncz.truncate(payload);
            ncz.extend(std::fs::read(&encoded_path)?);
            out.set_len(0)?;
            out.rewind()?;
            out.write_all(&ncz)?;
            let mut decoded = Vec::new();
            decompress(
                &out,
                &Entry { name: "test.ncz".into(), offset: 0, size: ncz.len() as u64 },
                &mut decoded,
            )?;
            assert_eq!(decoded, expected);
        }
        Ok(())
    }

    #[test]
    fn ctr_matches_nx_archive_reader_at_unaligned_offsets() -> anyhow::Result<()> {
        let key = [0x37; 16];
        let ctr = 0x1234_5678_9abc_def0u64;
        let expected = (0..79u8).collect::<Vec<_>>();
        for offset in [0x4200u64, 0x4203, 0x420f] {
            let mut counter = [0; 16];
            counter[..8].copy_from_slice(&ctr.to_be_bytes());
            let mut encrypted = expected.clone();
            crypt_ctr(&mut encrypted, offset, &key, counter);
            let mut bytes = vec![0; usize::try_from(offset)?];
            bytes.extend(encrypted);
            bytes.extend([0; 16]);
            let mut reader = nx_archive::io::Aes128CtrReader::new(
                std::io::Cursor::new(bytes),
                offset,
                ctr,
                key.to_vec(),
            );
            let mut actual = vec![0; expected.len()];
            reader.read_exact(&mut actual)?;
            assert_eq!(actual, expected);
        }
        Ok(())
    }

    #[test]
    fn solid_and_block_roundtrip_plain_and_ctr_sections() -> anyhow::Result<()> {
        for ctr in [false, true] {
            for block in [false, true] {
                let (file, entry, keys, expected) = fixture(ctr)?;
                let mut out = tempfile::tempfile()?;
                compress(
                    file.as_file(),
                    &entry,
                    &mut out,
                    &keys,
                    &TitleKeys::new(),
                    &CompressionSettings {
                        level: 1,
                        block_size_exponent: 14,
                        ..Default::default()
                    },
                    block,
                )?;
                let length = out.metadata()?.len();
                assert!(length < entry.size / 2, "payload should actually compress");
                let mut bytes = Vec::new();
                decompress(
                    &out,
                    &Entry { name: "test.ncz".into(), offset: 0, size: length },
                    &mut bytes,
                )?;
                assert_eq!(bytes, expected);
            }
        }
        Ok(())
    }
    #[test]
    fn rejects_truncated_and_overlapping_ncz_sections() -> anyhow::Result<()> {
        let mut file = tempfile::tempfile()?;
        file.write_all(&vec![0; HEADER as usize])?;
        file.write_all(b"NCZSECTN")?;
        file.write_all(&2u64.to_le_bytes())?;
        for _ in 0..2 {
            for n in [HEADER, 100, 1, 0] {
                file.write_all(&n.to_le_bytes())?;
            }
            file.write_all(&[0; 32])?;
        }
        let size = file.metadata()?.len();
        assert!(
            decompress(&file, &Entry { name: "bad.ncz".into(), offset: 0, size }, &mut Vec::new())
                .is_err()
        );
        Ok(())
    }

    fn block_fixture(
        mutate: impl FnOnce(&mut Vec<u8>, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let (file, entry, keys, _) = fixture(false)?;
        let mut out = tempfile::tempfile()?;
        compress(
            file.as_file(),
            &entry,
            &mut out,
            &keys,
            &TitleKeys::new(),
            &CompressionSettings { level: 1, block_size_exponent: 20, ..Default::default() },
            true,
        )?;
        let mut bytes = Vec::new();
        out.rewind()?;
        out.read_to_end(&mut bytes)?;
        let count = u64_at(&bytes, 0x4008)? as usize;
        let table = 0x4010 + count * 64 + 24;
        mutate(&mut bytes, table)?;
        out.set_len(0)?;
        out.rewind()?;
        out.write_all(&bytes)?;
        out.rewind()?;
        let size = out.metadata()?.len();
        let entry = Entry { name: "bad.ncz".into(), offset: 0, size };
        assert!(decompress(&out, &entry, &mut Vec::new()).is_err());
        Ok(())
    }

    #[test]
    fn rejects_zero_length_and_oversized_ncz_blocks() -> anyhow::Result<()> {
        block_fixture(|bytes, table| {
            bytes[table..table + 4].copy_from_slice(&0u32.to_le_bytes());
            Ok(())
        })?;
        block_fixture(|bytes, table| {
            bytes[table..table + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn rejects_compressed_block_expansion_past_declared_length() -> anyhow::Result<()> {
        block_fixture(|bytes, table| {
            let total = usize::try_from(u64_at(bytes, table - 8)?)?;
            let mut stream =
                encoder(Vec::new(), &CompressionSettings { level: 1, ..Default::default() })?;
            stream.write_all(&vec![0x42; total + 1])?;
            let compressed = stream.finish()?;
            bytes[table..table + 4]
                .copy_from_slice(&u32::try_from(compressed.len())?.to_le_bytes());
            bytes.truncate(table + 4);
            bytes.extend(compressed);
            Ok(())
        })
    }

    #[test]
    fn rejects_raw_block_larger_than_remaining_output() -> anyhow::Result<()> {
        block_fixture(|bytes, table| {
            let total = usize::try_from(u64_at(bytes, table - 8)?)?;
            bytes[table..table + 4].copy_from_slice(&u32::try_from(total + 1)?.to_le_bytes());
            bytes.resize(table + 4 + total + 1, 0);
            Ok(())
        })
    }

    #[test]
    fn rejects_invalid_compression_exponent_before_writing() -> anyhow::Result<()> {
        let (file, entry, keys, _) = fixture(false)?;
        for exponent in [0, 13, 33, 64, u32::MAX] {
            let mut out = tempfile::tempfile()?;
            assert!(
                compress(
                    file.as_file(),
                    &entry,
                    &mut out,
                    &keys,
                    &TitleKeys::new(),
                    &CompressionSettings { block_size_exponent: exponent, ..Default::default() },
                    true
                )
                .is_err()
            );
            assert_eq!(out.metadata()?.len(), 0);
        }
        Ok(())
    }

    #[test]
    fn rejects_ncz_block_total_mismatch() -> anyhow::Result<()> {
        block_fixture(|bytes, table| {
            let total = u64_at(bytes, table - 8)?;
            bytes[table - 8..table].copy_from_slice(&(total + 1).to_le_bytes());
            Ok(())
        })?;
        Ok(())
    }
}
