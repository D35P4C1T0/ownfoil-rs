use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use axum::http::HeaderMap;
use axum::http::header::{ACCEPT, USER_AGENT};
use rand::RngCore;
use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

pub const DEFAULT_TINFOIL_PUBLIC_KEY: &str = r"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvPdrJigQ0rZAy+jla7hS
jwen8gkF0gjtl+lZGY59KatNd9Kj2gfY7dTMM+5M2tU4Wr3nk8KWr5qKm3hzo/2C
Gbc55im3tlRl6yuFxWQ+c/I2SM5L3xp6eiLUcumMsEo0B7ELmtnHTGCCNAIzTFzV
4XcWGVbkZj83rTFxpLsa1oArTdcz5CG6qgyVe7KbPsft76DAEkV8KaWgnQiG0Dps
INFy4vISmf6L1TgAryJ8l2K4y8QbymyLeMsABdlEI3yRHAm78PSezU57XtQpHW5I
aupup8Es6bcDZQKkRsbOeR9T74tkj+k44QrjZo8xpX9tlJAKEEmwDlyAg0O5CLX3
CQIDAQAB
-----END PUBLIC KEY-----";

const TINFOIL_HEADERS: [&str; 7] =
    ["Theme", "Uid", "Version", "Revision", "Language", "Hauth", "Uauth"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Browser,
    Tinfoil,
    CyberFoil,
    Sphaira,
}

fn default_shop_motd() -> String {
    String::from("ok")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShopConfig {
    #[serde(default = "default_shop_motd")]
    pub motd: String,
    #[serde(default)]
    pub encrypt: bool,
    #[serde(default)]
    pub tinfoil_only_mode: bool,
    #[serde(default)]
    pub public_key: String,
}

impl Default for ShopConfig {
    fn default() -> Self {
        Self {
            motd: default_shop_motd(),
            encrypt: false,
            tinfoil_only_mode: false,
            public_key: String::new(),
        }
    }
}

impl ShopConfig {
    pub const fn effective_encrypt(&self) -> bool {
        self.encrypt || self.tinfoil_only_mode
    }

    pub fn public_key_pem(&self) -> &str {
        let trimmed = self.public_key.trim();
        if trimmed.is_empty() { DEFAULT_TINFOIL_PUBLIC_KEY } else { trimmed }
    }
}

#[derive(Debug, Error)]
pub enum ShopPayloadError {
    #[error("failed to serialize shop payload")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to compress shop payload")]
    Compress(#[from] std::io::Error),
    #[error("invalid tinfoil public key")]
    InvalidPublicKey,
    #[error("failed to encrypt session key")]
    SessionKeyEncrypt,
}

pub fn validate_public_key_pem(raw: &str) -> Result<(), ShopPayloadError> {
    RsaPublicKey::from_public_key_pem(raw.trim())
        .map(|_| ())
        .map_err(|_| ShopPayloadError::InvalidPublicKey)
}

pub fn is_cyberfoil_request(headers: &HeaderMap) -> bool {
    user_agent(headers) == "cyberfoil"
}

pub fn is_tinfoil_request(headers: &HeaderMap) -> bool {
    TINFOIL_HEADERS.iter().all(|header| headers.contains_key(*header))
        && !headers.contains_key(USER_AGENT)
}

pub fn is_shop_client_request(headers: &HeaderMap) -> bool {
    identify_client(headers) != ClientKind::Browser
}

pub fn identify_client(headers: &HeaderMap) -> ClientKind {
    let has_tinfoil_headers = TINFOIL_HEADERS.iter().all(|header| headers.contains_key(*header));
    if has_tinfoil_headers && is_cyberfoil_request(headers) {
        return ClientKind::CyberFoil;
    }
    if is_tinfoil_request(headers) {
        return ClientKind::Tinfoil;
    }
    if is_sphaira_request(headers) {
        return ClientKind::Sphaira;
    }
    ClientKind::Browser
}

fn is_sphaira_request(headers: &HeaderMap) -> bool {
    for required in ["host", "accept", "accept-encoding"] {
        if !headers.contains_key(required) {
            return false;
        }
    }
    headers.keys().all(|name| {
        let name = name.as_str();
        matches!(name, "host" | "accept" | "accept-encoding" | "authorization" | "range")
            || name.starts_with("x-")
    })
}

pub fn request_prefers_html(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"))
}

pub fn encrypt_shop_payload<T: Serialize>(
    payload: &T,
    public_key_pem: Option<&str>,
) -> Result<Vec<u8>, ShopPayloadError> {
    let serialized = serde_json::to_vec(payload)?;
    let compressed = zstd::encode_all(serialized.as_slice(), 22)?;
    let compressed_len = compressed.len() as u64;

    let mut aes_key = [0_u8; 16];
    OsRng.fill_bytes(&mut aes_key);

    let public_key_text = public_key_pem
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_TINFOIL_PUBLIC_KEY);
    let public_key = RsaPublicKey::from_public_key_pem(public_key_text.trim())
        .map_err(|_| ShopPayloadError::InvalidPublicKey)?;
    let session_key = public_key
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &aes_key)
        .map_err(|_| ShopPayloadError::SessionKeyEncrypt)?;

    let encrypted = encrypt_ecb_zero_padded(&compressed, &aes_key);

    let mut out = Vec::with_capacity(7 + 1 + session_key.len() + 8 + encrypted.len());
    out.extend_from_slice(b"TINFOIL");
    out.push(0xFD);
    out.extend_from_slice(&session_key);
    out.extend_from_slice(&compressed_len.to_le_bytes());
    out.extend_from_slice(&encrypted);
    Ok(out)
}

fn encrypt_ecb_zero_padded(input: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let block_count = (input.len() / 16) + 1;
    let mut padded = vec![0_u8; block_count * 16];
    padded[..input.len()].copy_from_slice(input);

    for chunk in padded.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }

    padded
}

fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}
