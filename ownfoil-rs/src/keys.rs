//! Secret-safe validation for an Ownfoil-compatible `keys.txt`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

const LATEST_MASTER_KEY_REVISION: u8 = 0x14;

#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_field_names)]
pub struct KeyStatus {
    pub valid_keys: Option<bool>,
    pub missing_keys: Vec<String>,
    pub corrupt_keys: Vec<String>,
}

pub fn inspect(path: &Path) -> KeyStatus {
    let Ok(bytes) = std::fs::read(path) else {
        return KeyStatus {
            valid_keys: None,
            missing_keys: master_key_names(),
            corrupt_keys: Vec::new(),
        };
    };
    inspect_bytes(&bytes)
}

pub fn inspect_bytes(bytes: &[u8]) -> KeyStatus {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return invalid(Vec::new(), vec!["invalid_utf8".to_string()]);
    };
    let mut keys = BTreeMap::new();
    let mut corrupt = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            corrupt.push(format!("line_{}", index + 1));
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name.is_empty()
            || value.is_empty()
            || value.len() % 2 != 0
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            corrupt.push(if name.is_empty() { format!("line_{}", index + 1) } else { name });
            continue;
        }
        if name.starts_with("master_key_") && value.len() != 32 {
            corrupt.push(name.clone());
        }
        keys.insert(name, value.to_ascii_lowercase());
    }
    let missing =
        master_key_names().into_iter().filter(|name| !keys.contains_key(name)).collect::<Vec<_>>();
    corrupt.sort();
    corrupt.dedup();
    KeyStatus {
        valid_keys: Some(!keys.is_empty() && corrupt.is_empty()),
        missing_keys: missing,
        corrupt_keys: corrupt,
    }
}

const fn invalid(missing: Vec<String>, corrupt: Vec<String>) -> KeyStatus {
    KeyStatus { valid_keys: Some(false), missing_keys: missing, corrupt_keys: corrupt }
}

fn master_key_names() -> Vec<String> {
    (0..=LATEST_MASTER_KEY_REVISION).map(|revision| format!("master_key_{revision:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::inspect_bytes;

    #[test]
    fn reports_missing_and_corrupt_master_keys_without_echoing_values() {
        let status = inspect_bytes(
            b"master_key_00 = 00112233445566778899aabbccddeeff\nmaster_key_01 = zz\n",
        );
        assert_eq!(status.valid_keys, Some(false));
        assert!(status.corrupt_keys.contains(&"master_key_01".to_string()));
        assert!(!status.missing_keys.contains(&"master_key_00".to_string()));
    }
}

#[cfg(test)]
mod local_validation {
    #[test]
    #[ignore = "requires OWNFOIL_TEST_KEYS pointing to a local key file"]
    fn local_keys_load_without_disclosing_material() -> anyhow::Result<()> {
        let path = std::env::var_os("OWNFOIL_TEST_KEYS")
            .ok_or_else(|| anyhow::anyhow!("Set OWNFOIL_TEST_KEYS"))?;
        let status = super::inspect(std::path::Path::new(&path));
        anyhow::ensure!(status.valid_keys == Some(true), "Key file format validation failed");
        let keys = nx_archive::formats::Keyset::from_file(path)
            .map_err(|_| anyhow::anyhow!("Key loading failed"))?;
        anyhow::ensure!(keys.header_key().is_some(), "NCA header key unavailable");
        Ok(())
    }
}
