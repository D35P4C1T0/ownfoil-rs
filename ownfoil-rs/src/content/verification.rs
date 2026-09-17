#![allow(clippy::case_sensitive_file_extension_comparisons)] // NCA member names are case-sensitive format identifiers.
//! NCA signature and content hash verification, including CNMT consistency.
use super::{
    archive::{self, Entry},
    ncz,
    public_moduli::MODULI,
};
use anyhow::{Context, ensure};
use nx_archive::formats::{
    Keyset,
    cnmt::Cnmt,
    nca::{Nca, decrypt_with_header_key},
    title_keyset::TitleKeys,
};
use rsa::{BigUint, Pss, RsaPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
};

#[derive(Default)]
struct HashWriter {
    hash: Sha256,
    header: Vec<u8>,
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.header.len() < 0xC00 {
            let n = (0xC00 - self.header.len()).min(bytes.len());
            self.header.extend_from_slice(&bytes[..n]);
        }
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn member_hash(file: &File, entry: &Entry) -> anyhow::Result<[u8; 32]> {
    member_hash_progress(file, entry, &mut |_| Ok(()))
}

fn member_hash_progress(
    file: &File,
    entry: &Entry,
    progress: &mut dyn FnMut(u64) -> std::io::Result<()>,
) -> anyhow::Result<[u8; 32]> {
    struct Sink<'a> {
        hash: HashWriter,
        progress: &'a mut dyn FnMut(u64) -> std::io::Result<()>,
    }
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            (self.progress)(bytes.len() as u64)?;
            self.hash.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink { hash: HashWriter::default(), progress };
    if entry.name.ends_with(".ncz") {
        ncz::decompress(file, entry, &mut sink)?;
    } else {
        ensure!(
            std::io::copy(&mut archive::reader(file, entry)?, &mut sink)? == entry.size,
            "Truncated member"
        );
    }
    Ok(sink.hash.hash.finalize().into())
}
pub fn title_keys(file: &File, entries: &[Entry]) -> anyhow::Result<TitleKeys> {
    let mut keys = TitleKeys::new();
    for entry in entries.iter().filter(|e| e.name.ends_with(".tik")) {
        ensure!(entry.size <= 1024 * 1024, "Oversized ticket");
        let mut bytes = Vec::new();
        archive::reader(file, entry)?.read_to_end(&mut bytes)?;
        let body = ticket_body(&bytes)?;
        ensure!(bytes.len() >= body + 0x170, "Truncated ticket");
        keys.add_title_key(
            &hex::encode(&bytes[body + 0x160..body + 0x170]),
            bytes[body + 0x40..body + 0x50].to_vec(),
        );
    }
    Ok(keys)
}
fn ticket_body(bytes: &[u8]) -> anyhow::Result<usize> {
    match archive::u32_at(bytes, 0)? {
        0x0001_0000 | 0x0001_0003 => Ok(0x240),
        0x0001_0001 | 0x0001_0004 => Ok(0x140),
        0x0001_0002 | 0x0001_0005 => Ok(0x80),
        _ => anyhow::bail!("Unknown ticket signature type"),
    }
}

pub const fn status(
    signature: Option<bool>,
    hash: Option<bool>,
    modified: Option<bool>,
) -> &'static str {
    match (signature, hash, modified) {
        (_, Some(false), Some(true)) => "MODIFIED",
        (_, Some(false), _) => "CORRUPT",
        (Some(true), Some(true), _) => "VALID",
        (Some(false), Some(true), _) => "REPACK",
        (Some(true), None, _) => "SIGNATURE_OK",
        (Some(false), None, _) => "SIGNATURE_FAILED",
        _ => "UNVERIFIED",
    }
}
fn filesystem_headers_valid(decoded: &[u8]) -> anyhow::Result<bool> {
    for index in 0..4 {
        let start = archive::u32_at(decoded, 0x240 + index * 16)?;
        let end = archive::u32_at(decoded, 0x244 + index * 16)?;
        if start == 0 && end == 0 {
            continue;
        }
        let header = decoded
            .get(0x400 + index * 0x200..0x600 + index * 0x200)
            .context("Truncated filesystem header")?;
        if start >= end
            || Sha256::digest(header).as_slice() != &decoded[0x280 + index * 32..0x2a0 + index * 32]
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn probe_key(decoded: &[u8], keys: &Keyset, titles: &TitleKeys) -> anyhow::Result<[u8; 16]> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    let generation = usize::from(decoded[0x206].max(decoded[0x220]).saturating_sub(1));
    let rights = &decoded[0x230..0x240];
    let (key, encrypted) = if rights.iter().any(|byte| *byte != 0) {
        (
            keys.get_title_kek(generation).context("Missing title encryption key")?,
            titles.get_title_key(&hex::encode(rights)).context("Missing title ticket")?.as_slice(),
        )
    } else {
        let key = match decoded[0x207] {
            0 => keys.get_key_area_key_application(generation),
            1 => keys.get_key_area_key_ocean(generation),
            2 => keys.get_key_area_key_system(generation),
            _ => None,
        }
        .context("Missing key area encryption key")?;
        (key, &decoded[0x320..0x330])
    };
    let cipher = aes::Aes128::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("Invalid encryption key length"))?;
    let mut block: aes::Block = archive::number::<16>(encrypted, 0)?.into();
    cipher.decrypt_block(&mut block);
    Ok(block.into())
}

struct Probe {
    offset: u64,
    section_end: u64,
    expected: Option<[u8; 32]>,
    counter: Option<[u8; 16]>,
    bytes: Vec<u8>,
}

// At most four 64 KiB windows, independent of reconstructed NCA size.
fn probe_layout(decoded: &[u8], content_size: u64) -> anyhow::Result<Vec<Probe>> {
    ensure!(decoded.len() >= 0xC00, "Truncated NCA header");
    let mut probes = Vec::new();
    for index in 0..4 {
        let start = u64::from(archive::u32_at(decoded, 0x240 + index * 16)?) * 0x200;
        let end = u64::from(archive::u32_at(decoded, 0x244 + index * 16)?) * 0x200;
        if start == 0 && end == 0 {
            continue;
        }
        ensure!(start >= 0xC00 && start < end && end <= content_size, "Invalid NCA section bounds");
        let fs = &decoded[0x400 + index * 0x200..0x600 + index * 0x200];
        if !matches!(fs[4], 1 | 3) || fs[0x148..0x1a0].iter().any(|byte| *byte != 0) {
            continue;
        }
        let (offset, length, expected) = match (fs[2], fs[3]) {
            (1, 2) => {
                let layers = archive::u32_at(fs, 0x2c)?;
                ensure!((2..=5).contains(&layers), "Invalid PFS0 hash layer count");
                let region = 0x30 + usize::try_from(layers - 1)? * 16;
                ensure!(archive::u64_at(fs, region + 8)? >= 16, "Truncated PFS0 region");
                (archive::u64_at(fs, region)?, 16, None)
            }
            (0, 3) if &fs[8..12] == b"IVFC" => {
                let length = archive::u64_at(fs, 0x20)?;
                ensure!(length > 0, "Empty IVFC root layer");
                if length > 64 * 1024 || archive::u32_at(fs, 0x10)? != 32 {
                    continue;
                }
                (archive::u64_at(fs, 0x18)?, length, Some(archive::number::<32>(fs, 0xc8)?))
            }
            _ => continue,
        };
        ensure!(
            offset <= end - start && length <= end - start - offset,
            "Decryption probe exceeds section bounds"
        );
        let offset = start + offset;
        let counter = if fs[4] == 3 {
            let mut counter = [0; 16];
            counter[..8].copy_from_slice(&archive::u64_at(fs, 0x140)?.to_be_bytes());
            Some(counter)
        } else {
            None
        };
        probes.push(Probe {
            offset,
            section_end: end,
            expected,
            counter,
            bytes: vec![0; usize::try_from(length)?],
        });
    }
    Ok(probes)
}

fn check_probes(
    probes: Vec<Probe>,
    decoded: &[u8],
    keys: &Keyset,
    titles: &TitleKeys,
) -> anyhow::Result<()> {
    for mut probe in probes {
        if let Some(counter) = probe.counter {
            let key = probe_key(decoded, keys, titles)?;
            ncz::crypt_ctr(&mut probe.bytes, probe.offset, &key, counter);
        }
        if let Some(hash) = probe.expected {
            ensure!(
                Sha256::digest(&probe.bytes).as_slice() == hash,
                "IVFC decryption probe failed"
            );
        } else {
            ensure!(&probe.bytes[..4] == b"PFS0", "PFS0 decryption probe failed");
            let table_size = 16
                + u64::from(archive::u32_at(&probe.bytes, 4)?) * 24
                + u64::from(archive::u32_at(&probe.bytes, 8)?);
            ensure!(
                table_size <= probe.section_end - probe.offset,
                "PFS0 table exceeds section bounds"
            );
        }
    }
    Ok(())
}

fn decryption_probes(
    file: &File,
    entry: &Entry,
    decoded: &[u8],
    keys: &Keyset,
    titles: &TitleKeys,
) -> anyhow::Result<()> {
    let mut probes = probe_layout(decoded, entry.size)?;
    for probe in &mut probes {
        let region = Entry {
            name: String::new(),
            offset: entry.offset.checked_add(probe.offset).context("Probe offset overflow")?,
            size: probe.bytes.len() as u64,
        };
        archive::reader(file, &region)?.read_exact(&mut probe.bytes)?;
    }
    check_probes(probes, decoded, keys, titles)
}

struct ProbeWriter<'a> {
    position: u64,
    limit: u64,
    hash: Sha256,
    probes: Vec<Probe>,
    progress: &'a mut dyn FnMut(u64) -> std::io::Result<()>,
}
impl Write for ProbeWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.progress)(bytes.len() as u64)?;
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("Reconstructed NCA size overflow"))?;
        if end > self.limit {
            return Err(std::io::Error::other("Reconstructed NCA exceeds declared size"));
        }
        for probe in &mut self.probes {
            let from = self.position.max(probe.offset);
            let to = end.min(probe.offset + probe.bytes.len() as u64);
            if from < to {
                // Both intersections are bounded by their in-memory slices.
                let destination =
                    usize::try_from(from - probe.offset).map_err(std::io::Error::other)?;
                let source =
                    usize::try_from(from - self.position).map_err(std::io::Error::other)?;
                let length = usize::try_from(to - from).map_err(std::io::Error::other)?;
                probe.bytes[destination..destination + length]
                    .copy_from_slice(&bytes[source..source + length]);
            }
        }
        self.hash.update(bytes);
        self.position = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CompressedChecks {
    hash: [u8; 32],
    probes: anyhow::Result<()>,
}

fn compressed_checks(
    file: &File,
    entry: &Entry,
    decoded: &[u8],
    keys: &Keyset,
    titles: &TitleKeys,
    progress: &mut dyn FnMut(u64) -> std::io::Result<()>,
) -> anyhow::Result<CompressedChecks> {
    let expected_size = archive::u64_at(decoded, 0x208)?;
    let (probes, layout_error) = match probe_layout(decoded, expected_size) {
        Ok(probes) => (probes, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let mut out =
        ProbeWriter { position: 0, limit: expected_size, hash: Sha256::new(), probes, progress };
    let size = ncz::decompress(file, entry, &mut out)?;
    ensure!(size == expected_size, "Reconstructed NCA size mismatch");
    let probes = layout_error.map_or_else(|| check_probes(out.probes, decoded, keys, titles), Err);
    Ok(CompressedChecks { hash: out.hash.finalize().into(), probes })
}

fn declared_modification(
    header_valid: bool,
    identity: &str,
    size: u64,
    declared: Option<&([u8; 32], u64, bool)>,
) -> bool {
    !header_valid
        && identity.len() == 32
        && declared.is_some_and(|(hash, expected_size, trusted)| {
            *trusted && *expected_size == size && hex::encode(hash).starts_with(identity)
        })
}

fn header_signature_valid(decoded: &[u8]) -> anyhow::Result<bool> {
    let modulus =
        MODULI.get(usize::from(decoded[0x221])).context("Unknown NCA signature key generation")?;
    let public =
        RsaPublicKey::new(BigUint::from_bytes_be(&hex::decode(modulus)?), BigUint::from(65537u32))?;
    Ok(public
        .verify(
            Pss::new_with_salt::<Sha256>(32),
            &Sha256::digest(&decoded[0x200..0x400]),
            &decoded[..0x100],
        )
        .is_ok())
}

pub fn verify(path: &std::path::Path, keys: &Keyset, depth: &str) -> anyhow::Result<Value> {
    verify_with_progress(path, keys, depth, &mut |_| Ok(()))
}

#[allow(clippy::too_many_lines)] // Keep per-container signature, manifest and content verdict accumulation together.
pub fn verify_with_progress(
    path: &std::path::Path,
    keys: &Keyset,
    depth: &str,
    progress: &mut dyn FnMut(u64) -> std::io::Result<()>,
) -> anyhow::Result<Value> {
    let (mut file, root) = archive::root(path)?;
    let entries = archive::leaves(&mut file, &root)?;
    let title_keys = title_keys(&file, &entries)?;
    let ncas = entries
        .iter()
        .filter(|e| e.name.ends_with(".nca") || e.name.ends_with(".ncz"))
        .collect::<Vec<_>>();
    ensure!(!ncas.is_empty(), "Container contains no NCA content");
    let total = ncas.iter().fold(0u64, |size, entry| size.saturating_add(entry.size)).max(1);
    let mut done = 0u64;
    progress(0)?;
    let mut declared = BTreeMap::new();
    for entry in ncas.iter().filter(|e| e.name.ends_with(".cnmt.nca")) {
        ensure!(entry.size <= 64 * 1024 * 1024, "Oversized metadata NCA");
        let mut bytes = Vec::new();
        archive::reader(&file, entry)?.read_to_end(&mut bytes)?;
        let decoded = decrypt_with_header_key(
            bytes.get(..0xC00).context("Truncated metadata header")?,
            keys,
            0x200,
            0,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let trusted = header_signature_valid(&decoded)?;
        let mut nca = Nca::from_reader(std::io::Cursor::new(bytes), keys, Some(&title_keys))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        for i in 0..nca.filesystem_count() {
            let mut pfs =
                nca.open_pfs0_filesystem(i).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            for member in pfs.files.clone().iter().filter(|f| f.name.ends_with(".cnmt")) {
                ensure!(member.size <= 16 * 1024 * 1024, "Oversized CNMT");
                let bytes = pfs.read_to_vec(member).map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let cnmt = Cnmt::from_reader(&mut std::io::Cursor::new(bytes))
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                for content in cnmt.content_entries {
                    declared.insert(
                        hex::encode(content.info.content_id),
                        (content.hash, content.info.size, trusted),
                    );
                }
            }
        }
    }
    let mut signature = true;
    let mut hash_valid = true;
    let mut modified = false;
    let mut corrupt = false;
    let mut errors = Vec::new();
    for entry in &ncas {
        progress(done.saturating_mul(100).checked_div(total).unwrap_or(99).min(99))?;
        let mut header = vec![0; 0xC00];
        archive::reader(&file, entry)?.read_exact(&mut header)?;
        let decoded = decrypt_with_header_key(&header, keys, 0x200, 0)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        ensure!(decoded.get(0x200..0x203) == Some(b"NCA"), "Cannot decrypt NCA header");
        let header_valid = header_signature_valid(&decoded)?;
        if !header_valid {
            signature = false;
            errors.push(format!("{}: invalid header signature", entry.name));
        }
        let rights = &decoded[0x230..0x240];
        if rights.iter().any(|byte| *byte != 0)
            && title_keys.get_title_key(&hex::encode(rights)).is_none()
        {
            signature = false;
            errors.push(format!("{}: Missing title ticket", entry.name));
        }
        let expected_size = archive::u64_at(&decoded, 0x208)?;
        let mut report = |bytes| {
            done = done.saturating_add(bytes);
            progress(done.saturating_mul(100).checked_div(total).unwrap_or(99).min(99))
        };
        let (compressed_hash, probe_result) = if entry.name.ends_with(".ncz") {
            let checks = compressed_checks(&file, entry, &decoded, keys, &title_keys, &mut report)?;
            (Some(checks.hash), checks.probes)
        } else {
            (None, decryption_probes(&file, entry, &decoded, keys, &title_keys))
        };
        if let Err(error) = probe_result {
            signature = false;
            errors.push(format!("{}: {error}", entry.name));
        }
        if !entry.name.ends_with(".ncz") && entry.size != expected_size {
            signature = false;
            corrupt = true;
            errors.push(format!("{}: NCA size mismatch", entry.name));
        }
        if depth == "hash" {
            if !filesystem_headers_valid(&decoded)? {
                corrupt = true;
                errors.push(format!("{}: invalid filesystem header", entry.name));
            }
            let actual = match compressed_hash {
                Some(hash) => hash,
                None => member_hash_progress(&file, entry, &mut report)?,
            };
            let digest = hex::encode(actual);
            let identity = entry.name.split('.').next().unwrap_or_default().to_ascii_lowercase();
            if identity.len() != 32 || !digest.starts_with(&identity) {
                hash_valid = false;
                if declared_modification(
                    header_valid,
                    &identity,
                    expected_size,
                    declared.get(&identity),
                ) {
                    modified = true;
                } else {
                    corrupt = true;
                }
                errors.push(format!("{}: content hash mismatch", entry.name));
            } else if let Some((expected, size, _)) = declared.get(&identity) {
                if *expected != actual || *size != expected_size {
                    hash_valid = false;
                    corrupt = true;
                    errors.push(format!("{}: CNMT hash/size mismatch", entry.name));
                }
            }
        }
    }
    for identity in declared.keys() {
        if !ncas.iter().any(|entry| entry.name.starts_with(identity)) {
            signature = false;
            corrupt = true;
            errors.push(format!("Missing CNMT content {identity}"));
        }
    }
    let hash = (depth == "hash").then_some(hash_valid && !corrupt);
    let modified = (depth == "hash").then_some(modified && !corrupt);
    Ok(json!({"signatureValid":signature,"hashValid":hash,"hashModified":modified,
        "verificationStatus":status(Some(signature),hash,modified),"verificationError":if errors.is_empty(){None}else{Some(errors.join("; "))}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_hash_reports_bytes_and_honors_cancellation() -> anyhow::Result<()> {
        let mut file = tempfile::tempfile()?;
        let bytes = vec![0x73; 256 * 1024];
        file.write_all(&bytes)?;
        let entry = Entry { name: "test.nca".into(), offset: 0, size: bytes.len() as u64 };
        let mut reported = 0;
        let digest = member_hash_progress(&file, &entry, &mut |count| {
            reported += count;
            Ok(())
        })?;
        assert_eq!(reported, entry.size);
        assert_eq!(digest.as_slice(), Sha256::digest(&bytes).as_slice());
        let error = member_hash_progress(&file, &entry, &mut |_| {
            Err(std::io::Error::other("Task cancelled"))
        })
        .err()
        .context("cancelled hash must stop reading")?;
        assert_eq!(error.to_string(), "Task cancelled");
        file.set_len(entry.size - 1)?;
        assert!(member_hash(&file, &entry).is_err());
        Ok(())
    }

    #[test]
    fn two_layer_pfs0_probe_agrees_with_nx_archive_parser() -> anyhow::Result<()> {
        let keys = Keyset { header_key_cache: Some([0x17; 32]), ..Default::default() };
        let mut bytes = vec![0; 0x4400];
        bytes[0x200..0x204].copy_from_slice(b"NCA3");
        bytes[0x208..0x210].copy_from_slice(&0x4400u64.to_le_bytes());
        bytes[0x240..0x244].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[0x244..0x248].copy_from_slice(&0x22u32.to_le_bytes());
        bytes[0x402] = 1;
        bytes[0x403] = 2;
        bytes[0x404] = 1;
        bytes[0x42c..0x430].copy_from_slice(&2u32.to_le_bytes());
        bytes[0x438..0x440].copy_from_slice(&32u64.to_le_bytes());
        bytes[0x440..0x448].copy_from_slice(&0x200u64.to_le_bytes());
        bytes[0x448..0x450].copy_from_slice(&0x200u64.to_le_bytes());
        bytes[0x450..0x458].copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[0x4200..0x4204].copy_from_slice(b"PFS0");
        let decoded = bytes[..0xC00].to_vec();
        bytes[..0xC00].copy_from_slice(&nx_archive::formats::nca::encrypt_with_header_key(
            &decoded, &keys, 0x200, 0,
        ));
        let titles = TitleKeys::new();
        let mut reference =
            Nca::from_reader(std::io::Cursor::new(bytes.clone()), &keys, Some(&titles))
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let pfs = reference
            .open_pfs0_filesystem(0)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        assert!(pfs.files.is_empty());
        let mut file = tempfile::tempfile()?;
        file.write_all(&bytes)?;
        let entry = Entry { name: "test.nca".into(), offset: 0, size: 0x4400 };
        decryption_probes(&file, &entry, &decoded, &keys, &titles)?;
        let mut wrong_region = decoded;
        wrong_region[0x440..0x448].copy_from_slice(&0u64.to_le_bytes());
        assert!(decryption_probes(&file, &entry, &wrong_region, &keys, &titles).is_err());
        Ok(())
    }

    #[test]
    fn modified_requires_matching_trusted_manifest_and_size() {
        let hash = [0x42; 32];
        let identity = hex::encode(&hash[..16]);
        assert!(declared_modification(false, &identity, 4096, Some(&(hash, 4096, true))));
        assert!(!declared_modification(true, &identity, 4096, Some(&(hash, 4096, true))));
        assert!(!declared_modification(false, &identity, 4096, Some(&(hash, 2048, true))));
        assert!(!declared_modification(false, &identity, 4096, Some(&(hash, 4096, false))));
        assert!(!declared_modification(false, &identity, 4096, Some(&([0x43; 32], 4096, true))));
        assert!(!declared_modification(false, &identity, 4096, None));
        assert!(!declared_modification(false, "", 4096, Some(&(hash, 4096, true))));
    }

    #[test]
    fn probes_plain_ctr_and_ivfc_with_bounds_and_wrong_keys() -> anyhow::Result<()> {
        use aes::cipher::{BlockEncrypt, KeyInit};
        let mut keys = Keyset { header_key_cache: Some([0x17; 32]), ..Default::default() };
        keys.raw_keys.insert("key_area_key_application_00".into(), vec![0x11; 16]);
        let mut decoded = vec![0; 0xC00];
        decoded[0x200..0x204].copy_from_slice(b"NCA3");
        decoded[0x208..0x210].copy_from_slice(&0x4400u64.to_le_bytes());
        decoded[0x240..0x244].copy_from_slice(&0x20u32.to_le_bytes());
        decoded[0x244..0x248].copy_from_slice(&0x22u32.to_le_bytes());
        let mut encrypted_key: aes::Block = [0x22; 16].into();
        aes::Aes128::new((&[0x11; 16]).into()).encrypt_block(&mut encrypted_key);
        decoded[0x320..0x330].copy_from_slice(&encrypted_key);
        let entry = Entry { name: "probe.nca".into(), offset: 37, size: 0x4400 };
        let titles = TitleKeys::new();
        for ivfc in [false, true] {
            for ctr in [false, true] {
                let fs = &mut decoded[0x400..0x600];
                fs.fill(0);
                fs[2] = u8::from(!ivfc);
                fs[3] = if ivfc { 3 } else { 2 };
                fs[4] = if ctr { 3 } else { 1 };
                fs[0x140..0x148].copy_from_slice(&7u64.to_le_bytes());
                let mut plain = vec![0; 32];
                if ivfc {
                    plain.fill(0x5a);
                    fs[8..12].copy_from_slice(b"IVFC");
                    fs[0x10..0x14].copy_from_slice(&32u32.to_le_bytes());
                    fs[0x18..0x20].copy_from_slice(&0x200u64.to_le_bytes());
                    fs[0x20..0x28].copy_from_slice(&32u64.to_le_bytes());
                    fs[0xc8..0xe8].copy_from_slice(&Sha256::digest(&plain));
                } else {
                    plain[..4].copy_from_slice(b"PFS0");
                    fs[0x2c..0x30].copy_from_slice(&2u32.to_le_bytes());
                    fs[0x40..0x48].copy_from_slice(&0x200u64.to_le_bytes());
                    fs[0x48..0x50].copy_from_slice(&0x200u64.to_le_bytes());
                }
                if ctr {
                    let mut counter = [0; 16];
                    counter[..8].copy_from_slice(&7u64.to_be_bytes());
                    ncz::crypt_ctr(&mut plain, 0x4200, &[0x22; 16], counter);
                }
                let mut file = tempfile::tempfile()?;
                let mut bytes = vec![0; usize::try_from(entry.offset + entry.size)?];
                let start = usize::try_from(entry.offset + 0x4200)?;
                bytes[start..start + plain.len()].copy_from_slice(&plain);
                let header_start = usize::try_from(entry.offset)?;
                bytes[header_start..header_start + decoded.len()].copy_from_slice(
                    &nx_archive::formats::nca::encrypt_with_header_key(&decoded, &keys, 0x200, 0),
                );
                file.write_all(&bytes)?;
                decryption_probes(&file, &entry, &decoded, &keys, &titles)?;
                assert_compressed_probes(&file, &entry, &decoded, &keys, ctr)?;
                if ctr {
                    assert!(
                        decryption_probes(&file, &entry, &decoded, &Keyset::default(), &titles)
                            .is_err()
                    );
                    let mut wrong = keys.clone();
                    wrong.raw_keys.insert("key_area_key_application_00".into(), vec![0x33; 16]);
                    assert!(decryption_probes(&file, &entry, &decoded, &wrong, &titles).is_err());
                }
                let mut bad = decoded.clone();
                let region = if ivfc { 0x418 } else { 0x440 };
                bad[region..region + 8].copy_from_slice(&u64::MAX.to_le_bytes());
                assert!(decryption_probes(&file, &entry, &bad, &keys, &titles).is_err());
                bad = decoded.clone();
                bad[0x244..0x248].copy_from_slice(&0x23u32.to_le_bytes());
                assert!(decryption_probes(&file, &entry, &bad, &keys, &titles).is_err());
            }
        }
        Ok(())
    }

    fn assert_compressed_probes(
        file: &File,
        entry: &Entry,
        decoded: &[u8],
        keys: &Keyset,
        encrypted: bool,
    ) -> anyhow::Result<()> {
        use crate::settings::CompressionSettings;
        let titles = TitleKeys::new();
        for block in [false, true] {
            let mut compressed = tempfile::tempfile()?;
            ncz::compress(
                file,
                entry,
                &mut compressed,
                keys,
                &titles,
                &CompressionSettings { level: 1, block_size_exponent: 14, ..Default::default() },
                block,
            )?;
            let ncz_entry =
                Entry { name: "probe.ncz".into(), offset: 0, size: compressed.metadata()?.len() };
            let mut reported = 0;
            let checks =
                compressed_checks(&compressed, &ncz_entry, decoded, keys, &titles, &mut |count| {
                    reported += count;
                    Ok(())
                })?;
            checks.probes?;
            assert_eq!(reported, entry.size);
            assert_eq!(checks.hash, member_hash(file, entry)?);
            let mut wrong = keys.clone();
            wrong.raw_keys.insert("key_area_key_application_00".into(), vec![0x33; 16]);
            let checks =
                compressed_checks(&compressed, &ncz_entry, decoded, &wrong, &titles, &mut |_| {
                    Ok(())
                })?;
            assert_eq!(checks.probes.is_err(), encrypted);
            assert!(
                compressed_checks(&compressed, &ncz_entry, decoded, keys, &titles, &mut |_| Err(
                    std::io::Error::other("Task cancelled")
                ))
                .is_err()
            );
            let mut wrong_size = decoded.to_vec();
            wrong_size[0x208..0x210].copy_from_slice(&(entry.size + 1).to_le_bytes());
            assert!(
                compressed_checks(
                    &compressed,
                    &ncz_entry,
                    &wrong_size,
                    keys,
                    &titles,
                    &mut |_| Ok(())
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn probe_capture_handles_split_writes_with_bounded_buffers() -> anyhow::Result<()> {
        let mut progress = |_| Ok(());
        let mut sink = ProbeWriter {
            position: 0,
            limit: 100,
            hash: Sha256::new(),
            progress: &mut progress,
            probes: vec![Probe {
                offset: 11,
                section_end: 100,
                expected: None,
                counter: None,
                bytes: vec![0; 16],
            }],
        };
        let bytes = (0..100u8).collect::<Vec<_>>();
        for chunk in bytes.chunks(7) {
            sink.write_all(chunk)?;
        }
        assert_eq!(sink.probes[0].bytes, bytes[11..27]);
        assert_eq!(sink.probes[0].bytes.capacity(), 16);
        assert!(sink.write_all(&[0]).is_err());
        assert_eq!(sink.position, 100);
        assert_eq!(sink.hash.finalize(), Sha256::digest(&bytes));
        Ok(())
    }

    #[test]
    fn verification_reports_missing_rights_ticket() -> anyhow::Result<()> {
        let keys = Keyset { header_key_cache: Some([0x17; 32]), ..Default::default() };
        let mut nca = vec![0; 0xC00];
        nca[0x200..0x204].copy_from_slice(b"NCA3");
        nca[0x208..0x210].copy_from_slice(&0xC00u64.to_le_bytes());
        nca[0x230..0x240].fill(0x42);
        let encrypted = nx_archive::formats::nca::encrypt_with_header_key(&nca, &keys, 0x200, 0);
        let name = format!("{}.nca\0", hex::encode(&Sha256::digest(&encrypted)[..16]));
        let mut container = tempfile::NamedTempFile::new()?;
        container.write_all(b"PFS0")?;
        container.write_all(&1u32.to_le_bytes())?;
        container.write_all(&u32::try_from(name.len())?.to_le_bytes())?;
        container.write_all(&[0; 4])?;
        container.write_all(&0u64.to_le_bytes())?;
        container.write_all(&0xC00u64.to_le_bytes())?;
        container.write_all(&[0; 8])?;
        container.write_all(name.as_bytes())?;
        container.write_all(&encrypted)?;
        for depth in ["signature", "hash"] {
            let result = verify(container.path(), &keys, depth)?;
            assert_eq!(result["signatureValid"], false);
            assert!(
                result["verificationError"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Missing title ticket")
            );
            if depth == "hash" {
                assert_eq!(result["hashValid"], true);
                assert_eq!(result["verificationStatus"], "REPACK");
            } else {
                assert!(result["hashValid"].is_null());
            }
        }
        Ok(())
    }

    #[test]
    fn ticket_offsets_follow_signature_type() -> anyhow::Result<()> {
        for (kind, expected) in [(0x0001_0003u32, 0x240), (0x0001_0004, 0x140), (0x0001_0005, 0x80)]
        {
            assert_eq!(ticket_body(&kind.to_le_bytes())?, expected);
        }
        assert!(ticket_body(&[0; 4]).is_err());
        assert!(ticket_body(&[0; 3]).is_err());
        Ok(())
    }
    #[test]
    fn verdicts_keep_signature_and_content_results_separate() {
        for (signature, hash, modified, expected) in [
            (None, None, None, "UNVERIFIED"),
            (Some(true), None, None, "SIGNATURE_OK"),
            (Some(false), None, None, "SIGNATURE_FAILED"),
            (Some(true), Some(true), Some(false), "VALID"),
            (Some(false), Some(true), Some(false), "REPACK"),
            (Some(false), Some(false), Some(true), "MODIFIED"),
            (Some(true), Some(false), Some(false), "CORRUPT"),
        ] {
            assert_eq!(status(signature, hash, modified), expected);
        }
    }
}
