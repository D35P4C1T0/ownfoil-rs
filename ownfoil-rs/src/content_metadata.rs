//! Bounded Control NCA metadata extraction. Never materializes program NCAs.
use super::{CloneReader, identity_from_cnmt};
use crate::catalog::IdentifiedContent;
use nx_archive::formats::{
    Keyset,
    cnmt::PackagedContentType,
    nca::{Nca, decrypt_with_header_key},
    pfs0::Pfs0,
    xci::Xci,
};
use nx_archive::{FileEntryExt, TitleDataExt};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_CONTROL: u64 = 32 * 1024 * 1024;
const MAX_ICON: usize = 4 * 1024 * 1024;
const LANGUAGES: [&str; 16] = [
    "AmericanEnglish",
    "BritishEnglish",
    "Japanese",
    "French",
    "German",
    "LatinAmericanSpanish",
    "Spanish",
    "Italian",
    "Dutch",
    "CanadianFrench",
    "Portuguese",
    "Russian",
    "Korean",
    "TraditionalChinese",
    "SimplifiedChinese",
    "BrazilianPortuguese",
];

#[derive(Debug, Default)]
pub struct ContainerMetadata {
    pub contents: Vec<IdentifiedContent>,
    pub metadata: Vec<ExtractedMetadata>,
}
#[derive(Debug)]
pub struct ExtractedMetadata {
    pub title_id: String,
    pub app_id: String,
    pub version: u32,
    pub record: serde_json::Map<String, serde_json::Value>,
    pub display_version: Option<String>,
    pub icon: Option<Vec<u8>>,
    pub language: Option<String>,
    pub icon_language: Option<String>,
}

pub async fn read_container_metadata(
    path: &Path,
    keys_path: &Path,
    region: &str,
    language: &str,
) -> Result<ContainerMetadata, String> {
    let path = path.to_path_buf();
    let keys = keys_path.to_path_buf();
    let wanted = language_for_locale(region, language);
    tokio::task::spawn_blocking(move || read_container(&path, &keys, wanted))
        .await
        .map_err(|e| e.to_string())?
}

#[allow(clippy::too_many_lines)] // Container branches share the same extraction and limits.
fn read_container(path: &Path, keys: &Path, wanted: usize) -> Result<ContainerMetadata, String> {
    let keyset = Keyset::from_file(keys).map_err(|e| e.to_string())?;
    let reader = CloneReader::open(path).map_err(|e| e.to_string())?;
    let title_keys = crate::content::metadata_title_keys(path).map_err(|e| e.to_string())?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let mut result = ContainerMetadata::default();
    match ext.as_str() {
        "nsp" | "nsz" => {
            let mut container = Pfs0::from_reader(reader).map_err(|e| e.to_string())?;
            let cnmts =
                container.get_cnmts(&keyset, Some(&title_keys)).map_err(|e| e.to_string())?;
            for cnmt in cnmts {
                let Some(identity) = identity_from_cnmt(&cnmt) else { continue };
                for control in cnmt
                    .content_entries
                    .iter()
                    .filter(|c| c.info.content_type == PackagedContentType::Control)
                {
                    let name = format!("{}.nca", hex::encode(control.info.content_id));
                    if let Some(entry) = container.get_file(&name) {
                        if entry.size > MAX_CONTROL {
                            return Err("Control NCA exceeds extraction limit".into());
                        }
                        let bytes = entry
                            .read_bytes(
                                &mut container,
                                usize::try_from(entry.size).map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                        result.metadata.push(read_control(
                            &bytes,
                            &keyset,
                            Some(&title_keys),
                            &identity,
                            wanted,
                        )?);
                    } else if container.get_file(&name.replace(".nca", ".ncz")).is_some() {
                        let bytes = crate::content::metadata_member(
                            path,
                            &name,
                            usize::try_from(MAX_CONTROL).map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                        result.metadata.push(read_control(
                            &bytes,
                            &keyset,
                            Some(&title_keys),
                            &identity,
                            wanted,
                        )?);
                    }
                }
                result.contents.push(identity);
            }
        }
        "xci" | "xcz" => {
            let mut container = Xci::new(reader).map_err(|e| e.to_string())?;
            let cnmts =
                container.get_cnmts(&keyset, Some(&title_keys)).map_err(|e| e.to_string())?;
            let Some(mut secure) = container.open_secure_partition().map_err(|e| e.to_string())?
            else {
                return Ok(result);
            };
            for cnmt in cnmts {
                let Some(identity) = identity_from_cnmt(&cnmt) else { continue };
                for control in cnmt
                    .content_entries
                    .iter()
                    .filter(|c| c.info.content_type == PackagedContentType::Control)
                {
                    let name = format!("{}.nca", hex::encode(control.info.content_id));
                    if let Some(entry) = secure.get_file(&name).map_err(|e| e.to_string())? {
                        if entry.size > MAX_CONTROL {
                            return Err("Control NCA exceeds extraction limit".into());
                        }
                        let bytes = secure.read_to_vec(&entry).map_err(|e| e.to_string())?;
                        result.metadata.push(read_control(
                            &bytes,
                            &keyset,
                            Some(&title_keys),
                            &identity,
                            wanted,
                        )?);
                    } else if secure
                        .get_file(&name.replace(".nca", ".ncz"))
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        let bytes = crate::content::metadata_member(
                            path,
                            &name,
                            usize::try_from(MAX_CONTROL).map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                        result.metadata.push(read_control(
                            &bytes,
                            &keyset,
                            Some(&title_keys),
                            &identity,
                            wanted,
                        )?);
                    }
                }
                result.contents.push(identity);
            }
        }
        _ => return Err("Unsupported container extension".into()),
    }
    Ok(result)
}

fn read_control(
    bytes: &[u8],
    keys: &Keyset,
    title_keys: Option<&nx_archive::formats::TitleKeys>,
    identity: &IdentifiedContent,
    wanted: usize,
) -> Result<ExtractedMetadata, String> {
    let nca = Nca::from_reader(Cursor::new(bytes), keys, title_keys).map_err(|e| e.to_string())?;
    let decrypted =
        decrypt_with_header_key(bytes.get(..0xc00).ok_or("Truncated NCA header")?, keys, 0x200, 0)
            .map_err(|e| e.to_string())?;
    let sections = nca
        .header
        .fs_entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.start_offset != 0 || entry.end_offset != 0)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    for (index, header) in nca.fs_headers.iter().enumerate() {
        let raw_index = sections[index];
        let fs = decrypted
            .get(0x400 + raw_index * 0x200..0x600 + raw_index * 0x200)
            .ok_or("Missing filesystem header")?;
        if fs[2] != 0 {
            continue;
        } // RomFS
        let (offset, size) = match fs[3] {
            3 => {
                if &fs[8..12] != b"IVFC" {
                    return Err("Invalid integrity header".into());
                }
                let count = u32::from_le_bytes(
                    fs[0x14..0x18].try_into().map_err(|_| "Invalid integrity count")?,
                ) as usize;
                // MaxLayers includes the master hash; six descriptors follow it.
                if !(2..=7).contains(&count) {
                    return Err("Invalid integrity count".into());
                }
                let at = 0x18 + (count - 2) * 0x18;
                (u64_at(fs, at)?, u64_at(fs, at + 8)?)
            }
            2 => (u64_at(fs, 0x40)?, u64_at(fs, 0x48)?),
            _ => return Err("Unsupported Control RomFS hash type".into()),
        };
        if size > MAX_CONTROL {
            return Err("Control RomFS exceeds extraction limit".into());
        }
        let offset = nca
            .get_fs_offset(index)
            .ok_or("Missing filesystem offset")?
            .checked_add(offset)
            .ok_or("Invalid offset")?;
        if offset.checked_add(size).is_none_or(|end| end > bytes.len() as u64) {
            return Err("Control RomFS is truncated".into());
        }
        let mut blob = vec![0; usize::try_from(size).map_err(|e| e.to_string())?];
        match fs[4] {
            1 => {
                let mut reader = Cursor::new(bytes);
                reader.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                reader.read_exact(&mut blob).map_err(|e| e.to_string())?;
            }
            3 => {
                let key = nca.get_aes_ctr_decrypt_key().map_err(|e| e.to_string())?;
                let mut reader = nx_archive::io::Aes128CtrReader::new(
                    Cursor::new(bytes),
                    offset,
                    header.ctr,
                    key.to_vec(),
                );
                reader.read_exact(&mut blob).map_err(|e| e.to_string())?;
            }
            _ => return Err("Unsupported Control NCA encryption".into()),
        }
        return parse_romfs(&blob, identity, wanted);
    }
    Err("Control NCA has no RomFS".into())
}

fn u64_at(blob: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        blob.get(offset..offset + 8)
            .ok_or("Truncated RomFS")?
            .try_into()
            .map_err(|_| "Invalid integer")?,
    ))
}
fn parse_romfs(
    blob: &[u8],
    identity: &IdentifiedContent,
    wanted: usize,
) -> Result<ExtractedMetadata, String> {
    if u64_at(blob, 0)? != 0x50 {
        return Err("Invalid RomFS header".into());
    }
    let meta = usize::try_from(u64_at(blob, 0x38)?).map_err(|e| e.to_string())?;
    let size = usize::try_from(u64_at(blob, 0x40)?).map_err(|e| e.to_string())?;
    let data = usize::try_from(u64_at(blob, 0x48)?).map_err(|e| e.to_string())?;
    let table = blob
        .get(meta..meta.checked_add(size).ok_or("RomFS overflow")?)
        .ok_or("Invalid RomFS table")?;
    let mut files = std::collections::BTreeMap::new();
    let mut offset = 0;
    while offset < table.len() {
        let entry = table.get(offset..offset + 32).ok_or("Truncated RomFS entry")?;
        let name_size =
            u32::from_le_bytes(entry[28..32].try_into().map_err(|_| "Invalid name size")?) as usize;
        if name_size == 0 && table[offset..].iter().all(|byte| *byte == 0) {
            break;
        }
        let parent = u32::from_le_bytes(entry[..4].try_into().map_err(|_| "Invalid parent")?);
        let next = offset.checked_add(32 + ((name_size + 3) & !3)).ok_or("RomFS overflow")?;
        // Control metadata lives at the root; nested resources may reuse filenames.
        if parent != 0 || name_size == 0 {
            offset = next;
            continue;
        }
        let name = String::from_utf8_lossy(
            table.get(offset + 32..offset + 32 + name_size).ok_or("Invalid RomFS filename")?,
        )
        .into_owned();
        let start = data
            .checked_add(usize::try_from(u64_at(entry, 8)?).map_err(|e| e.to_string())?)
            .ok_or("RomFS overflow")?;
        let length = usize::try_from(u64_at(entry, 16)?).map_err(|e| e.to_string())?;
        let value = blob
            .get(start..start.checked_add(length).ok_or("RomFS overflow")?)
            .ok_or("Invalid RomFS file range")?;
        if files.insert(name, value).is_some() {
            return Err("Duplicate RomFS name".into());
        }
        offset = next;
    }
    let nacp = *files.get("control.nacp").ok_or("Missing control.nacp")?;
    parse_nacp(nacp, &files, identity, wanted)
}
fn string_at(blob: &[u8], offset: usize, size: usize) -> Option<String> {
    let bytes = blob.get(offset..offset + size)?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).into_owned();
    (!s.is_empty()).then_some(s)
}
fn pick(available: &[usize], wanted: usize) -> Option<usize> {
    [wanted, 0].into_iter().find(|i| available.contains(i)).or_else(|| available.first().copied())
}
fn parse_nacp(
    nacp: &[u8],
    files: &std::collections::BTreeMap<String, &[u8]>,
    identity: &IdentifiedContent,
    wanted: usize,
) -> Result<ExtractedMetadata, String> {
    if nacp.len() < 0x3070 {
        return Err("Truncated NACP".into());
    }
    let names = (0..16).filter(|i| string_at(nacp, i * 0x300, 0x200).is_some()).collect::<Vec<_>>();
    let icons = (0..16)
        .filter(|i| files.contains_key(&format!("icon_{}.dat", LANGUAGES[*i])))
        .collect::<Vec<_>>();
    let name_language = pick(&names, wanted);
    let icon_language = pick(&icons, wanted);
    let mut record = serde_json::Map::new();
    if let Some(i) = name_language {
        for (key, value) in [
            ("name", string_at(nacp, i * 0x300, 0x200)),
            ("publisher", string_at(nacp, i * 0x300 + 0x200, 0x100)),
        ] {
            if let Some(value) = value {
                record.insert(key.into(), value.into());
            }
        }
    }
    let icon = icon_language
        .map(|i| files[&format!("icon_{}.dat", LANGUAGES[i])])
        .map(|b| {
            if b.len() > MAX_ICON {
                Err(String::from("Icon exceeds extraction limit"))
            } else {
                Ok(b.to_vec())
            }
        })
        .transpose()?;
    Ok(ExtractedMetadata {
        title_id: identity.title_id.clone(),
        app_id: identity.app_id.clone(),
        version: identity.version,
        record,
        display_version: string_at(nacp, 0x3060, 0x10),
        icon,
        language: name_language.map(|i| LANGUAGES[i].into()),
        icon_language: icon_language.map(|i| LANGUAGES[i].into()),
    })
}
fn language_for_locale(region: &str, language: &str) -> usize {
    let region = region.to_ascii_uppercase();
    let americas = ["US", "CA", "MX", "AR", "BR", "CL", "CO", "PE"].contains(&region.as_str());
    match (region.as_str(), language.to_ascii_lowercase().as_str()) {
        ("CA", "fr") => 9,
        ("BR", "pt") => 15,
        ("HK", "zh") => 13,
        (_, "en") => usize::from(!americas),
        (_, "es") => {
            if americas {
                5
            } else {
                6
            }
        }
        (_, "ja") => 2,
        (_, "fr") => 3,
        (_, "de") => 4,
        (_, "it") => 7,
        (_, "nl") => 8,
        (_, "pt") => 10,
        (_, "ru") => 11,
        (_, "ko") => 12,
        (_, "zh") => 14,
        _ => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn identity() -> IdentifiedContent {
        IdentifiedContent {
            title_id: "0100000000000000".into(),
            app_id: "0100000000000800".into(),
            version: 65536,
            kind: crate::catalog::ContentKind::Update,
        }
    }
    #[test]
    fn locale_and_independent_icon_fallback() {
        let mut bytes = vec![0; 0x4000];
        bytes[0..4].copy_from_slice(b"Game");
        bytes[7 * 0x300..7 * 0x300 + 5].copy_from_slice(b"Gioco");
        bytes[7 * 0x300 + 0x200..7 * 0x300 + 0x204].copy_from_slice(b"Pub!");
        bytes[0x3060..0x3065].copy_from_slice(b"1.2.3");
        let files = std::collections::BTreeMap::from([(
            "icon_AmericanEnglish.dat".into(),
            b"icon".as_slice(),
        )]);
        let result =
            parse_nacp(&bytes, &files, &identity(), language_for_locale("IT", "it")).unwrap();
        assert_eq!(result.record["name"], "Gioco");
        assert_eq!(result.record["publisher"], "Pub!");
        assert_eq!(result.display_version.as_deref(), Some("1.2.3"));
        assert_eq!(result.icon_language.as_deref(), Some("AmericanEnglish"));
        assert_eq!(language_for_locale("BR", "pt"), 15);
        assert_eq!(language_for_locale("GB", "en"), 1);
    }
    #[test]
    fn malformed_metadata_is_rejected() {
        assert!(parse_romfs(&[0; 80], &identity(), 0).is_err());
        assert!(
            parse_nacp(&[0; 20], &std::collections::BTreeMap::default(), &identity(), 0).is_err()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
mod container_tests {
    use super::*;
    use aes::cipher::{BlockEncrypt, KeyInit};

    fn fixture(ctr: bool, section: usize) -> (Vec<u8>, Keyset) {
        let mut keys = Keyset { header_key_cache: Some([0x17; 32]), ..Default::default() };
        keys.raw_keys.insert("key_area_key_application_00".into(), vec![0x11; 16]);
        let mut bytes = vec![0; 0xc000];
        bytes[0x200..0x204].copy_from_slice(b"NCA3");
        bytes[0x205] = 2; // Control
        bytes[0x208..0x210].copy_from_slice(&0xc000u64.to_le_bytes());
        bytes[0x240 + section * 16..0x244 + section * 16].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[0x244 + section * 16..0x248 + section * 16].copy_from_slice(&0x60u32.to_le_bytes());
        let fs = 0x400 + section * 0x200;
        bytes[fs] = 2;
        bytes[fs + 2] = 0;
        bytes[fs + 3] = 3;
        bytes[fs + 4] = if ctr { 3 } else { 1 };
        bytes[fs + 8..fs + 12].copy_from_slice(b"IVFC");
        bytes[fs + 0x14..fs + 0x18].copy_from_slice(&7u32.to_le_bytes());
        bytes[fs + 0x98..fs + 0xa0].copy_from_slice(&0x8000u64.to_le_bytes());
        bytes[fs + 0x140..fs + 0x148].copy_from_slice(&7u64.to_le_bytes());
        let aes = aes::Aes128::new((&[0x11; 16]).into());
        let mut wrapped = ([0x22; 16]).into();
        aes.encrypt_block(&mut wrapped);
        bytes[0x320..0x330].copy_from_slice(&wrapped);
        let rom = &mut bytes[0x4000..];
        rom[0..8].copy_from_slice(&0x50u64.to_le_bytes());
        rom[0x38..0x40].copy_from_slice(&0x50u64.to_le_bytes());
        rom[0x40..0x48].copy_from_slice(&44u64.to_le_bytes());
        rom[0x48..0x50].copy_from_slice(&0x100u64.to_le_bytes());
        rom[0x60..0x68].copy_from_slice(&0x4000u64.to_le_bytes());
        rom[0x6c..0x70].copy_from_slice(&12u32.to_le_bytes());
        rom[0x70..0x7c].copy_from_slice(b"control.nacp");
        rom[0x100..0x104].copy_from_slice(b"Game");
        rom[0x300..0x309].copy_from_slice(b"Publisher");
        rom[0x3160..0x3163].copy_from_slice(b"2.0");
        if ctr {
            // Independent CTR implementation verifies nx-archive reader's absolute offset.
            for (block_index, block) in bytes[0x4000..].chunks_mut(16).enumerate() {
                let mut counter = [0; 16];
                counter[..8].copy_from_slice(&7u64.to_be_bytes());
                counter[8..].copy_from_slice(&(0x400u64 + block_index as u64).to_be_bytes());
                let mut encrypted = counter.into();
                aes::Aes128::new((&[0x22; 16]).into()).encrypt_block(&mut encrypted);
                for (byte, key) in block.iter_mut().zip(encrypted.iter()) {
                    *byte ^= key;
                }
            }
        }
        let encrypted =
            nx_archive::formats::nca::encrypt_with_header_key(&bytes[..0xc00], &keys, 0x200, 0);
        bytes[..0xc00].copy_from_slice(&encrypted);
        (bytes, keys)
    }
    #[test]
    fn modern_key_generations_parse_without_changing_header_bytes() {
        for generation in [0x14u8, 0x15, 0x16] {
            let (bytes, keys) = fixture(false, 0);
            let mut header = decrypt_with_header_key(&bytes[..0xc00], &keys, 0x200, 0).unwrap();
            header[0x220] = generation;
            let encrypted =
                nx_archive::formats::nca::encrypt_with_header_key(&header, &keys, 0x200, 0);
            let mut bytes = bytes;
            bytes[..0xc00].copy_from_slice(&encrypted);
            let nca = Nca::from_reader(Cursor::new(&bytes), &keys, None).unwrap();
            assert_eq!(nca.header.key_generation as u8, generation);
        }
    }

    #[test]
    fn root_metadata_ignores_nested_duplicates_and_table_padding() {
        let (mut bytes, _) = fixture(false, 0);
        let rom = &mut bytes[0x4000..];
        rom[0x40..0x48].copy_from_slice(&120u64.to_le_bytes());
        let nested = 0x7c;
        rom[nested..nested + 4].copy_from_slice(&4u32.to_le_bytes());
        rom[nested + 28..nested + 32].copy_from_slice(&12u32.to_le_bytes());
        rom[nested + 32..nested + 44].copy_from_slice(b"control.nacp");
        let identity = IdentifiedContent {
            title_id: "0100000000000000".into(),
            app_id: "0100000000000000".into(),
            version: 0,
            kind: crate::catalog::ContentKind::Base,
        };
        assert_eq!(parse_romfs(rom, &identity, 0).unwrap().record["name"], "Game");
    }

    #[test]
    fn encrypted_control_nca_and_sparse_section_extract_metadata() {
        for ctr in [false, true] {
            for section in [0, 2] {
                let (bytes, keys) = fixture(ctr, section);
                let identity = IdentifiedContent {
                    title_id: "0100000000000000".into(),
                    app_id: "0100000000000800".into(),
                    version: 65536,
                    kind: crate::catalog::ContentKind::Update,
                };
                let metadata = read_control(&bytes, &keys, None, &identity, 0).unwrap();
                assert_eq!(metadata.record["name"], "Game");
                assert_eq!(metadata.record["publisher"], "Publisher");
                assert_eq!(metadata.display_version.as_deref(), Some("2.0"));
            }
        }
    }
    #[test]
    fn compressed_control_member_roundtrip_is_bounded() {
        use std::io::Write;
        let (bytes, keys) = fixture(false, 0);
        let mut ncz = bytes[..0x4000].to_vec();
        ncz.extend_from_slice(b"NCZSECTN");
        ncz.extend_from_slice(&1u64.to_le_bytes());
        ncz.extend_from_slice(&0x4000u64.to_le_bytes());
        ncz.extend_from_slice(&0x8000u64.to_le_bytes());
        ncz.extend_from_slice(&1u64.to_le_bytes());
        ncz.extend_from_slice(&[0; 40]);
        ncz.extend(zstd::stream::encode_all(&bytes[0x4000..], 3).unwrap());
        let mut container = Vec::new();
        container.extend_from_slice(b"PFS0");
        container.extend_from_slice(&1u32.to_le_bytes());
        container.extend_from_slice(&12u32.to_le_bytes());
        container.extend_from_slice(&[0; 4]);
        container.extend_from_slice(&0u64.to_le_bytes());
        container.extend_from_slice(&(ncz.len() as u64).to_le_bytes());
        container.extend_from_slice(&[0; 8]);
        container.extend_from_slice(b"control.ncz\0");
        container.extend_from_slice(&ncz);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&container).unwrap();
        let restored = crate::content::metadata_member(
            file.path(),
            "control.nca",
            usize::try_from(MAX_CONTROL).unwrap(),
        )
        .unwrap();
        assert_eq!(restored, bytes);
        assert!(crate::content::metadata_member(file.path(), "control.nca", 0x5000).is_err());
        let identity = IdentifiedContent {
            title_id: "0100000000000000".into(),
            app_id: "0100000000000000".into(),
            version: 0,
            kind: crate::catalog::ContentKind::Base,
        };
        assert_eq!(
            read_control(&restored, &keys, None, &identity, 0).unwrap().record["name"],
            "Game"
        );
    }
}
