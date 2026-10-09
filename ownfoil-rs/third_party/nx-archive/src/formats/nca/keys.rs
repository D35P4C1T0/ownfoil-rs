use super::KeyArea;
use super::NcaHeader;
use super::types::*;
use crate::error::Error;
use crate::formats::{Keyset, TitleKeys};
use tracing;

pub struct NcaKeyManagement {
    dec_title_key: Option<[u8; 0x10]>,
    dec_key_area: KeyArea,
    key_status: bool,
}

impl NcaKeyManagement {
    pub fn new(
        header: &NcaHeader,
        keyset: &Keyset,
        title_keys: Option<&TitleKeys>,
    ) -> Result<Self, Error> {
        let mut dec_key_area = KeyArea::default();
        let mut key_status = true;

        // Process key decryption based on rights ID
        let dec_title_key = if !header.rights_id.iter().all(|&b| b == 0) {
            Self::process_title_key(header, keyset, title_keys, &mut key_status)?
        } else {
            Self::process_key_area(header, keyset, &mut dec_key_area, &mut key_status)?;
            None
        };

        Ok(Self {
            dec_title_key,
            dec_key_area,
            key_status,
        })
    }

    fn process_title_key(
        header: &NcaHeader,
        keyset: &Keyset,
        title_keys: Option<&TitleKeys>,
        key_status: &mut bool,
    ) -> Result<Option<[u8; 0x10]>, Error> {
        let rights_id_hex = hex::encode(header.rights_id).to_uppercase();
        tracing::trace!(rights_id = %rights_id_hex, "NCA requires title key");

        let key_gen = header.get_key_generation();

        if let Some(title_keys_db) = title_keys {
            let title_kek = keyset.get_title_kek(key_gen as usize);
            tracing::trace!(
                key_gen = %key_gen,
                title_kek = ?title_kek,
                "Title KEK obtained"
            );

            if let Some(title_kek) = title_kek {
                match title_keys_db.decrypt_title_key(&rights_id_hex, &title_kek) {
                    Ok(dec_key) => Ok(Some(dec_key)),
                    Err(e) => {
                        tracing::warn!("Failed to decrypt title key: {}", e);
                        *key_status = false;
                        Ok(None)
                    }
                }
            } else {
                tracing::warn!(
                    "Title key encryption key not present for key generation {}",
                    key_gen
                );
                *key_status = false;
                Ok(None)
            }
        } else {
            tracing::warn!("NCA requires title key but no title keys database was supplied");
            *key_status = false;
            Ok(None)
        }
    }

    fn process_key_area(
        header: &NcaHeader,
        keyset: &Keyset,
        dec_key_area: &mut KeyArea,
        key_status: &mut bool,
    ) -> Result<(), Error> {
        tracing::trace!("NCA does not require title key, attempting to get key area key");
        let key_gen = header.get_key_generation();

        let key_area_key = match header.key_area_appkey_index {
            KeyAreaEncryptionKeyIndex::Application => {
                let key = keyset.get_key_area_key_application(key_gen as usize);
                tracing::trace!(key_gen = %key_gen, key_type = "Application", key = ?key, "Key area key obtained");
                key
            }
            KeyAreaEncryptionKeyIndex::Ocean => {
                let key = keyset.get_key_area_key_ocean(key_gen as usize);
                tracing::trace!(key_gen = %key_gen, key_type = "Ocean", key = ?key, "Key area key obtained");
                key
            }
            KeyAreaEncryptionKeyIndex::System => {
                let key = keyset.get_key_area_key_system(key_gen as usize);
                tracing::trace!(key_gen = %key_gen, key_type = "System", key = ?key, "Key area key obtained");
                key
            }
        };

        if let Some(key) = key_area_key {
            tracing::trace!(
                encrypted_key = %hex::encode(header.encrypted_keys.aes_ctr_key),
                "Decrypting key area"
            );

            use cipher::BlockDecryptMut;
            use cipher::KeyInit;

            let mut key_area_copy = header.encrypted_keys.clone();
            type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;

            let mut decryptor = Aes128EcbDec::new_from_slice(&key)
                .map_err(|_| Error::CryptoError("Failed to create ECB decryptor".to_string()))?;

            decryptor.decrypt_blocks_mut(unsafe {
                core::slice::from_raw_parts_mut(
                    &mut key_area_copy as *mut KeyArea as *mut aes::Block,
                    std::mem::size_of::<KeyArea>() / 16,
                )
            });

            *dec_key_area = key_area_copy;

            tracing::trace!(
                decrypted_key = %hex::encode(dec_key_area.aes_ctr_key),
                "Key area decrypted"
            );
            Ok(())
        } else {
            tracing::warn!(
                key_type = ?header.key_area_appkey_index,
                key_gen = %key_gen,
                "Key area key not present"
            );
            *key_status = false;
            Ok(())
        }
    }

    pub fn has_valid_keys(&self) -> bool {
        self.key_status
    }

    pub fn get_aes_ctr_decrypt_key(&self, rights_id: &[u8; 0x10]) -> Result<[u8; 0x10], Error> {
        if !rights_id.iter().all(|&b| b == 0) {
            if let Some(dec_key) = self.dec_title_key {
                tracing::trace!(key = %hex::encode(dec_key), "Using decrypted title key");
                return Ok(dec_key);
            }

            let rights_id_hex = hex::encode(rights_id).to_uppercase();
            return Err(Error::KeyLookupError(format!(
                "NCA requires title key for rights ID {}, but it was not available or could not be decrypted",
                rights_id_hex
            )));
        }

        if !self.key_status {
            return Err(Error::KeyLookupError(
                "Key area could not be decrypted".to_string(),
            ));
        }

        tracing::trace!(key = %hex::encode(self.dec_key_area.aes_ctr_key), "Using decrypted key area key");
        Ok(self.dec_key_area.aes_ctr_key)
    }
}
