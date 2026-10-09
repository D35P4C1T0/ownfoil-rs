use aes::Aes128;
use cipher::{KeyInit, generic_array::GenericArray};
use hex::FromHex;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Result, Seek};
use std::path::Path;
use xts_mode::Xts128;

/// Builds a tweak for Nintendo XTS encryption
/// This is a non-standard tweak that has reversed endianness compared to normal XTS
pub fn get_nintendo_tweak(sector_index: u128) -> [u8; 16] {
    sector_index.to_be_bytes()
}

#[derive(Clone, Default)]
pub struct Keyset {
    // Raw storage for all keys
    pub raw_keys: HashMap<String, Vec<u8>>,

    // Keep cached versions of frequently accessed keys for performance
    pub header_key_cache: Option<[u8; 0x20]>,
}

impl fmt::Debug for Keyset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct HexBytes<'a>(&'a [u8]);

        impl fmt::Debug for HexBytes<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:.X?}", self.0)
            }
        }

        // Group keys by prefix for easier reading
        let mut grouped_keys: HashMap<&str, Vec<(&String, &Vec<u8>)>> = HashMap::new();

        for (key, value) in &self.raw_keys {
            let prefix = key.split('_').next().unwrap_or("");
            grouped_keys.entry(prefix).or_default().push((key, value));
        }

        // Sort entries for consistent output
        let mut groups: Vec<_> = grouped_keys.into_iter().collect();
        groups.sort_by(|a, b| a.0.cmp(b.0));

        let mut debug_struct = f.debug_struct("Keyset");

        // Add count of total keys
        debug_struct.field("total_keys", &self.raw_keys.len());

        // Add each group
        for (prefix, entries) in groups {
            let mut sorted_entries = entries;
            sorted_entries.sort_by(|a, b| a.0.cmp(b.0));

            let formatted_entries: Vec<_> = sorted_entries
                .iter()
                .map(|(key, value)| (key.to_string(), HexBytes(value)))
                .collect();

            debug_struct.field(prefix, &formatted_entries);
        }

        debug_struct.finish()
    }
}

impl Display for Keyset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let format_indices = |keys: &HashMap<u8, [u8; 0x10]>,
                              name: &str,
                              f: &mut fmt::Formatter<'_>|
         -> fmt::Result {
            let mut indices: Vec<_> = keys.keys().collect();
            indices.sort();
            write!(
                f,
                "{} keys: {}",
                name,
                indices
                    .iter()
                    .map(|i| format!("{:02x}", i))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        format_indices(&self.key_area_keys_application(), "Application", f)?;
        writeln!(f)?;
        format_indices(&self.key_area_keys_ocean(), "Ocean", f)?;
        writeln!(f)?;
        format_indices(&self.key_area_keys_system(), "System", f)?;
        writeln!(f)?;
        format_indices(&self.title_keks(), "Title KEK", f)
    }
}

impl Keyset {
    /// Create a new keyset from a file path
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path)?;
        Self::from_reader(file)
    }

    /// Parse a key file to extract Nintendo Switch keys
    pub fn from_reader(reader: impl Read + Seek) -> Result<Self> {
        let lines = BufReader::new(reader).lines();
        let mut keyset = Keyset::default();
        let mut keys_loaded = 0;

        for line in lines {
            let line_str = line?;

            // Skip empty lines or comments
            if line_str.trim().is_empty() || line_str.trim().starts_with(';') {
                continue;
            }

            let line_parts: Vec<_> = line_str.split('=').collect();
            if line_parts.len() != 2 {
                continue;
            }

            let key = line_parts[0].trim().to_string();
            let value = line_parts[1].trim().split(';').next().unwrap_or("").trim();

            let key_data = match Vec::from_hex(value) {
                Ok(data) => data,
                Err(_) => {
                    tracing::warn!("Invalid hex value for key {}: {}", key, value);
                    continue;
                }
            };

            // Store the raw key
            keyset.raw_keys.insert(key, key_data);
            keys_loaded += 1;
        }

        // Cache frequently used keys
        keyset.update_caches();

        tracing::info!(
            "Loaded {} keys with {} distinct prefixes",
            keys_loaded,
            keyset.get_key_prefixes().len()
        );

        Ok(keyset)
    }

    /// Update internal caches for frequently accessed keys
    fn update_caches(&mut self) {
        // Cache header key
        if let Some(key_data) = self.raw_keys.get("header_key") {
            if key_data.len() == 0x20 {
                let mut header_key = [0u8; 0x20];
                header_key.copy_from_slice(key_data);
                self.header_key_cache = Some(header_key);
            }
        }
    }

    /// Creates an XTS128 cipher for NCA header encryption/decryption
    ///
    /// The header key is split into two 128-bit keys for XTS, with the first half used for the data unit key
    /// and the second half used for the tweak key.
    pub fn header_crypt(&self) -> Option<Xts128<Aes128>> {
        self.header_key_cache.map(|header_key| {
            let cipher_1 = Aes128::new(GenericArray::from_slice(&header_key[..0x10]));
            let cipher_2 = Aes128::new(GenericArray::from_slice(&header_key[0x10..]));
            Xts128::new(cipher_1, cipher_2)
        })
    }

    /// Get a list of all key prefixes in the keyset
    pub fn get_key_prefixes(&self) -> Vec<String> {
        let mut prefixes = std::collections::HashSet::new();

        for key in self.raw_keys.keys() {
            // For keys with format like key_area_key_application_00,
            // we want the prefix to be key_area_key_application
            let parts: Vec<&str> = key.split('_').collect();
            if parts.len() > 1 {
                // Check if the last part is a hex number (like "00", "01", etc.)
                if let Some(last) = parts.last() {
                    if u8::from_str_radix(last, 16).is_ok() {
                        // If last part is a hex number, use everything before it as prefix
                        let prefix = parts[..parts.len() - 1].join("_");
                        prefixes.insert(prefix);
                        continue;
                    }
                }
            }

            // For non-indexed keys or as a fallback, insert the whole key
            prefixes.insert(key.clone());
        }

        let mut prefix_list: Vec<String> = prefixes.into_iter().collect();
        prefix_list.sort();
        prefix_list
    }

    /// Get all keys with a specific prefix
    pub fn get_keys_with_prefix(&self, prefix: &str) -> HashMap<String, Vec<u8>> {
        self.raw_keys
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Try to get a raw key by its exact name
    pub fn get_raw_key(&self, key_name: &str) -> Option<&[u8]> {
        self.raw_keys.get(key_name).map(|v| v.as_slice())
    }

    /// Try to get a fixed-size key by its exact name
    pub fn get_key<const N: usize>(&self, key_name: &str) -> Option<[u8; N]> {
        self.raw_keys.get(key_name).and_then(|data| {
            if data.len() == N {
                let mut result = [0u8; N];
                result.copy_from_slice(data);
                Some(result)
            } else {
                None
            }
        })
    }

    /// Get the header key (cached for performance)
    pub fn header_key(&self) -> Option<&[u8; 0x20]> {
        self.header_key_cache.as_ref()
    }

    /// Extract indexes from keys with a specific format (e.g., titlekek_10 => 0x10)
    pub fn get_indexed_keys<const N: usize>(&self, prefix: &str) -> HashMap<u8, [u8; N]> {
        let mut result = HashMap::new();
        let prefix_with_underscore = format!("{}_", prefix);

        for (key, value) in &self.raw_keys {
            if key.starts_with(&prefix_with_underscore) && value.len() == N {
                // Extract the index part (everything after the last underscore)
                if let Some(idx_str) = key.split('_').last() {
                    if let Ok(idx) = u8::from_str_radix(idx_str, 16) {
                        let mut fixed_arr = [0u8; N];
                        fixed_arr.copy_from_slice(value);
                        result.insert(idx, fixed_arr);
                    }
                }
            }
        }

        result
    }

    /// Get an application key area key by index
    pub fn get_key_area_key_application(&self, idx: usize) -> Option<[u8; 0x10]> {
        let key_name = format!("key_area_key_application_{:02x}", idx as u8);
        self.get_key(&key_name)
    }

    /// Get an ocean key area key by index
    pub fn get_key_area_key_ocean(&self, idx: usize) -> Option<[u8; 0x10]> {
        let key_name = format!("key_area_key_ocean_{:02x}", idx as u8);
        self.get_key(&key_name)
    }

    /// Get a system key area key by index
    pub fn get_key_area_key_system(&self, idx: usize) -> Option<[u8; 0x10]> {
        let key_name = format!("key_area_key_system_{:02x}", idx as u8);
        self.get_key(&key_name)
    }

    /// Get the title KEK for the specified key generation
    pub fn get_title_kek(&self, key_generation: usize) -> Option<[u8; 0x10]> {
        let key_name = format!("titlekek_{:02x}", key_generation as u8);
        self.get_key(&key_name)
    }

    /// Get all title KEKs as a HashMap indexed by generation
    pub fn title_keks(&self) -> HashMap<u8, [u8; 0x10]> {
        self.get_indexed_keys("titlekek")
    }

    /// Get all application key area keys as a HashMap indexed by generation
    pub fn key_area_keys_application(&self) -> HashMap<u8, [u8; 0x10]> {
        self.get_indexed_keys("key_area_key_application")
    }

    /// Get all ocean key area keys as a HashMap indexed by generation
    pub fn key_area_keys_ocean(&self) -> HashMap<u8, [u8; 0x10]> {
        self.get_indexed_keys("key_area_key_ocean")
    }

    /// Get all system key area keys as a HashMap indexed by generation
    pub fn key_area_keys_system(&self) -> HashMap<u8, [u8; 0x10]> {
        self.get_indexed_keys("key_area_key_system")
    }

    /// Check if the keyset has the necessary keys for NCA decryption
    pub fn has_required_nca_keys(&self) -> bool {
        self.header_key_cache.is_some()
            && !self.key_area_keys_application().is_empty()
            && !self.title_keks().is_empty()
    }

    /// Get key area key by key type and crypto index
    pub fn get_key_area_key(&self, key_type: u8, crypto_type: u8) -> Option<[u8; 0x10]> {
        match key_type {
            0 => self.get_key_area_key_application(crypto_type as usize),
            1 => self.get_key_area_key_ocean(crypto_type as usize),
            2 => self.get_key_area_key_system(crypto_type as usize),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing_test::traced_test;

    use super::*;

    #[test]
    #[traced_test]
    fn test_load_keyset() {
        let keyset = Keyset::from_file("prod.keys").unwrap();

        tracing::info!("{:#?}", keyset);

        // Print all available key indices for debugging
        tracing::info!("Available key indices:\n{}", keyset);

        tracing::info!(
            appkey_len = keyset.key_area_keys_application().len(),
            "Loaded {} application key(s)",
            keyset.key_area_keys_application().len()
        );

        // Test various key generations including beyond 0xF
        for idx in [0, 1, 10, 16, 17, 18] {
            let app_key = keyset.get_key_area_key_application(idx);
            tracing::info!(
                idx = idx,
                has_key = app_key.is_some(),
                "Application key generation {:02x}",
                idx
            );

            let title_kek = keyset.get_title_kek(idx);
            tracing::info!(
                idx = idx,
                has_key = title_kek.is_some(),
                "Title KEK generation {:02x}",
                idx
            );
        }

        tracing::info!("Keyset loaded successfully");
    }

    #[test]
    #[traced_test]
    fn test_key_parsing_edge_cases() {
        // Create a test key file with keys for 0x10-0x12
        let test_keys = r#"
        header_key = 0000000000000000000000000000000000000000000000000000000000000000
        key_area_key_application_00 = 00000000000000000000000000000000
        key_area_key_application_10 = 1010101010101010101010101010101a
        key_area_key_application_11 = 1111111111111111111111111111111b
        key_area_key_application_12 = 1212121212121212121212121212121c
        titlekek_00 = 00000000000000000000000000000000
        titlekek_10 = 1010101010101010101010101010101a
        titlekek_11 = 1111111111111111111111111111111b
        titlekek_12 = 1212121212121212121212121212121c
        ; Add some custom keys to test the new flexible system
        custom_test_key = aabbccddeeff00112233445566778899
        another_custom_key = 99887766554433221100ffeeddccbbaa
        "#;

        let cursor = std::io::Cursor::new(test_keys);
        let keyset = Keyset::from_reader(cursor).unwrap();

        // Test the indexed keys
        let app_keys = keyset.key_area_keys_application();
        let title_keks = keyset.title_keks();

        assert!(app_keys.contains_key(&0));
        assert!(app_keys.contains_key(&16)); // 0x10
        assert!(app_keys.contains_key(&17)); // 0x11
        assert!(app_keys.contains_key(&18)); // 0x12

        assert!(title_keks.contains_key(&0));
        assert!(title_keks.contains_key(&16)); // 0x10
        assert!(title_keks.contains_key(&17)); // 0x11
        assert!(title_keks.contains_key(&18)); // 0x12

        // Check values of indexed keys
        if let Some(key) = keyset.get_key_area_key_application(16) {
            let expected_last_byte = 0x1a;
            assert_eq!(
                key[15], expected_last_byte,
                "Key value mismatch for app key 0x10"
            );
        }

        if let Some(key) = keyset.get_title_kek(16) {
            let expected_last_byte = 0x1a;
            assert_eq!(
                key[15], expected_last_byte,
                "Key value mismatch for titlekek 0x10"
            );
        }

        // Test custom keys
        let custom_key = keyset.get_key::<16>("custom_test_key");
        assert!(custom_key.is_some(), "Custom key should be loaded");

        if let Some(key) = custom_key {
            assert_eq!(key[0], 0xaa);
            assert_eq!(key[15], 0x99);
        }

        // Test prefixes
        let prefixes = keyset.get_key_prefixes();
        assert!(prefixes.contains(&"key_area_key_application".to_string()));
        assert!(prefixes.contains(&"titlekek".to_string()));
        assert!(prefixes.contains(&"custom_test_key".to_string()));
        assert!(prefixes.contains(&"another_custom_key".to_string()));

        // Test getting keys by prefix
        let custom_keys = keyset.get_keys_with_prefix("custom");
        assert_eq!(custom_keys.len(), 1); // Fix: only "custom_test_key" starts with prefix "custom"

        let title_keys = keyset.get_keys_with_prefix("titlekek");
        assert_eq!(title_keys.len(), 4);
    }

    #[test]
    fn test_header_key_and_crypt() {
        let test_keys = r#"
        header_key = 0000000000000000000000000000000000000000000000000000000000000001
        "#;

        let cursor = std::io::Cursor::new(test_keys);
        let keyset = Keyset::from_reader(cursor).unwrap();

        // Check if header key was loaded and cached
        let header_key = keyset.header_key();
        assert!(header_key.is_some(), "Header key should be cached");

        if let Some(key) = header_key {
            assert_eq!(key[31], 0x01);
        }

        // Check if header_crypt returns a valid cipher
        let cipher = keyset.header_crypt();
        assert!(cipher.is_some(), "Header cipher should be created");
    }
}
