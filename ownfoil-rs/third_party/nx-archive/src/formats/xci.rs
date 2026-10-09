//! The XCI (eXtendable Card Image) format is a Nintendo Switch game card image format.
//!
//! This format is the raw data from a Nintendo Switch game card, those little bitter
//! SD card-like things that you insert physically on top of the console.
//!
//! The cards themselves physically have a similar footprint to an SD card, but are slightly thicker
//! and less wide. The data inside is stored in an eMMC chip, which is soldered onto the card.
//! They are also coated with Denatonium Benzoate, a bittering agent, to discourage consumption.
//! Just in case you (or your pet, or your little sibling) get any ideas. No, the game cards taste horrible (by design).

// note to self: XCI files are massive, so we need to be veery careful with memory usage.
// maybe consider using a buffered reader to read the file in chunks?
// or, mmap the file and read it in chunks that way?
use binrw::prelude::*;
use std::io::{Read, Seek, SeekFrom};
use tracing::trace;

use crate::{FileEntryExt, TitleDataExt, VirtualFSExt, error::Error, io::SubFile};

use super::hfs0::Hfs0;

#[binrw]
#[derive(Debug)]
#[brw(little, repr = u8)]
/// Size of the eMMC chip on the game card,
/// base 2 logarithm of the size in gigabytes.
pub enum RomSize {
    Size1Gb = 0xFA,
    Size2Gb = 0xF8,
    Size4Gb = 0xF0,
    Size8Gb = 0xE0,
    Size16Gb = 0xE1,
    Size32Gb = 0xE2,
}

#[binrw]
#[derive(Debug)]
#[brw(little, repr = u8)]
/// Flags for the Game card
/// These are used to determine extra features of the game card.
pub enum GameCardFlags {
    /// Boots to this game card's title automatically
    /// on startup
    AutoBoot = 0,
    /// Don't ever record this card ever being launched
    HistoryErase = 1,
    /// This card is a service card,
    /// used for debugging and repairs by
    /// Nintendo
    RepairTool = 2,
    /// This card is Chinese region-locked,
    /// and can only be played on Chinese Switches
    /// (Terra region)
    DifferentRegionCupToTerraDevice = 3,
    /// This card is region-free, and can be played
    /// globally on any Switch. Used for worldwide
    /// game releases that contain multiple Asian locales.
    /// i.e Southeast Asian releases
    ///
    /// These releases are usually labeled as "Asia" or "Multi-Language"
    DifferentRegionCupToGlobalDevice = 4,
    /// This card has a next-generation header format, used
    /// for later Switch games
    HasNewCardHeader = 7,
    HasNewCardHeaderV2 = 8,
}

#[binrw]
#[derive(Debug)]
#[brw(little, repr = u64)]
/// Firmware version for game card
pub enum FirmwareVersion {
    Development = 0,
    Retail1_0_0 = 1,
    Retail4_0_0 = 2,
    Devel11_0_0 = 3,
    Retail11_0_0 = 4,
    Retail12_0_0 = 5,
}

#[binrw]
#[derive(Debug)]
#[brw(little, repr = u32)]
/// Access control flags for game card
pub enum AccessControlFlags {
    /// No access control
    TwentyFiveMhz = 0x00A10011,
    /// 50mhz
    FiftyMhz = 0x00A10012,
}

#[binrw]
#[derive(Debug)]
#[brw(little, repr = u8)]
/// Compatibility type for game card
pub enum CompatibilityType {
    /// Normal game card
    Normal = 0,
    /// Chinese game card, compatible
    /// with Chinese Switches
    Terra = 1,
}

/// Size of a media sector
pub const MEDIA_SIZE: u64 = 0x200;

/// XCI Header structure
#[derive(Debug, BinRead, BinWrite)]
#[br(little)]
pub struct XciHeader {
    /// RSA-2048 signature over the header
    pub signature: [u8; 0x100],
    #[brw(magic = b"HEAD")]
    /// `RomAreaStartPageAddress`
    /// (in Gamecard page units, which are 0x200 bytes)
    pub rom_area_offset: u32,
    /// Offset to backup partition
    /// (Should always be 0xFFFFFFFF)
    pub backup_area_offset: u32,
    /// Index for title key encryption key
    /// TitleKeyDecIndex (high nibble) and KekIndex (low nibble)
    pub title_kek_index: u8,
    /// Size of the gamecard (0-7)
    pub rom_size: RomSize,
    /// Version of the gamecard header
    pub gamecard_header_version: u8,
    /// Flags for the gamecard
    pub gamecard_flags: GameCardFlags,
    /// Unique identifier for the game package
    pub package_id: u64,
    /// End offset of the valid data
    /// (in Gamecard page units, which are 0x200 bytes)
    pub valid_data_end_address: u32,
    /// Additional gamecard info
    _reserved: u8,
    /// Flags2
    pub gamecard_flags2: u8,
    /// ApplicationIdListEntryCount
    pub application_id_list_entry_count: u16,
    /// Reversed IV
    pub reversed_iv: [u8; 0x10],
    /// PartitionFS header address
    pub hfs0_offset: u64,
    /// Size of the HFS0 header
    pub hfs0_header_size: u64,
    // HFS0 header hash
    pub hfs0_header_hash: [u8; 0x20],
    /// Hash of the InitialData
    pub initial_data_hash: [u8; 0x20],
    /// SelSec
    pub sel_sec: u32,
    /// SelT1Key
    pub sel_t1_key: u32,
    /// SelKey
    pub sel_key: u32,
    /// LimArea
    /// (in Gamecard page units, which are 0x200 bytes)
    pub lim_area: u32,
    /// CardHeaderEncrypted data,
    /// encrypted with AES-128-CBC
    ///
    /// We will not parse this field, as it is encrypted
    pub card_header_encrypted: [u8; 0x70],
}

/// Gamecard Information structure
#[derive(Debug, BinRead, BinWrite)]
#[br(little)]
pub struct CardHeaderEncryptedData {
    /// Firmware version
    pub firmware_version: FirmwareVersion,
    /// Access control flags
    pub access_control_flags: AccessControlFlags,
    /// Read wait time
    ///
    /// Most of the time this is always
    /// 0x1388
    pub read_wait_time: u32,
    /// Read wait time 2
    ///
    /// Most of the time this is always 0
    pub read_wait_time2: u32,
    /// Write wait time
    ///
    /// Most of the time this is always 0
    pub write_wait_time: u32,
    /// Write wait time 2
    ///
    /// Most of the time this is always 0
    pub write_wait_time2: u32,
    /// Firmware mode
    ///
    /// (The SDK Addon version for this game)
    pub firmware_mode: u32,
    /// Minimal version for this game?
    pub update_partition_version: u32,
    /// Reserved
    pub _reserved1: [u8; 0x3],
    /// Hash of the update partition
    pub update_partition_hash: u64,
    /// Update partition ID
    ///
    /// This should always be 0x0100000000000816
    pub update_partition_id: u64,
    /// Empty field 2
    pub empty2: [u8; 0x38],
}

/// Gamecard Certificate structure
#[binrw]
#[derive(Debug)]
#[br(little)]
pub struct GamecardCertificate {
    /// RSA-2048 signature
    pub signature: [u8; 0x100],
    #[brw(magic = b"CERT")]
    /// Unknown data 1
    pub unknown1: [u8; 0x10],
    /// Unknown data 2
    pub unknown2: [u8; 0xA],
    /// Certificate data
    pub data: [u8; 0xD6],
}

/// XCI file representation
pub struct Xci<R: Read + Seek> {
    /// Reader for the XCI file
    reader: R,
    /// XCI header
    pub header: XciHeader,
    /// Optional key area for "full" XCI files
    pub key_area: Option<Vec<u8>>,
    /// Gamecard info
    // pub gamecard_info: CardHeaderEncryptedData,
    /// Gamecard certificate
    pub gamecard_cert: Option<GamecardCertificate>,
}

/// Key Area found in "full" XCI files
#[derive(Debug)]
pub struct KeyArea {
    /// Package ID (same as in header)
    pub package_id: u64,
    /// Challenge Response Auth Data
    pub challenge_response_auth_data: [u8; 0x10],
    /// Challenge Response Auth MAC
    pub challenge_response_auth_mac: [u8; 0x10],
    /// Challenge Response Auth Nonce
    pub challenge_response_auth_nonce: [u8; 0x10],
    /// Title key 1
    pub title_key1: [u8; 0x8],
    /// Title key 2
    pub title_key2: [u8; 0x8],
}

/// A partition entry in an XCI file
#[derive(Debug, Clone)]
pub struct XciPartition {
    pub name: String,
    pub offset: u64,
    pub size: u64,
}

impl<R: Read + Seek + Clone> VirtualFSExt<R> for Xci<R> {
    type Entry = XciPartition;

    fn list_files(&self) -> Result<Vec<Self::Entry>, Error> {
        // We need to list the HFS0 partitions, but we can't borrow self as mutable
        // So we'll clone the reader and create a new instance
        let mut reader = self.reader.clone();
        let hfs0_offset = self.get_hfs0_offset();
        reader.seek(SeekFrom::Start(hfs0_offset)).ok();

        // Create a SubFile that covers the entire rest of the XCI file
        let subfile = SubFile::new(
            &mut reader,
            hfs0_offset,
            hfs0_offset + (self.header.valid_data_end_address as u64 * MEDIA_SIZE),
        );

        let hfs0 = Hfs0::from_reader(subfile)?;
        Ok(hfs0
            .list_files()?
            .into_iter()
            .map(|file| XciPartition {
                name: file.name,
                offset: hfs0_offset + file.offset,
                size: file.size,
            })
            .collect())
    }

    fn get_file(&self, name: &str) -> Result<Option<Self::Entry>, Error> {
        let files = self.list_files()?;
        Ok(files.into_iter().find(|f| f.name == name))
    }

    fn create_reader(&mut self, file: &Self::Entry) -> Result<SubFile<R>, Error> {
        Ok(SubFile::new(
            self.reader.clone(),
            file.offset,
            file.offset + file.size,
        ))
    }
}

impl<R: Read + Seek + Clone> FileEntryExt<R> for XciPartition {
    type FS = Xci<R>;

    fn file_reader(&self, fs: &mut Xci<R>) -> Result<SubFile<R>, Error> {
        fs.create_reader(self)
    }

    fn file_size(&self) -> u64 {
        self.size
    }

    fn file_name(&self) -> String {
        self.name.clone()
    }

    fn read_bytes(&self, fs: &mut Xci<R>, size: usize) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0; size];
        let mut reader = self.file_reader(fs)?;
        reader.read_exact(&mut buf).map_err(Error::from)?;
        Ok(buf)
    }
}

impl<R: Read + Seek> Xci<R> {
    /// Creates a new XCI instance from a reader
    pub fn new(mut reader: R) -> Result<Self, Error> {
        // Check if this is a "full" XCI
        // (full XCIs contain a 0x1000 key area, usually)
        // unreadable directly in most r/w operations
        let is_full_xci = {
            trace!("Checking if this is a full XCI");
            // Read at 0x100 first, we're checking for a trimmed XCI
            // trimmed XCIs will have a header at 0x100
            reader.seek(SeekFrom::Start(0x100)).map_err(Error::from)?;
            let mut magic = [0u8; 4];
            reader.read_exact(&mut magic).map_err(Error::from)?;
            let at_0x100 = b"HEAD"[..] == magic[..];
            trace!("Found magic at 0x100?: {}", at_0x100);

            if at_0x100 {
                trace!("This is a trimmed XCI, there's no key area");
                !at_0x100
            } else {
                // Let's go to 0x200
                // full XCIs will have a 0x1000 key area before the XciHeader
                reader.seek(SeekFrom::Start(0x1100)).map_err(Error::from)?;
                reader.read_exact(&mut magic).map_err(Error::from)?;
                let at_0x1100 = b"HEAD"[..] == magic[..];
                trace!("Found magic at 0x1100?: {}", at_0x1100);
                at_0x1100
            }
        };

        // Set up the key area if this is a full XCI
        let key_area = if is_full_xci {
            let mut key_data = vec![0u8; 0x1000];
            reader.seek(SeekFrom::Start(0)).map_err(Error::from)?;
            reader.read_exact(&mut key_data).map_err(Error::from)?;
            Some(key_data)
        } else {
            None
        };

        trace!("full XCI? {}", is_full_xci);

        // Determine header offset
        let header_offset = if is_full_xci { 0x100 } else { 0 };

        let mut magic_area = [0u8; 4];
        reader
            .seek(SeekFrom::Start(header_offset))
            .map_err(Error::from)?;
        reader.read_exact(&mut magic_area).map_err(Error::from)?;
        reader
            .seek(SeekFrom::Start(header_offset))
            .map_err(Error::from)?;

        // Read the header
        reader
            .seek(SeekFrom::Start(header_offset))
            .map_err(Error::from)?;
        let header: XciHeader = reader.read_le().map_err(Error::from)?;

        // Read gamecard certificate
        // Full XCIs would have this offset at 0x8000,
        // so trimmed XCIs would have this at 0x7000
        reader
            .seek(SeekFrom::Start(header_offset + 0x7000))
            .map_err(Error::from)?;
        // Attempt to read the gamecard certificate but don't fail if it doesn't exist
        let gamecard_cert = match reader.read_le::<GamecardCertificate>() {
            Ok(cert) => Some(cert),
            Err(err) => {
                tracing::warn!("Failed to read gamecard certificate: {}", err);
                None
            }
        };

        Ok(Xci {
            reader,
            header,
            key_area,
            gamecard_cert,
        })
    }

    /// Gets the offset to the HFS0 partition
    pub fn get_hfs0_offset(&self) -> u64 {
        if self.key_area.is_some() {
            0x1000 + self.header.hfs0_offset
        } else {
            self.header.hfs0_offset
        }
    }

    /// Reads the initial HFS0 header on the XCI file, returning a list of partitions found
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn list_hfs0_partitions(&mut self) -> Result<Hfs0<SubFile<&mut R>>, Error> {
        let hfs0_offset = self.get_hfs0_offset();
        self.reader
            .seek(SeekFrom::Start(hfs0_offset))
            .map_err(Error::from)?;

        // Create a SubFile that covers the entire rest of the XCI file
        // This allows reading the HFS0 header and all partition data
        let subfile = SubFile::new(
            &mut self.reader,
            hfs0_offset,
            // Don't limit to just header_size - we need access to the full content
            hfs0_offset + (self.header.valid_data_end_address as u64 * MEDIA_SIZE),
        );
        let hfs0 = Hfs0::from_reader(subfile)
            .map_err(|e| Error::Other(format!("Failed to parse HFS0: {}", e)))?;

        trace!("HFS0 header read successfully");

        Ok(hfs0)
    }

    /// Opens an HFS0 partition by name, returning the partition if it exists
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn open_hfs0_partition(
        &mut self,
        part_name: &str,
    ) -> Result<Option<Hfs0<SubFile<&mut R>>>, Error> {
        let hfs0_header = self.list_hfs0_partitions()?;

        // trace!("Attempting to open HFS0 partition: {}", part_name);
        let part = hfs0_header.get_file(part_name);

        if let Ok(Some(file)) = part {
            trace!("Attempting to open HFS0 partition: {}", part_name);
            // Calculate the absolute offset in the file
            let hfs0_offset = self.get_hfs0_offset();
            let start_offset = hfs0_offset + file.offset;
            let end_offset = start_offset + file.size;

            let part = Hfs0::from_reader(SubFile::new(&mut self.reader, start_offset, end_offset))
                .map_err(|e| {
                    Error::Other(format!(
                        "Failed to parse HFS0 partition {}: {}",
                        part_name, e
                    ))
                })?;

            trace!("HFS0 partition opened successfully");

            return Ok(Some(part));
        }

        Ok(None)
    }

    /// Opens the `secure` partition if it exists
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn open_secure_partition(&mut self) -> Result<Option<Hfs0<SubFile<&mut R>>>, Error> {
        self.open_hfs0_partition("secure")
    }

    /// Opens the `normal` partition if it exists
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn open_normal_partition(&mut self) -> Result<Option<Hfs0<SubFile<&mut R>>>, Error> {
        self.open_hfs0_partition("normal")
    }

    /// Opens the `logo` partition if it exists
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn open_logo_partition(&mut self) -> Result<Option<Hfs0<SubFile<&mut R>>>, Error> {
        self.open_hfs0_partition("logo")
    }

    #[tracing::instrument(skip(self), level = "trace")]
    pub fn open_update_partition(&mut self) -> Result<Option<Hfs0<SubFile<&mut R>>>, Error> {
        self.open_hfs0_partition("update")
    }
}

impl<R: Read + Seek> TitleDataExt for Xci<R> {
    fn get_cnmts(
        &mut self,
        keyset: &crate::formats::Keyset,
        title_keyset: std::option::Option<&crate::formats::title_keyset::TitleKeys>,
    ) -> Result<Vec<crate::formats::cnmt::Cnmt>, Error> {
        let mut cnmts = Vec::new();

        let secure = self.open_secure_partition()?;
        if let Some(mut secure) = secure {
            let files = secure.list_files()?;
            for file in files {
                if file.name.ends_with(".cnmt.nca") {
                    let mut buf = vec![0u8; file.size as usize];
                    secure.read_buf(&file, &mut buf)?;
                    let mut cursor = std::io::Cursor::new(buf);
                    let mut nca =
                        crate::formats::nca::Nca::from_reader(&mut cursor, keyset, title_keyset)?;
                    let mut pfs0 = nca.open_pfs0_filesystem(0)?;
                    // for each file
                    let files = pfs0.list_files()?;
                    for file in files {
                        if file.name.ends_with(".cnmt") {
                            let file = pfs0.read_to_vec(&file)?;
                            let mut cursor = std::io::Cursor::new(file);
                            let cnmt = crate::formats::cnmt::Cnmt::from_reader(&mut cursor)?;
                            cnmts.push(cnmt);
                        }
                    }
                } else if file.name.ends_with(".cnmt") {
                    let file = secure
                        .get_file(&file.name)?
                        .ok_or(Error::NotFound(file.name.clone()))?;
                    let mut buf = vec![0u8; file.size as usize];
                    secure.read_buf(&file, &mut buf)?;
                    let mut cursor = std::io::Cursor::new(buf);
                    let cnmt = crate::formats::cnmt::Cnmt::from_reader(&mut cursor)?;
                    cnmts.push(cnmt);
                }
            }
        }

        Ok(cnmts)
    }

    fn title_id(&self) -> Result<u64, Error> {
        Ok(self.header.package_id)
    }
}

#[cfg(test)]
mod tests {
    use tracing_test::traced_test;

    use crate::formats::Keyset;

    use super::*;
    use std::fs::File;

    #[test]
    #[traced_test]
    fn test_xci() {
        // test file from Ace Attorney Investigations Collection
        // thank you Edgeworth for being my guinea pig
        let file = File::open("dump/aaic.xci").unwrap();
        let mut xci = Xci::new(file).unwrap();

        println!("{:#?}", xci.header);
        trace!("HFS Header Offset {:02X?}", xci.header.hfs0_offset);
        // println!("{:#?}", xci.gamecard_info);
        println!("{:?}", xci.gamecard_cert);
        let keyset = Keyset::from_file("prod.keys").unwrap();
        let title_keyset = None;

        let cnmts = xci.get_cnmts(&keyset, title_keyset).unwrap();
        println!("{:#?}", cnmts);
    }
}
