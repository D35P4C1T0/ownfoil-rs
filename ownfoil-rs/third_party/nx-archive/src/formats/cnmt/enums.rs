

//! Module for handling Content Meta (CNMT) enums.
//! 
//! This module provides enumerations for dealing with Nintendo Switch content metadata:
//! - `ContentMetaType`: Defines the different types of content meta that can exist (applications, 
//!   system data, updates, etc.)
//! - `ContentMetaPlatform`: Defines the platform for content (currently only Nintendo Switch)
//!
//! These enums are serializable/deserializable via the binrw crate and are used in parsing 
//! Nintendo Switch content metadata files.
use binrw::prelude::*;

/// Content Meta Type for application
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[binrw]
#[brw(little, repr = u8)]
#[repr(u8)]
pub enum ContentMetaType {
    Invalid = 0x00,
    /// A system application,
    /// (e.g sysmodules, applets, etc)
    SystemProgram = 0x01,
    /// A system data archive
    SystemData = 0x02,
    /// A system update
    SystemUpdate = 0x03,
    /// A boot image package
    /// (Firmware package A or C)
    BootImagePackage = 0x04,
    /// A boot image package (safe mode)
    /// (Firmware package B or D)
    BootImagePackageSafe = 0x05,
    /// An application
    Application = 0x80,
    /// A patch
    Patch = 0x81,
    /// An add-on
    AddOnContent = 0x82,
    /// A delta fragment
    /// (e.g. a delta update)
    Delta = 0x83,
    /// [15.0.0+] A data patch
    DataPatch = 0x84,
}



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[binrw]
#[brw(little, repr = u8)]
#[repr(u8)]
pub enum ContentMetaPlatform {
    /// NX (Nintendo Switch)
    NX = 0x00,
}

