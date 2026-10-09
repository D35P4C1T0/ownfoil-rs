//! Module for handling Nintendo Switch Content Meta (CNMT) files
//!
//! Content Meta files are part of the Nintendo Switch file system and contain metadata
//! about digital titles, including information about the title itself and its content files.
//!
//! This module provides structures and functionality to parse and interact with CNMT files:
//!
//! # Main Components:
//!
//! * `CnmtHeader`: The header structure containing basic title information
//! * `ContentInfo`: Information about individual content files
//! * `ContentEntry`: Content entries with hash information
//! * `ContentMetaEntry`: Entries for dependent content
//! * `Cnmt`: The main structure that contains all parsed CNMT data
//!
//! # Content Types:
//!
//! CNMT files can represent different types of content:
//! - Applications (games and software)
//! - Patches/Updates
//! - Add-on Content (DLC)
//! - System Updates
//! - Delta fragments
//! - Data patches (15.0.0+)
mod enums;
mod extended_header;
use binrw::prelude::*;
pub use enums::*;
pub use extended_header::*;
use std::io::{Read, Seek};

/// Content Meta header structure
#[derive(Debug, Clone)]
#[binrw]
#[brw(little)]

pub struct CnmtHeader {
    /// Title ID
    pub title_id: u64,
    /// Title version
    pub title_version: u32,
    /// Content meta type
    pub meta_type: ContentMetaType,
    /// Platform for the content meta
    pub meta_platform: ContentMetaPlatform,
    /// Extended header size
    // note: in NSTools this is called `headerOffset`
    pub extended_header_size: u16,
    /// Number of content entries
    pub total_content_entries: u16,
    /// Number of meta entries
    pub total_content_meta_entries: u16,
    /// Content meta attributes
    pub attributes: u8,
    // --- Start of unknown fields - Undocumented in switchbrew but assumed to be correct
    // (0x3 of reserved fields)
    /// Storage ID?
    pub storage_id: u8,
    /// Content Install type?
    pub content_install_type: u8,
    /// Reserved field
    pub _reserved: u8,
    // --- End of unknown fields
    /// Required system version for download
    pub required_dl_system_version: u32,
    pub _reserved2: u32,
}

/// Content info structure containing details about content files
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct PackagedContentInfo {
    /// Content ID (usually a hash or identifier)
    pub content_id: [u8; 16],
    /// Size of the content in bytes (stored as a 48-bit value)
    #[br(map = |bytes: [u8; 6]| u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0]))]
    #[bw(map = |&size: &u64| [size as u8, (size >> 8) as u8, (size >> 16) as u8, (size >> 24) as u8, (size >> 32) as u8, (size >> 40) as u8])]
    pub size: u64,
    /// Content type
    pub content_type: PackagedContentType,
    /// ID offset
    pub id_offset: u8,
}

/// Content entry with hash and info
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct PackagedContent {
    /// SHA-256 hash of the content
    pub hash: [u8; 32],
    /// Content info fields (without the hash)
    pub info: PackagedContentInfo,
}

/// Content type enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[binrw]
#[brw(little, repr = u8)]
#[repr(u8)]
pub enum PackagedContentType {
    /// Meta content
    Meta = 0,
    /// Program content
    Program = 1,
    /// Data content
    Data = 2,
    /// Control content
    Control = 3,
    /// HTML document content
    HtmlDocument = 4,
    /// Legal information content
    LegalInformation = 5,
    /// Delta fragment content
    DeltaFragment = 6,
}

/// Content meta entry for dependent content
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ContentMetaEntry {
    /// Title ID
    pub title_id: u64,
    /// Version
    pub version: u32,
    /// Meta type
    pub meta_type: u8,
    /// Attributes
    pub attributes: u8,
    /// Reserved field
    #[br(pad_after = 2)]
    pub _reserved: (),
}

/// Main CNMT structure containing all parsed data
///
/// This structure contains the CNMT header, extended header, content entries, and meta entries.
/// The extended header is parsed based on the meta type of the CNMT file.
#[derive(Debug, Clone)]
pub struct Cnmt {
    /// CNMT header
    pub header: CnmtHeader,
    /// Extended header based on meta_type
    pub extended_header: ExtendedHeader,
    /// Content entries
    pub content_entries: Vec<PackagedContent>,
    /// Meta entries
    pub meta_entries: Vec<ContentMetaEntry>,
}

impl Cnmt {
    /// Parse a CNMT file from a reader
    pub fn from_reader<R: Read + Seek>(reader: &mut R) -> BinResult<Self> {
        // Read the header
        let header: CnmtHeader = reader.read_le()?;

        // Read extended header based on meta type
        let extended_header = match header.meta_type {
            ContentMetaType::Application => {
                let app_header: ApplicationMetaExtendedHeader = reader.read_le()?;
                ExtendedHeader::Application(app_header)
            }
            ContentMetaType::Patch => {
                let patch_header: PatchMetaExtendedHeader = reader.read_le()?;
                ExtendedHeader::Patch(patch_header)
            }
            ContentMetaType::AddOnContent => {
                let addon_header: AddonContentMetaExtendedHeader = reader.read_le()?;
                ExtendedHeader::Addon(addon_header)
            }
            ContentMetaType::Delta => {
                let delta_header: DeltaMetaExtendedHeader = reader.read_le()?;
                ExtendedHeader::Delta(delta_header)
            }
            ContentMetaType::SystemUpdate => {
                let sys_header: SystemUpdateMetaExtendedHeader = reader.read_le()?;
                ExtendedHeader::SystemUpdate(sys_header)
            }
            _ => {
                // Read unknown extended header
                let mut unknown_data = vec![0; header.extended_header_size as usize];
                reader.read_exact(&mut unknown_data)?;
                ExtendedHeader::Unknown(unknown_data)
            }
        };

        // Position reader at the start of the content entries
        // (Header size is 0x20 bytes plus the extended header size)
        let content_start_pos =
            std::mem::size_of::<CnmtHeader>() as u64 + header.extended_header_size as u64;
        reader.seek(std::io::SeekFrom::Start(content_start_pos))?;

        // Read content entries
        let mut content_entries = Vec::with_capacity(header.total_content_entries as usize);
        for _ in 0..header.total_content_entries {
            let entry: PackagedContent = reader.read_le()?;
            content_entries.push(entry);
        }

        // Read meta entries
        let mut meta_entries = Vec::with_capacity(header.total_content_meta_entries as usize);
        for _ in 0..header.total_content_meta_entries {
            let entry: ContentMetaEntry = reader.read_le()?;
            meta_entries.push(entry);
        }

        Ok(Cnmt {
            header,
            extended_header,
            content_entries,
            meta_entries,
        })
    }

    pub fn get_title_id_string(&self) -> String {
        hex::encode(self.header.title_id.to_be_bytes()).to_uppercase()
    }

    /// Get content entry by its type
    pub fn get_content_entry_by_type(
        &self,
        content_type: PackagedContentType,
    ) -> Option<&PackagedContent> {
        self.content_entries
            .iter()
            .find(|entry| entry.info.content_type == content_type)
    }

    /// Get the content ID of the main program
    pub fn get_main_content_id(&self) -> Option<[u8; 16]> {
        self.get_content_entry_by_type(PackagedContentType::Program)
            .map(|entry| entry.info.content_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::BufReader;
    use std::io::Cursor;
    use std::path::Path;

    #[test]
    fn test_parse_cnmt_header() {
        let test_data = [
            // Header data
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, // title_id (u64)
            0x12, 0x34, 0x56, 0x78, // title_version (u32)
            0x80, // meta_type (u8) - APPLICATION
            0x00, // meta_platform (u8) - NX
            0x10, 0x00, // extended_header_size (u16)
            0x02, 0x00, // total_content_entries (u16)
            0x01, 0x00, // total_content_meta_entries (u16)
            0x01, // attributes (u8)
            0x02, // storage_id (u8)
            0x03, // content_install_type (u8)
            0x04, // _reserved (u8)
            0x11, 0x22, 0x33, 0x44, // required_dl_system_version (u32)
            0x55, 0x66, 0x77, 0x88, // _reserved2 (u32)
        ];

        let mut cursor = Cursor::new(test_data);
        let header: CnmtHeader = cursor.read_le().unwrap();

        assert_eq!(header.title_id, 0xEFCDAB8967452301);
        assert_eq!(header.title_version, 0x78563412);
        assert_eq!(header.meta_type, ContentMetaType::Application);
        assert_eq!(header.meta_platform, ContentMetaPlatform::NX);
        assert_eq!(header.extended_header_size, 0x10);
        assert_eq!(header.total_content_entries, 0x02);
        assert_eq!(header.total_content_meta_entries, 0x01);
        assert_eq!(header.attributes, 0x01);
        assert_eq!(header.storage_id, 0x02);
        assert_eq!(header.content_install_type, 0x03);
        assert_eq!(header._reserved, 0x04);
        assert_eq!(header.required_dl_system_version, 0x44332211);
        assert_eq!(header._reserved2, 0x88776655);
    }

    #[test]
    fn test_content_entry() {
        let test_data = [
            // Hash (32 bytes)
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C,
            0x1D, 0x1E, 0x1F, 0x20, // Content ID (16 bytes)
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E,
            0x2F, 0x30, // Size (6 bytes)
            0x31, 0x32, 0x33, 0x34, 0x35, 0x36, // Content type (1 byte)
            0x01, // ID offset (1 byte)
            0x42,
        ];

        let mut cursor = Cursor::new(test_data);
        let entry: PackagedContent = cursor.read_le().unwrap();

        // Test hash
        let expected_hash = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C,
            0x1D, 0x1E, 0x1F, 0x20,
        ];
        assert_eq!(entry.hash, expected_hash);

        // Test content ID
        let expected_content_id = [
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E,
            0x2F, 0x30,
        ];
        assert_eq!(entry.info.content_id, expected_content_id);

        // Test size (6 bytes)
        let expected_size = 0x363534333231; // Little-endian representation of the 6 bytes
        assert_eq!(entry.info.size, expected_size);

        // Test content type and id offset
        assert_eq!(entry.info.content_type, PackagedContentType::Program); // 0x01 corresponds to Program
        assert_eq!(entry.info.id_offset, 0x42);
    }

    #[test]
    fn test_application_cnmt() {
        let path = Path::new("test/Browser-cnmt/Application_0100c4c320c0ffee.cnmt");

        // Skip test if file doesn't exist (to avoid CI failure)
        if !path.exists() {
            println!("Skipping test_application_cnmt: test file not found");
            return;
        }

        let file = File::open(path).expect("Failed to open test file");
        let mut reader = BufReader::new(file);
        let cnmt = Cnmt::from_reader(&mut reader).expect("Failed to parse CNMT");

        // Test header fields
        println!(
            "Title ID: {:016X} (Serialized as {})",
            cnmt.header.title_id,
            cnmt.get_title_id_string()
        );
        println!("Title Version: {}", cnmt.header.title_version);
        println!("Meta Type: {:?}", cnmt.header.meta_type);
        println!("Extended Header Size: {}", cnmt.header.extended_header_size);
        println!("Extended header: {:?}", cnmt.extended_header);
        println!("Content Count: {}", cnmt.header.total_content_entries);
        println!(
            "Content Meta Count: {}",
            cnmt.header.total_content_meta_entries
        );
        println!("Attributes: {}", cnmt.header.attributes);
        println!("Storage ID: {}", cnmt.header.storage_id);
        println!("Content Install Type: {}", cnmt.header.content_install_type);
        assert_eq!(cnmt.header.title_id, 0x0100c4c320c0ffee);

        for entry in cnmt.content_entries.iter() {
            // assert!(entry.info.size > 0, "Content size should be greater than 0");
            println!("Content ID: {:02X?}", entry.info.content_id);
            println!("Content Size: {}", entry.info.size);
            println!("Content Type: {:?}", entry.info.content_type);
            // println!("ID Offset: {}", entry.info.id_offset);
        }
    }
}
