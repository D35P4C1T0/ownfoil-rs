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
    if entry.name.ends_with(".ncz") {
        let mut sink = HashWriter::default();
        ncz::decompress(file, entry, &mut sink)?;
        Ok(sink.hash.finalize().into())
    } else {
        archive::hash(file, entry)
    }
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

#[allow(clippy::too_many_lines)] // Keep per-container signature, manifest and content verdict accumulation together.
pub fn verify(path: &std::path::Path, keys: &Keyset, depth: &str) -> anyhow::Result<Value> {
    let (mut file, root) = archive::root(path)?;
    let entries = archive::leaves(&mut file, &root)?;
    let title_keys = title_keys(&file, &entries)?;
    let ncas = entries
        .iter()
        .filter(|e| e.name.ends_with(".nca") || e.name.ends_with(".ncz"))
        .collect::<Vec<_>>();
    ensure!(!ncas.is_empty(), "Container contains no NCA content");
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
        let expected_size = archive::u64_at(&decoded, 0x208)?;
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
            let actual = member_hash(&file, entry)?;
            let digest = hex::encode(actual);
            let identity = entry.name.split('.').next().unwrap_or_default().to_ascii_lowercase();
            if identity.len() != 32 || !digest.starts_with(&identity) {
                hash_valid = false;
                if !header_valid
                    && declared.get(&identity).is_some_and(|(hash, _, trusted)| {
                        *trusted && hex::encode(hash).starts_with(&identity)
                    })
                {
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
