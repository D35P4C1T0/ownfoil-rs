use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use cipher::{BlockDecrypt, KeyInit};
use hex::decode as hex_decode;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Error, Debug)]
pub enum KeyError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Invalid key format: {0}")]
    InvalidFormat(String),

    #[error("Failed to decode hex value: {0}")]
    HexDecodeError(#[from] hex::FromHexError),

    #[error("Key not found: {0}")]
    KeyNotFound(String),
}

/// Stores title keys for decryption
#[derive(Default, Debug)]
pub struct TitleKeys {
    keys: HashMap<String, Vec<u8>>,
    loaded_file: Option<String>,
}

impl TitleKeys {
    /// Create a new empty TitleKeys instance
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            loaded_file: None,
        }
    }

    /// Add a title key to the database
    pub fn add_title_key(&mut self, rights_id: &str, key: Vec<u8>) {
        self.keys.insert(rights_id.to_uppercase(), key);
    }

    /// Get a title key by rights ID
    pub fn get_title_key(&self, rights_id: &str) -> Option<&Vec<u8>> {
        self.keys.get(&rights_id.to_uppercase())
    }

    /// Decrypt a title key using the title KEK
    pub fn decrypt_title_key(
        &self,
        rights_id: &str,
        title_kek: &[u8],
    ) -> Result<[u8; 16], KeyError> {
        let enc_key = self
            .get_title_key(rights_id)
            .ok_or_else(|| KeyError::KeyNotFound(rights_id.to_string()))?;

        let mut key_bytes = [0u8; 16];
        key_bytes.copy_from_slice(&enc_key[0..16]);

        let mut block = GenericArray::from(key_bytes);
        let key = GenericArray::from_slice(title_kek);

        Aes128::new(key).decrypt_block(&mut block);

        Ok(*block.as_ref())
    }

    /// Load title keys from a file, following the NSTools format for title.keys
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, KeyError> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to open title key file {:?}: {}", path.as_ref(), e);
                return Err(KeyError::Io(e));
            }
        };
        let reader = io::BufReader::new(file);

        let mut keys = TitleKeys::new();
        keys.loaded_file = Some(path.as_ref().to_string_lossy().to_string());

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }

            // Parse key-value format "rights_id = hex_key"
            if let Some(pos) = line.find('=') {
                let rights_id = line[..pos].trim();
                let key_hex = line[pos + 1..].trim();

                // Check if this is a valid format for a title key (32 char rights_id)
                if rights_id.len() == 32 {
                    match hex_decode(key_hex) {
                        Ok(key) => {
                            if key.len() == 16 {
                                keys.add_title_key(rights_id, key);
                                info!("Loaded title key for rights ID: {}", rights_id);
                            } else {
                                warn!(
                                    "Invalid key length for rights ID {} at line {}: expected 16 bytes, got {}",
                                    rights_id,
                                    line_num + 1,
                                    key.len()
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to decode hex key for rights ID {} at line {}: {}",
                                rights_id,
                                line_num + 1,
                                e
                            );
                        }
                    }
                }
            }
        }

        info!(
            "Loaded {} title keys from {}",
            keys.len(),
            path.as_ref().display()
        );
        Ok(keys)
    }

    /// Load title keys from the default location: ~/.switch/title.keys
    pub fn load_default() -> Result<Self, KeyError> {
        let home_dir = dirs::home_dir();

        let possible_paths = vec![
            // ~/.switch/title.keys
            home_dir
                .as_ref()
                .map(|h| h.join(".switch").join("title.keys")),
            // Current directory title.keys
            Some(Path::new("title.keys").to_path_buf()),
        ];

        let mut last_error = None;

        for path in possible_paths.into_iter().flatten() {
            if path.exists() {
                match Self::load_from_file(&path) {
                    Ok(keys) => {
                        info!("Successfully loaded title keys from {}", path.display());
                        return Ok(keys);
                    }
                    Err(e) => {
                        warn!("Failed to load title keys from {}: {}", path.display(), e);
                        last_error = Some(e);
                    }
                }
            }
        }

        // If we get here, we couldn't find/load any title keys
        Err(last_error.unwrap_or_else(|| {
            KeyError::InvalidFormat("No title.keys file found in default locations".to_string())
        }))
    }

    /// Get the number of keys in the database
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Check if the database is empty
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Get the path of the loaded file, if any
    pub fn loaded_file(&self) -> Option<&str> {
        self.loaded_file.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_keys() {
        let mut keys = TitleKeys::new();

        keys.add_title_key("foo", vec![0; 16]);
        keys.add_title_key("bar", vec![1; 16]);

        assert_eq!(keys.get_title_key("foo").unwrap(), &vec![0; 16]);
        assert_eq!(keys.get_title_key("bar").unwrap(), &vec![1; 16]);

        assert_eq!(keys.len(), 2);

        assert!(!keys.is_empty());

        assert!(keys.get_title_key("baz").is_none());
    }

    #[test]
    fn test_case_insensitive_keys() {
        let mut keys = TitleKeys::new();

        keys.add_title_key("abc123", vec![0; 16]);

        assert_eq!(keys.get_title_key("ABC123").unwrap(), &vec![0; 16]);
        assert_eq!(keys.get_title_key("abc123").unwrap(), &vec![0; 16]);
    }
}
