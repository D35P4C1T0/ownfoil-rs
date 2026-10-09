use crate::{
    FileEntryExt, VirtualFSExt,
    error::Error,
    io::{SharedReader, SubFile},
};
use binrw::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// RomFS header structure
#[binrw]
#[derive(Debug, Clone)]
#[br(little)]
pub struct RomFsHeader {
    pub header_size: u32,
    pub dir_hash_table_offset: u64,
    pub dir_hash_table_size: u32,
    pub dir_table_offset: u64,
    pub dir_table_size: u32,
    pub file_hash_table_offset: u64,
    pub file_hash_table_size: u32,
    pub file_table_offset: u64,
    pub file_table_size: u32,
    pub file_data_offset: u64,
}

/// Directory entry structure
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub parent_offset: u32,
    pub sibling_offset: u32,
    pub child_dir_offset: u32,
    pub child_file_offset: u32,
    pub hash_sibling_offset: u32,
    pub name_size: u32,
    pub name: String,
}

/// File entry structure
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub parent_offset: u32,
    pub sibling_offset: u32,
    pub data_offset: u64,
    pub data_size: u64,
    pub hash_sibling_offset: u32,
    pub name_size: u32,
    pub name: String,
}

/// A RomFS directory iterator
pub struct RomFsDirectoryIterator<R: Read + Seek> {
    romfs: Arc<Mutex<RomFs<R>>>,
    dir_offsets: Vec<u32>,
    file_offsets: Vec<u32>,
    current_dir_index: usize,
    current_file_index: usize,
}

impl<R: Read + Seek> RomFsDirectoryIterator<R> {
    /// Returns the next directory name or None if there are no more directories
    pub fn next_dir(&mut self) -> Option<Result<String, Error>> {
        if self.current_dir_index >= self.dir_offsets.len() {
            return None;
        }

        let mut romfs = self.romfs.lock().unwrap();
        let result = match romfs.read_dir_entry(self.dir_offsets[self.current_dir_index]) {
            Ok(dir) => Ok(dir.name),
            Err(e) => Err(e),
        };

        self.current_dir_index += 1;
        Some(result)
    }

    /// Returns the next file name and size or None if there are no more files
    pub fn next_file(&mut self) -> Option<Result<(String, u64), Error>> {
        if self.current_file_index >= self.file_offsets.len() {
            return None;
        }

        let mut romfs = self.romfs.lock().unwrap();
        let result = match romfs.read_file_entry(self.file_offsets[self.current_file_index]) {
            Ok(file) => Ok((file.name, file.data_size)),
            Err(e) => Err(e),
        };

        self.current_file_index += 1;
        Some(result)
    }

    /// Reset the directory iterator to the beginning
    pub fn rewind(&mut self) {
        self.current_dir_index = 0;
        self.current_file_index = 0;
    }

    /// Returns the number of directories in this iterator
    pub fn dir_count(&self) -> usize {
        self.dir_offsets.len()
    }

    /// Returns the number of files in this iterator
    pub fn file_count(&self) -> usize {
        self.file_offsets.len()
    }
}

#[derive(Debug)]
/// RomFS representation
pub struct RomFs<R: Read + Seek> {
    reader: R,
    header: RomFsHeader,
    dir_hash_table: Vec<u32>,
    file_hash_table: Vec<u32>,
    cache_dir_entries: HashMap<u32, DirectoryEntry>,
    cache_file_entries: HashMap<u32, FileEntry>,
}

impl<R: Read + Seek> RomFs<R> {
    /// Magic value for invalid entry offsets
    pub const INVALID_ENTRY: u32 = u32::MAX;
    /// Offset of the root directory
    pub const ROOT_DIR_OFFSET: u32 = 0;
    /// Maximum reasonable header size (to prevent excessive allocations)
    const MAX_REASONABLE_HEADER_SIZE: u32 = 0x10000000; // 256MB
    /// Maximum reasonable table size (to prevent excessive allocations)
    const MAX_REASONABLE_TABLE_SIZE: u32 = 0x10000000; // 256MB

    /// Create a new RomFS parser from a reader
    pub fn from_reader(mut reader: R) -> Result<Self, Error> {
        // Read and dump the first 64 bytes for debugging
        let mut preview = [0u8; 64];
        let reader_clone = &mut reader;
        reader_clone.seek(SeekFrom::Start(0))?;
        let bytes_read = reader_clone.read(&mut preview)?;
        tracing::trace!(
            preview_hex = %hex::encode(&preview[..bytes_read]),
            "First {} bytes of RomFS data", bytes_read
        );

        // Reset position for actual parsing
        reader.seek(SeekFrom::Start(0))?;

        // Read the header
        let header: RomFsHeader = match reader.read_le() {
            Ok(h) => h,
            Err(e) => {
                return Err(Error::InvalidData(format!(
                    "Failed to parse RomFS header: {}",
                    e
                )));
            }
        };

        tracing::trace!(
            header_size = header.header_size,
            dir_hash_table_offset = header.dir_hash_table_offset,
            dir_hash_table_size = header.dir_hash_table_size,
            dir_table_offset = header.dir_table_offset,
            dir_table_size = header.dir_table_size,
            file_hash_table_offset = header.file_hash_table_offset,
            file_hash_table_size = header.file_hash_table_size,
            file_table_offset = header.file_table_offset,
            file_table_size = header.file_table_size,
            file_data_offset = header.file_data_offset,
            "RomFS header parsed"
        );

        // Validate header values to avoid security issues with large allocations
        if header.header_size == 0 || header.header_size > Self::MAX_REASONABLE_HEADER_SIZE {
            return Err(Error::InvalidData(format!(
                "Invalid header size: {}",
                header.header_size
            )));
        }

        if header.dir_hash_table_size > Self::MAX_REASONABLE_TABLE_SIZE {
            return Err(Error::InvalidData(format!(
                "Dir hash table too large: {}",
                header.dir_hash_table_size
            )));
        }

        if header.file_hash_table_size > Self::MAX_REASONABLE_TABLE_SIZE {
            return Err(Error::InvalidData(format!(
                "File hash table too large: {}",
                header.file_hash_table_size
            )));
        }

        // Validate hash table offsets are within reasonable bounds
        if header.dir_hash_table_offset == 0 {
            return Err(Error::InvalidData(
                "Directory hash table offset is 0".into(),
            ));
        }

        if header.file_hash_table_offset == 0 {
            return Err(Error::InvalidData("File hash table offset is 0".into()));
        }

        // Read the directory hash table
        reader.seek(SeekFrom::Start(header.dir_hash_table_offset))?;
        let dir_hash_count = header.dir_hash_table_size as usize / std::mem::size_of::<u32>();

        tracing::trace!(
            dir_hash_count = dir_hash_count,
            "Reading directory hash table"
        );

        let mut dir_hash_table = vec![0u32; dir_hash_count];
        for entry in dir_hash_table.iter_mut() {
            match reader.read_le() {
                Ok(hash) => *entry = hash,
                Err(e) => return Err(Error::BinaryParser(e)),
            }
        }

        // Read the file hash table
        reader.seek(SeekFrom::Start(header.file_hash_table_offset))?;
        let file_hash_count = header.file_hash_table_size as usize / std::mem::size_of::<u32>();

        tracing::trace!(file_hash_count = file_hash_count, "Reading file hash table");

        let mut file_hash_table = vec![0u32; file_hash_count];
        for entry in file_hash_table.iter_mut() {
            match reader.read_le() {
                Ok(hash) => *entry = hash,
                Err(e) => return Err(Error::BinaryParser(e)),
            }
        }

        Ok(Self {
            reader,
            header,
            dir_hash_table,
            file_hash_table,
            cache_dir_entries: HashMap::new(),
            cache_file_entries: HashMap::new(),
        })
    }

    /// Compute a hash for a given parent offset and name
    fn compute_hash(&self, parent: u32, name: &[u8], table_size: usize) -> u32 {
        let mut hash = parent ^ 123456789;
        for &b in name {
            hash = hash.rotate_right(5);
            hash ^= b as u32;
        }
        hash % (table_size as u32)
    }

    pub fn list_files(&mut self) -> Result<Vec<FileEntry>, Error> {
        let mut files = Vec::new();

        for offset in 0..self.file_hash_table.len() {
            if let Ok(file) = self.read_file_entry(offset as u32) {
                files.push(file);
            }
        }

        Ok(files)
    }

    /// Read a directory entry from the directory table
    fn read_dir_entry(&mut self, offset: u32) -> Result<DirectoryEntry, Error> {
        // Check the cache first
        if let Some(entry) = self.cache_dir_entries.get(&offset) {
            return Ok(entry.clone());
        }

        // If not in cache, read from file
        let reader = &mut self.reader;
        reader.seek(SeekFrom::Start(
            self.header.dir_table_offset + offset as u64,
        ))?;

        let parent_offset: u32 = reader.read_le()?;
        let sibling_offset: u32 = reader.read_le()?;
        let child_dir_offset: u32 = reader.read_le()?;
        let child_file_offset: u32 = reader.read_le()?;
        let hash_sibling_offset: u32 = reader.read_le()?;
        let name_size: u32 = reader.read_le()?;

        let mut name_bytes = vec![0u8; name_size as usize];
        reader.read_exact(&mut name_bytes)?;

        let name = String::from_utf8(name_bytes).map_err(|e| Error::InvalidData(e.to_string()))?;

        Ok(DirectoryEntry {
            parent_offset,
            sibling_offset,
            child_dir_offset,
            child_file_offset,
            hash_sibling_offset,
            name_size,
            name,
        })
    }

    /// Read a file entry from the file table
    fn read_file_entry(&mut self, offset: u32) -> Result<FileEntry, Error> {
        // Check the cache first
        if let Some(entry) = self.cache_file_entries.get(&offset) {
            return Ok(entry.clone());
        }

        // If not in cache, read from file
        let reader = &mut self.reader;
        reader.seek(SeekFrom::Start(
            self.header.file_table_offset + offset as u64,
        ))?;

        let parent_offset: u32 = reader.read_le()?;
        let sibling_offset: u32 = reader.read_le()?;
        let data_offset: u64 = reader.read_le()?;
        let data_size: u64 = reader.read_le()?;
        let hash_sibling_offset: u32 = reader.read_le()?;
        let name_size: u32 = reader.read_le()?;

        let mut name_bytes = vec![0u8; name_size as usize];
        reader.read_exact(&mut name_bytes)?;

        let name = String::from_utf8(name_bytes).map_err(|e| Error::InvalidData(e.to_string()))?;

        Ok(FileEntry {
            parent_offset,
            sibling_offset,
            data_offset,
            data_size,
            hash_sibling_offset,
            name_size,
            name,
        })
    }

    /// Find a directory by its path
    pub fn find_dir(&mut self, path: &str) -> Result<u32, Error> {
        let path_parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();

        let mut current_dir = Self::ROOT_DIR_OFFSET;
        for part in path_parts {
            match self.find_dir_in_parent(current_dir, part) {
                Ok(dir) => current_dir = dir,
                Err(e) => {
                    return Err(Error::NotFound(format!(
                        "Could not find directory '{}': {}",
                        part, e
                    )));
                }
            }
        }

        Ok(current_dir)
    }

    /// Find a directory within a parent directory by name
    fn find_dir_in_parent(&mut self, parent_offset: u32, name: &str) -> Result<u32, Error> {
        let hash = self.compute_hash(parent_offset, name.as_bytes(), self.dir_hash_table.len());

        let mut current_offset = self.dir_hash_table[hash as usize];
        while current_offset != Self::INVALID_ENTRY {
            let entry = self.read_dir_entry(current_offset)?;
            if entry.parent_offset == parent_offset && entry.name == name {
                return Ok(current_offset);
            }
            current_offset = entry.hash_sibling_offset;
        }

        Err(Error::NotFound("Directory not found".into()))
    }

    /// Find a file by its path
    pub fn get_file_by_path(&mut self, path: &str) -> Result<Option<FileEntry>, Error> {
        let mut path_buf = PathBuf::from(path);
        let file_name = path_buf
            .file_name()
            .ok_or_else(|| Error::InvalidData(format!("Invalid path: {}", path)))?
            .to_string_lossy()
            .to_string();

        path_buf.pop();
        let parent_path = path_buf.to_string_lossy().to_string();

        match self.find_dir(&parent_path) {
            Ok(parent_offset) => match self.find_file_in_dir(parent_offset, &file_name) {
                Ok(file) => Ok(Some(file)),
                Err(Error::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            },
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Find a file within a parent directory by name
    fn find_file_in_dir(&mut self, parent_offset: u32, name: &str) -> Result<FileEntry, Error> {
        let hash = self.compute_hash(parent_offset, name.as_bytes(), self.file_hash_table.len());

        let mut current_offset = self.file_hash_table[hash as usize];
        while current_offset != Self::INVALID_ENTRY {
            let entry = self.read_file_entry(current_offset)?;
            if entry.parent_offset == parent_offset && entry.name == name {
                return Ok(entry);
            }
            current_offset = entry.hash_sibling_offset;
        }

        Err(Error::NotFound("File not found".into()))
    }

    /// Check if a file exists by path
    pub fn file_exists(&mut self, path: &str) -> bool {
        self.get_file_by_path(path).is_ok()
    }

    /// Check if a directory exists by path
    pub fn dir_exists(&mut self, path: &str) -> bool {
        self.find_dir(path).is_ok()
    }

    // /// Get the size of a file by path
    // pub fn get_file_size(&mut self, path: &str) -> Result<u64, Error> {
    //     let file = self.get_file_by_path(path)?;
    //     Ok(file.data_size)
    // }
    /// Read a file from the RomFS to a Vec<u8>
    pub fn read_to_vec(&mut self, path: &str) -> Result<Option<Vec<u8>>, Error> {
        let file = self.get_file_by_path(path)?;

        match file {
            Some(file) => {
                tracing::info!("Extracting file: {} (size: {})", path, file.data_size);

                let offset = self.header.file_data_offset + file.data_offset;
                let size = file.data_size as usize;

                self.reader.seek(SeekFrom::Start(offset))?;

                // Read the file data in chunks to avoid excessive memory usage
                let mut data = Vec::with_capacity(size);
                let mut ofs = 0;
                let chunk_size = 0x800000; // 8MB chunks

                while ofs < size {
                    let sz = if size - ofs < chunk_size {
                        size - ofs
                    } else {
                        chunk_size
                    };

                    let mut buffer = vec![0u8; sz];
                    match self.reader.read_exact(&mut buffer) {
                        Ok(_) => data.extend_from_slice(&buffer),
                        Err(e) => return Err(Error::Io(e)),
                    }
                    ofs += sz;
                }

                tracing::info!("Extraction complete!");
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }

    /// Open a directory iterator for browsing directories and files
    pub fn open_dir(&mut self, path: &str) -> Result<RomFsDirectoryIterator<R>, Error>
    where
        Self: Clone,
        R: Clone,
    {
        let dir_offset = self.find_dir(path)?;
        let dir_entry = self.read_dir_entry(dir_offset)?;

        let mut dir_offsets = Vec::new();
        let mut current_child = dir_entry.child_dir_offset;

        while current_child != Self::INVALID_ENTRY {
            dir_offsets.push(current_child);
            let child_entry = self.read_dir_entry(current_child)?;
            current_child = child_entry.sibling_offset;
        }

        let mut file_offsets = Vec::new();
        let mut current_file = dir_entry.child_file_offset;

        while current_file != Self::INVALID_ENTRY {
            file_offsets.push(current_file);
            let file_entry = self.read_file_entry(current_file)?;
            current_file = file_entry.sibling_offset;
        }

        Ok(RomFsDirectoryIterator {
            romfs: Arc::new(Mutex::new(self.clone())),
            dir_offsets,
            file_offsets,
            current_dir_index: 0,
            current_file_index: 0,
        })
    }
}

impl<R: Read + Seek + Clone> RomFs<R> {
    /// Convert this RomFS to use a shared reader
    pub fn into_shared(self) -> Result<RomFs<SharedReader<R>>, Error> {
        Ok(RomFs {
            reader: SharedReader::new(self.reader),
            header: self.header,
            dir_hash_table: self.dir_hash_table,
            file_hash_table: self.file_hash_table,
            cache_dir_entries: self.cache_dir_entries,
            cache_file_entries: self.cache_file_entries,
        })
    }
}

impl<R: Read + Seek> RomFs<SharedReader<R>> {
    /// Create a new RomFS parser from a shared reader
    pub fn from_shared(reader: SharedReader<R>) -> Result<Self, Error> {
        Self::from_reader(reader)
    }
}

impl<R: Read + Seek + Clone> Clone for RomFs<R> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            header: self.header.clone(),
            dir_hash_table: self.dir_hash_table.clone(),
            file_hash_table: self.file_hash_table.clone(),
            cache_dir_entries: self.cache_dir_entries.clone(),
            cache_file_entries: self.cache_file_entries.clone(),
        }
    }
}

impl<R: Read + Seek + Clone> VirtualFSExt<R> for RomFs<R> {
    type Entry = FileEntry;

    fn list_files(&self) -> Result<Vec<Self::Entry>, Error> {
        // Call the base implementation directly using fully qualified syntax
        RomFs::list_files(&mut self.clone())
    }

    fn get_file(&self, name: &str) -> Result<Option<Self::Entry>, Error> {
        // We need to mutably borrow self, so we clone it
        RomFs::get_file_by_path(&mut self.clone(), name)
    }

    fn create_reader(&mut self, file: &Self::Entry) -> Result<SubFile<R>, Error> {
        let offset = self.header.file_data_offset + file.data_offset;
        Ok(SubFile::new(
            self.reader.clone(),
            offset,
            offset + file.data_size,
        ))
    }
}

impl<R: Read + Seek + Clone> FileEntryExt<R> for FileEntry {
    type FS = RomFs<R>;

    fn file_reader(&self, fs: &mut Self::FS) -> Result<SubFile<R>, Error> {
        fs.create_reader(self)
    }

    fn read_bytes(&self, fs: &mut Self::FS, size: usize) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0; size];
        let mut reader = self.file_reader(fs)?;
        reader.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn file_size(&self) -> u64 {
        self.data_size
    }

    fn file_name(&self) -> String {
        self.name.clone()
    }
}
