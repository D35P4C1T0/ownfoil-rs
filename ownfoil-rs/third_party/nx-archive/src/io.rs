use aes::Aes128;
use cipher::KeyIvInit;
use cipher::StreamCipher;
use std::io::{self, Read, Result, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

/// Function to align down to 16-byte boundary for AES operations
pub const fn align_down(value: u64, align: u64) -> u64 {
    let inv_mask = align - 1;
    value & !inv_mask
}

/// Function to align up to 16-byte boundary for AES operations
pub const fn align_up(value: usize, align: usize) -> usize {
    let inv_mask = align - 1;
    (value + inv_mask) & !inv_mask
}

/// Returns a tweak suitable for Nintendo crypto operations
///
/// The tweak is the sector index in big-endian.
pub fn get_nintendo_tweak(sector_index: u128) -> [u8; 0x10] {
    sector_index.to_be_bytes()
}

/// Trait that combines Read and Seek
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A shared reader that can be used by multiple consumers
pub struct SharedReader<R: Read + Seek> {
    inner: Arc<Mutex<R>>,
}

impl<R: Read + Seek> Clone for SharedReader<R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R: Read + Seek + Clone> SharedReader<R> {
    /// Create a new shared reader from a regular reader
    pub fn new(reader: R) -> Self {
        Self {
            inner: Arc::new(Mutex::new(reader)),
        }
    }

    /// Create a new shared reader from an existing reader
    pub fn from_reader(reader: R) -> Self {
        Self::new(reader)
    }

    /// Create a new shared reader from an existing shared reader
    pub fn from_shared(reader: SharedReader<R>) -> Self {
        reader
    }

    /// Create a SubFile from this shared reader
    pub fn sub_file(&self, start: u64, end: u64) -> SubFile<Self> {
        SubFile::new(self.clone(), start, end)
    }

    /// Create an AES-CTR reader from this shared reader
    pub fn aes_ctr_reader(
        &self,
        base_offset: u64,
        ctr: u64,
        key: Vec<u8>,
    ) -> Aes128CtrReader<Self> {
        Aes128CtrReader::new(self.clone(), base_offset, ctr, key)
    }
}

impl<R: Read + Seek> Read for SharedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.inner.lock().unwrap().read(buf)
    }
}

impl<R: Read + Seek> Seek for SharedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.lock().unwrap().seek(pos)
    }
}

/// Represents a sub-section of a file
#[derive(Debug)]
pub struct SubFile<R: Read + Seek> {
    reader: R,
    start: u64,
    end: u64,
    position: u64,
}

impl<R: Read + Seek> SubFile<R> {
    pub fn new(reader: R, start: u64, end: u64) -> Self {
        Self {
            reader,
            start,
            end,
            position: 0,
        }
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn size(&self) -> u64 {
        self.end - self.start
    }

    /// Convert this SubFile into one that uses a shared reader
    pub fn shared(self) -> SubFile<SharedReader<R>>
    where
        R: Clone,
    {
        SubFile::new(SharedReader::new(self.reader), self.start, self.end)
    }

    /// Create a new subfile from this one
    pub fn subfile(&self, relative_start: u64, relative_end: u64) -> SubFile<R>
    where
        R: Clone,
    {
        let absolute_start = self.start + relative_start;
        let absolute_end = self.start + relative_end;
        assert!(
            absolute_end <= self.end,
            "Subfile range exceeds parent bounds"
        );
        SubFile::new(self.reader.clone(), absolute_start, absolute_end)
    }
}

impl<R: Read + Seek> Read for SubFile<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.start == self.end || self.position >= self.end - self.start {
            return Ok(0);
        }

        self.reader
            .seek(SeekFrom::Start(self.start + self.position))?;

        let max_read =
            std::cmp::min(buf.len() as u64, (self.end - self.start) - self.position) as usize;
        let bytes_read: usize = self.reader.read(&mut buf[..max_read])?;

        self.position += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl<R: Read + Seek> Seek for SubFile<R> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => ((self.end - self.start) as i64 + offset) as u64,
            SeekFrom::Current(offset) => (self.position as i64 + offset) as u64,
        };

        if new_pos > self.end - self.start {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Cannot seek past end of subfile",
            ));
        }

        self.position = new_pos;
        Ok(self.position)
    }
}

/// AES-128-CTR reader that decrypts data as it's read
pub struct Aes128CtrReader<R: Read + Seek> {
    base_reader: R,
    base_offset: u64,
    offset: u64,
    ctr: u64,
    key: Vec<u8>,
}

impl<R: Read + Seek> Aes128CtrReader<R> {
    pub fn new(base_reader: R, base_offset: u64, ctr: u64, key: Vec<u8>) -> Self {
        // Important: Seek to the base_offset during initialization, just like CNTX does
        let mut reader = base_reader;
        let _ = reader.seek(SeekFrom::Start(base_offset));

        Self {
            base_reader: reader,
            base_offset,
            offset: base_offset,
            ctr,
            key,
        }
    }
}

impl<R: Read + Seek> Read for Aes128CtrReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Get current position exactly like CNTX does
        let offset = self.base_reader.stream_position()?;

        // Align the offset to 16-byte boundary for AES
        let aligned_offset = align_down(offset, 0x10);
        let diff = (offset - aligned_offset) as i64;

        // Calculate size needed for aligned read
        let read_buf_size_raw = buf.len() + diff as usize;
        let read_buf_size = align_up(read_buf_size_raw, 0x10);
        let read_buf_size_diff = (read_buf_size - read_buf_size_raw) as i64;

        // Prepare buffer for aligned read
        let mut read_buf = vec![0u8; read_buf_size];

        // Seek to aligned position and handle errors exactly as CNTX does
        self.seek(SeekFrom::Current(-diff))?;

        // Read data
        let read_size = self.base_reader.read(&mut read_buf)? as i64;

        // Re-seek to maintain correct position
        self.seek(SeekFrom::Current(read_size - read_buf_size_diff))?;

        // Calculate IV using Nintendo's approach: (aligned_offset >> 4) | (ctr << 64)
        let iv = get_nintendo_tweak(((aligned_offset as u128) >> 4) | ((self.ctr as u128) << 64));

        // Use the same exact AES-CTR implementation as CNTX
        // use cipher::{NewCipher, StreamCipher};

        // Create cipher using KeyIvInit and from_core
        let key_array: &[u8; 16] = self
            .key
            .as_slice()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Invalid key length"))?;
        let mut ctr = ctr::Ctr128BE::<Aes128>::new(key_array.into(), (&iv).into());

        // Apply keystream for decryption in CTR mode
        ctr.apply_keystream(&mut read_buf);

        // Copy the relevant portion to the output buffer
        let read_buf_start = diff as usize;
        let read_buf_end = read_buf_start + buf.len();
        buf.copy_from_slice(&read_buf[read_buf_start..read_buf_end]);

        Ok(buf.len())
    }
}

impl<R: Read + Seek> Seek for Aes128CtrReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Current(cur_pos) => {
                let new_offset = self.offset as i64 + cur_pos;
                self.offset = new_offset as u64;
            }
            SeekFrom::Start(start_pos) => self.offset = self.base_offset + start_pos,
            SeekFrom::End(end_pos) => {
                let new_offset = self.offset as i64 + end_pos;
                self.offset = new_offset as u64;
            }
        }

        self.base_reader.seek(SeekFrom::Start(self.offset))
    }
}

/// Extension trait for readers to easily convert them to shared readers
pub trait ReaderExt: Read + Seek + Clone + Sized {
    /// Convert this reader into a shared reader
    fn into_shared(self) -> SharedReader<Self> {
        SharedReader::new(self)
    }
}

impl<R: Read + Seek + Clone> ReaderExt for R {}

#[cfg(test)]
mod tests {
    use super::*;
    use ctr::Ctr128BE;
    use std::io::{Cursor, Read};
    #[test]
    fn test_aes128_ctr_reader() {
        let test_data = b"0123456789ABCDEF0123456789ABCDEF";
        let key = vec![
            0x13, 0x37, 0x13, 0x37, 0x13, 0x37, 0x13, 0x37, 0x13, 0x37, 0x13, 0x37, 0x13, 0x37,
            0x13, 0x37,
        ];

        // First encrypt the data
        let iv = [0u8; 0x10]; // Nintendo tweak for sector 0
        let key_array: &[u8; 16] = key.as_slice().try_into().unwrap();
        let mut cipher = Ctr128BE::<Aes128>::new(key_array.into(), &iv.into());
        let mut encrypted = test_data.to_vec();
        cipher.apply_keystream(&mut encrypted);

        println!("Encrypted: {:?}", encrypted);

        // Now test decryption using Aes128CtrReader
        let cursor = Cursor::new(encrypted);
        let shared = SharedReader::new(cursor);
        let mut aes_reader = shared.aes_ctr_reader(0, 0, key);

        let mut buf = vec![0u8; 16];
        aes_reader.read_exact(&mut buf).unwrap();

        println!("Decrypted: {}", String::from_utf8_lossy(&buf));
        assert_eq!(&buf, &test_data[..16]);
    }
}
