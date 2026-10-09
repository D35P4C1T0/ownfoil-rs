
//! Defines the extended header structures for Content Meta (CNMT) files in Nintendo Switch formats.
//!
//! This module contains the various extended header types that can be found in CNMT files,
//! which provide additional metadata depending on the content type. Each content meta type
//! (Application, Patch, AddOn, etc.) has its own extended header format with specific fields
//! relevant to that content type.
//!
//! The `ExtendedHeader` enum allows for handling different header types through a common interface,
//! while the specific structs provide type-safe access to the metadata fields appropriate for
//! each content category.
use binrw::prelude::*;

/// Extended header variants based on content meta type
#[derive(Debug, Clone)]
pub enum ExtendedHeader {
    Application(ApplicationMetaExtendedHeader),
    Patch(PatchMetaExtendedHeader),
    Addon(AddonContentMetaExtendedHeader),
    Delta(DeltaMetaExtendedHeader),
    SystemUpdate(SystemUpdateMetaExtendedHeader),
    DataPatch(DataPatchMetaExtendedHeader),
    Unknown(Vec<u8>),
}

/// Extended header for System Update type
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct SystemUpdateMetaExtendedHeader {
    /// Data size
    pub extended_data_size: u32,
}

/// Extended header for Application type
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct ApplicationMetaExtendedHeader {
    /// Patch ID
    pub patch_id: u64,
    /// Minimum system version required
    pub required_system_version: u32,
    /// Required application version
    pub required_application_version: u32,
}

/// Extended header for Patch type
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct PatchMetaExtendedHeader {
    /// Application ID
    pub application_id: u64,
    /// Minimum system version required
    pub required_system_version: u32,
    /// extended data size
    pub extended_data_size: u32,
    /// Reserved
    pub _reserved: u64,
}

/// Extended header for AddOn type
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct AddonContentMetaExtendedHeader {
    /// Application ID
    pub application_id: u64,
    /// Minimum application version required
    pub required_application_version: u32,
    /// [15.0.0+] Content accessibilities
    pub content_accessibilities: u8,
    /// Reserved
    pub _reserved: [u8; 3],
    /// Data patch ID
    pub data_patch_id: u64,
}

/// Extended header for Delta type
#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct DeltaMetaExtendedHeader {
    /// Application ID
    pub application_id: u64,
    /// Extended data size
    pub extended_data_size: u32,
    /// Reserved
    pub _reserved: u32,
}

#[binrw]
#[brw(little)]
#[derive(Debug, Clone)]
pub struct DataPatchMetaExtendedHeader {
    /// Application ID
    pub application_id: u64,
    /// Minimum system version required
    pub required_application_version: u32,
    pub extended_data_size: u32,
    pub _reserved: u64,
}
