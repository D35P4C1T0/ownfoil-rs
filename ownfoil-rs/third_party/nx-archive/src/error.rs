use cipher::InvalidLength;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("Unable to parse binary data: {0}")]
    BinaryParser(#[from] binrw::Error),
    #[error("Unable to parse string: {0}")]
    StringParser(#[from] core::str::Utf8Error),
    #[error("Invalid state: {0}")]
    InvalidState(String),
    #[error("Invalid format: {0}")]
    InvalidFormat(String),
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Not supported: {0}")]
    NotSupported(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Other error: {0}")]
    Other(String),
    #[error("Encryption error: {0}")]
    CryptoError(String),
    #[error("Key Lookup error: {0}")]
    KeyLookupError(String),
    #[error("Title key error: {0}")]
    TitleKeyError(#[from] crate::formats::title_keyset::KeyError),
}

impl From<InvalidLength> for Error {
    fn from(_: InvalidLength) -> Self {
        Error::CryptoError("Invalid key length".to_string())
    }
}
