use binrw::prelude::*;

#[binrw]
#[brw(little)]
#[derive(Debug, Default)]
pub struct RSASignature {
    // #[brw(count = 8)]
    pub signature: [[u8; 0x20]; 8],
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The source of the content the NCA is for, either downloaded from
/// the CDN or from a game card (cartridge).
pub enum DistributionType {
    /// The content was downloaded from a CDN, such as the eShop.
    Download = 0x00,
    /// The content is from a game card (cartridge).
    GameCard = 0x01,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The type of content stored in the NCA.
pub enum ContentType {
    /// Program content (executable code).
    Program = 0x00,
    /// Meta content (information about the title).
    Meta = 0x01,
    /// Control content (icon, screenshots, etc.).
    Control = 0x02,
    /// Manual content (digital manual/documentation).
    Manual = 0x03,
    /// Data content (general game data).
    Data = 0x04,
    /// Public data content.
    PublicData = 0x05,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The key generation used for the NCA.
pub enum KeyGeneration {
    /// 1.0.0 key generation
    Gen1_0_0 = 0x00,
    /// Unknown key generation (presumably planned for Horizon 2.0.0 but never used)
    Unused = 0x01,
    /// 3.0.0 key generation
    Gen3_0_0 = 0x02,
    /// 3.0.1 key generation
    Gen3_0_1 = 0x03,
    /// 4.0.0 key generation
    Gen4_0_0 = 0x04,
    /// 5.0.0 key generation
    Gen5_0_0 = 0x05,
    /// 6.0.0 key generation
    Gen6_0_0 = 0x06,
    /// 6.2.0 key generation
    Gen6_2_0 = 0x07,
    /// 7.0.0 key generation
    Gen7_0_0 = 0x08,
    /// 8.1.0 key generation
    Gen8_1_0 = 0x09,
    /// 9.0.0 key generation
    Gen9_0_0 = 0x0A,
    /// 9.1.0 key generation
    Gen9_1_0 = 0x0B,
    /// 12.1.0 key generation
    Gen12_1_0 = 0x0C,
    /// 13.0.0 key generation
    Gen13_0_0 = 0x0D,
    /// 14.0.0 key generation
    Gen14_0_0 = 0x0E,
    /// 15.0.0 key generation
    Gen15_0_0 = 0x0F,
    /// 16.0.0 key generation
    Gen16_0_0 = 0x10,
    /// 17.0.0 key generation
    Gen17_0_0 = 0x11,
    /// 18.0.0 key generation
    Gen18_0_0 = 0x12,
    /// 19.0.0 key generation
    Gen19_0_0 = 0x13,
    /// Key generation observed in modern NCA headers.
    Gen20_0_0 = 0x14,
    /// Key generation observed in modern NCA headers.
    Gen21_0_0 = 0x15,
    Gen22_0_0 = 0x16,
    /// Invalid key generation
    Invalid = 0xFF,
}

/// Alias for backward compatibility
pub type KeyGenerationOld = KeyGeneration;

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The encryption key index used for the key area in the NCA header.
pub enum KeyAreaEncryptionKeyIndex {
    /// Application key area encryption key.
    Application = 0x00,
    /// Ocean key area encryption key.
    Ocean = 0x01,
    /// System key area encryption key.
    System = 0x02,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Filesystem type
pub enum FsType {
    /// RomFS filesystem
    RomFs = 0x00,
    /// Partition filesystem
    PartitionFs = 0x01,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// Hash type used for filesystem verification
pub enum HashType {
    #[default]
    /// Automatically select hash type
    Auto = 0x00,
    /// No hash verification
    None = 0x01,
    /// Hierarchical SHA256 hash
    HierarchicalSha256Hash = 0x02,
    /// Hierarchical integrity hash
    HierarchicalIntegrityHash = 0x03,
    /// [14.0.0+] Automatically select SHA3 hash
    AutoSha3 = 0x04,
    /// [14.0.0+] Hierarchical SHA3-256 hash
    HierarchicalSha3256Hash = 0x05,
    /// [14.0.0+] Hierarchical integrity SHA3 hash
    HierarchicalIntegritySha3Hash = 0x06,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Encryption type for NCA content
pub enum EncryptionType {
    /// Automatically select encryption type
    Auto = 0x00,
    /// No encryption
    None = 0x01,
    /// AES-XTS encryption
    AesXts = 0x02,
    /// AES-CTR encryption
    AesCtr = 0x03,
    /// AES-CTR extended encryption
    AesCtrEx = 0x04,
    /// [14.0.0+] AES-CTR encryption with skipped layer hash
    AesCtrSkipLayerHash = 0x05,
    /// [14.0.0+] AES-CTR extended encryption with skipped layer hash
    AesCtrExSkipLayerHash = 0x06,
}

#[binrw]
#[brw(little, repr = u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// [14.0.0+] Hash type for metadata
pub enum MetaDataHashType {
    /// No metadata hash
    None = 0x00,
    /// Hierarchical integrity hash for metadata
    HierarchicalIntegrity = 0x01,
}

#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq)]
pub struct LayerRegion {
    pub offset: u64,
    pub size: u64,
}

#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq)]
pub struct HierarchicalSha256Data {
    // 0x00
    // MasterHash (SHA256 hash over the hash-table at section-start+0 with the below hash-table size)
    pub master_hash: [u8; 0x20],
    // 0x4, now we're at 0x20
    pub hash_block_size: u32,
    // 0x24
    // previously unknown field
    pub layer_count: u32,

    // We're going to split the layer regions into 2 separate fields.
    // Originally, this would be a 0x50 buffer of 5 LayerRegions,
    // but since the first region is guaranteed to be the hash table,
    // we'll use a separate field for that.
    pub hash_table_region: LayerRegion,
    #[brw(pad_size_to = 0x40)] // minus 0x10 for the hash table region
    #[br(count = 4)]
    pub layer_regions: Vec<LayerRegion>,
    // #[brw(pad_size_to = 0x80)]
    pub _reserved: [u8; 0x80],
}

#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq)]
#[br(magic = b"IVFC")] // We have skipped 0x4 bytes by checking this magic
pub struct IntegrityMetaInfo {
    pub version: u32,
    pub master_hash_size: u32,
    pub info_level_hash: InfoLevelHash,
}
#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq)]
#[br(import(hash_type: HashType))]
pub enum HashData {
    #[br(pre_assert(hash_type == HashType::HierarchicalSha256Hash))]
    HierarchicalSha256(HierarchicalSha256Data),
    #[br(pre_assert(hash_type == HashType::HierarchicalIntegrityHash))]
    HierarchicalIntegrity(IntegrityMetaInfo),
}

#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq)]
pub struct InfoLevelHash {
    pub max_layers: u32,
    #[brw(pad_size_to = 0x90)]
    #[br(count = 6)]
    pub levels: Vec<HierarchicalIntegrityLevelInfo>,
    #[brw(pad_size_to = 0x20)]
    pub signature_salt: [u8; 0x20],
}

impl InfoLevelHash {
    /// Get the number of layers in the hash data
    pub fn get_layer_count(&self) -> u32 {
        self.max_layers
    }

    /// Get the block size for a specific layer
    /// Returns None if the layer index is out of bounds
    pub fn get_block_size(&self, layer_index: usize) -> Option<u32> {
        if layer_index < self.levels.len() {
            // The block size is stored as log2, so we need to calculate 2^block_size_log2
            let block_size_log2 = self.levels[layer_index].block_size_log2;
            Some(1 << block_size_log2)
        } else {
            None
        }
    }

    pub fn get_last_layer(&self) -> Option<&HierarchicalIntegrityLevelInfo> {
        self.levels.last()
    }
}

#[binrw]
#[brw(little)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct HierarchicalIntegrityLevelInfo {
    pub logical_offset: u64,
    pub size: u64,
    pub block_size_log2: u32,
    pub _reserved: u32,
}

#[binrw]
#[brw(little)]
#[derive(Debug)]
// The FsHeader for each section is at offset + 0x400 + (sectionid * 0x200),
// where sectionid corresponds to the index used with the entry/hash tables.
/// NCA filesystem header
pub struct FsHeader {
    /// The filesystem version for the NCA.
    ///
    /// In most cases, this should always be a 2.
    pub version: u16,
    /// The type of filesystem used in this section.
    pub fs_type: FsType,
    /// The hash type used for filesystem verification.
    pub hash_type: HashType,
    /// The encryption type used for the filesystem.
    pub encryption_type: EncryptionType,
    /// Metadata hash type, only used in 14.0.0+ NCAs.
    pub metadata_hash_type: MetaDataHashType,
    _reserved: [u8; 0x2],
    #[brw(pad_size_to = 0xF8)]
    #[br(args(hash_type))] // pass the hash type to the HashData enum to determine the variant
    pub hash_data: HashData,
    #[br(count = 0x40)]
    #[brw(pad_size_to = 0x40)]
    pub patch_info: Vec<u8>,
    // now we're at 0x140

    // cntx combines these 2 fields into a single u64
    // so I don't know if I should do the same
    pub ctr: u64,
    #[brw(pad_size_to = 0x30)]
    #[br(count = 0x30)]
    pub sparse_info: Vec<u8>,
    #[brw(pad_size_to = 0x28)]
    #[br(count = 0x28)]
    pub compression_info: Vec<u8>,
    #[brw(pad_size_to = 0x30)]
    #[br(count = 0x30)]
    pub metadata_hashdata_info: Vec<u8>,
    #[brw(pad_size_to = 0x30)]
    #[br(count = 0x30)]
    _reserved2: Vec<u8>,
}
