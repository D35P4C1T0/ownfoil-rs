//! Utility helpers and traits for nx-archive.

use std::io::{Read, Seek};

/// A trait that combines Read and Seek, used to simplify type bounds.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

use crate::{
    formats::{Keyset, TitleKeys, cnmt::Cnmt},
    io::SubFile,
};

pub trait TitleDataExt {
    fn get_cnmts(
        &mut self,
        keyset: &Keyset,
        title_keyset: Option<&TitleKeys>,
    ) -> Result<Vec<Cnmt>, crate::error::Error>;
    fn title_id(&self) -> Result<u64, crate::error::Error>;
    fn title_id_serialized(&self) -> Result<String, crate::error::Error> {
        Ok(format!("{:016X}", self.title_id()?))
    }
}

pub trait FileEntryExt<R: Read + Seek> {
    type FS: VirtualFSExt<R>;

    /// Returns a virtual file reader that buffers and reads the file
    ///
    /// This is useful for reading files from archives, which may be compressed or encrypted.
    /// And for reading large files, which may cause performance issues if the entire file is read into memory.
    fn file_reader(&self, fs: &mut Self::FS) -> Result<SubFile<R>, crate::error::Error>;

    /// Returns the size of the file in bytes.
    fn file_size(&self) -> u64;

    /// Read the whole file into memory, and returns a Vec<u8>.
    ///
    /// This is not recommended for large files, as it may cause performance issues.
    fn read_bytes(&self, fs: &mut Self::FS, size: usize) -> Result<Vec<u8>, crate::error::Error>;

    /// Get the file name of the entry.
    fn file_name(&self) -> String;

    /// Get the file extension, if any.
    fn file_extension(&self) -> Option<String> {
        self.file_name()
            .split('.')
            .last()
            .map(|ext| ext.to_string())
    }
}

pub trait VirtualFSExt<R: Read + Seek> {
    type Entry: FileEntryExt<R, FS = Self>;

    /// List files in the archive
    fn list_files(&self) -> Result<Vec<Self::Entry>, crate::error::Error>;

    /// Get a file by name
    fn get_file(&self, name: &str) -> Result<Option<Self::Entry>, crate::error::Error>;

    /// Create a SubFile reader for a given file entry
    fn create_reader(&mut self, file: &Self::Entry) -> Result<SubFile<R>, crate::error::Error>;
}
