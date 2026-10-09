//! The Nintendo Hashed filesystem (HFS0) is a filesystem used by the Nintendo Switch to store data in a hashed format.
//! This filesystem is used in the Nintendo Switch's game cards (the little bitter carts that you insert physically into the console).
//!
//! This module doesn't allow you to eat the game itself, but lets you dump data
//! from the game card.
//!
//! You still require the XCI module to read the game card image format, which in turn contains this filesystem.
//! For the game card image format, see [xci](crate::formats::xci).

use crate::{
    FileEntryExt, VirtualFSExt,
    io::{SharedReader, SubFile},
};
use binrw::prelude::*;
use std::io::{Read, Seek, SeekFrom};

/// Nintendo Switch HFS0 (Hashed File System 0) header structure
///
/// This header is located at the beginning of an HFS0 archive file and contains:
/// - A magic identifier "HFS0" (validated by binrw)
///
/// The "SHA-256 File System" or "HFS0" starts at offset 0x10000 in the Gamecard.
/// The first 0x200 bytes act as a global header and
/// represent the root partition which points to the other partitions
/// ("normal", "logo", "update" and "secure").
#[derive(Debug)]
#[binrw]
#[brw(little, magic = b"HFS0")]
pub struct Hfs0Header {
    // magic field is handled by binrw using the magic attribute
    pub file_count: u32,
    pub string_table_size: u32,
    pub _reserved: u32,
    #[br(count = file_count)]
    pub file_entries: Vec<Hfs0Entry>,
    /// String table - 00-padded to align the start of raw filedata with a sector/media unit boundary
    #[br(pad_size_to = string_table_size)]
    #[br(count = string_table_size)]
    pub string_table: Vec<u8>,
    // the rest is raw file data, which is not parsed by this struct
    // due to memory constraints

    // We will seek and read as needed
}

#[derive(Debug)]
#[binrw]
#[brw(little)]
pub struct Hfs0Entry {
    /// Offset of the file in data
    pub offset: u64,
    /// File size
    pub size: u64,
    /// Offset of filename in string table
    pub filename_offset: u32,
    /// Size of hashed region of file
    /// (for HFS0s, this is the size of the pre-filedata portion,
    /// for NCAs this is usually 0x200)
    pub hashed_region_size: u32,
    /// Reserved field
    pub _reserved: u64,
    /// SHA-256 hash of the first (size of hashed region) bytes of filedata
    pub sha256: [u8; 0x20],
}

#[derive(Debug)]
pub struct Hfs0File {
    pub name: String,
    pub size: u64,
    /// Offset of the start of the file in the HFS0
    ///
    /// In our case, this offset is absolute to the start of the HFS0 file.
    pub offset: u64,
    pub hash: [u8; 0x20],
}

#[derive(Debug)]
pub struct Hfs0<R: Read + Seek> {
    pub header: Hfs0Header,
    pub reader: R,
}

impl<R: Read + Seek> Hfs0<R> {
    /// Create a new HFS0 parser from a reader
    pub fn from_reader(mut reader: R) -> Result<Self, crate::error::Error> {
        let header = Hfs0Header::read(&mut reader)?;
        Ok(Self { header, reader })
    }
}

impl<R: Read + Seek + Clone> Hfs0<R> {
    /// Convert this HFS0 to use a shared reader
    pub fn into_shared(self) -> Result<Hfs0<SharedReader<R>>, crate::error::Error> {
        Ok(Hfs0 {
            reader: SharedReader::new(self.reader),
            header: self.header,
        })
    }
}

impl<R: Read + Seek> Hfs0<SharedReader<R>> {
    /// Create a new HFS0 parser from a shared reader
    pub fn from_shared(reader: SharedReader<R>) -> Result<Self, crate::error::Error> {
        Self::from_reader(reader)
    }
}

impl<R: Read + Seek> Hfs0<R> {
    /// Reads the file data into a vector of bytes
    ///
    /// Previously known as `list_files`, which is ironically a misnomer
    pub fn read_to_vec(&mut self, file: &Hfs0File) -> Result<Vec<u8>, crate::error::Error> {
        self.reader
            .seek(SeekFrom::Start(file.offset))
            .map_err(crate::error::Error::Io)?;
        let mut data = vec![0; file.size as usize];
        self.reader
            .read_exact(&mut data)
            .map_err(crate::error::Error::Io)?;
        Ok(data)
    }

    /// Read file data from the HFS0 archive into a provided buffer
    ///
    /// # Arguments
    /// * `file` - The HFS0 file entry containing offset and size information
    /// * `buf` - The pre-allocated buffer to read the file data into. Must be exactly the size of the file.
    ///
    /// # Returns
    /// * `Result<(), crate::error::Error>` - Ok(()) on successful read, Error if read fails
    ///
    /// # Errors
    /// * `crate::error::Error::Io` - If seeking or reading from the underlying reader fails
    ///
    /// # Implementation Details
    /// - Performs a single read operation for the entire file
    /// - Uses absolute offset positioning from the start of the archive
    /// - Expects the provided buffer to be exactly the size of the file
    ///
    /// # Example
    /// ```no_run
    /// # use std::fs::File;
    /// # use nx_archive::formats::hfs0::Hfs0;
    /// let hfs0_image = File::open("path/to/file.hfs0").unwrap();
    /// let mut hfs0 = Hfs0::from_reader(hfs0_image).unwrap();
    /// let file = hfs0.list_files().unwrap();
    /// let file = file.first().unwrap();
    /// let mut buffer = vec![0u8; file.size as usize];
    /// hfs0.read_buf(file, &mut buffer).unwrap();
    /// ```
    pub fn read_buf(&mut self, file: &Hfs0File, buf: &mut [u8]) -> Result<(), crate::error::Error> {
        self.reader.seek(SeekFrom::Start(file.offset))?;
        self.reader.read_exact(buf)?;
        Ok(())
    }

    /// Create a SubFile reader for a given file entry
    pub fn subfile(&mut self, file: &Hfs0File) -> SubFile<R>
    where
        R: Clone,
    {
        SubFile::new(self.reader.clone(), file.offset, file.offset + file.size)
    }

    /// List all files in the HFS0 archive
    pub fn list_files(&self) -> Result<Vec<Hfs0File>, crate::error::Error> {
        self.header
            .file_entries
            .iter()
            .map(|entry| {
                let filename_bytes = &self.header.string_table[entry.filename_offset as usize..];
                let end = filename_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(filename_bytes.len());
                let name = std::str::from_utf8(&filename_bytes[..end])
                    .map_err(|e| crate::error::Error::InvalidData(e.to_string()))?;
                self.get_file(name)?.ok_or_else(|| {
                    crate::error::Error::InvalidState("File not found after parsing".to_string())
                })
            })
            .collect()
    }

    /// Get a file from the HFS0 archive by name
    ///
    /// # Arguments
    /// * `name` - The name of the file to get
    ///
    /// # Returns
    /// * `Result<Option<Hfs0File>, crate::error::Error>` - The file if found, None otherwise
    ///
    /// # Errors
    /// * `crate::error::Error::InvalidData` - If the file name is not valid UTF-8
    pub fn get_file(&self, name: &str) -> Result<Option<Hfs0File>, crate::error::Error> {
        // Calculate the header size: 16 bytes for the HFS0 header fields + file entries + string table
        let header_size = 16
            + (self.header.file_entries.len() * std::mem::size_of::<Hfs0Entry>())
            + self.header.string_table_size as usize;

        self.header
            .file_entries
            .iter()
            .find_map(|entry| {
                let filename_bytes = &self.header.string_table[entry.filename_offset as usize..];
                let end = filename_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(filename_bytes.len());
                let entry_name = std::str::from_utf8(&filename_bytes[..end])
                    .map_err(|e| crate::error::Error::InvalidData(e.to_string()))
                    .ok()?;

                if entry_name == name {
                    Some(Hfs0File {
                        name: entry_name.to_string(),
                        size: entry.size,
                        offset: entry.offset + header_size as u64,
                        hash: entry.sha256,
                    })
                } else {
                    None
                }
            })
            .map_or(Ok(None), |file| Ok(Some(file)))
    }
}

impl<R: Read + Seek + Clone> VirtualFSExt<R> for Hfs0<R> {
    type Entry = Hfs0File;

    fn list_files(&self) -> Result<Vec<Self::Entry>, crate::error::Error> {
        self.list_files()
    }

    fn get_file(&self, name: &str) -> Result<Option<Self::Entry>, crate::error::Error> {
        self.get_file(name)
    }

    fn create_reader(&mut self, file: &Self::Entry) -> Result<SubFile<R>, crate::error::Error> {
        let offset = file.offset;
        Ok(SubFile::new(
            self.reader.clone(),
            offset,
            offset + file.size,
        ))
    }
}

impl<R: Read + Seek + Clone> FileEntryExt<R> for Hfs0File {
    type FS = Hfs0<R>;

    fn file_reader(&self, fs: &mut Self::FS) -> Result<SubFile<R>, crate::error::Error> {
        fs.create_reader(self)
    }

    fn file_size(&self) -> u64 {
        self.size
    }

    fn read_bytes(&self, fs: &mut Self::FS, size: usize) -> Result<Vec<u8>, crate::error::Error> {
        let mut buf = vec![0; size];
        let mut reader = self.file_reader(fs)?;
        reader
            .read_exact(&mut buf)
            .map_err(crate::error::Error::Io)?;
        Ok(buf)
    }

    fn file_name(&self) -> String {
        self.name.clone()
    }
}
