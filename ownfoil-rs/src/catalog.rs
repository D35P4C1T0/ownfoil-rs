//! Catalog: in-memory index of content files with title/version grouping.
//!
//! Parses filenames for 16-char hex title IDs and version numbers. Classifies content
//! as Base (suffix `000`), Update (`800`), or DLC (other).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

/// Content type derived from title ID suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Base,
    Update,
    Dlc,
    Unknown,
}

/// A single content file (NSP, XCI, etc.) with parsed metadata.
/// A single content file (NSP, XCI, etc.) with parsed metadata.
#[derive(Debug, Clone, Serialize)]
pub struct ContentFile {
    pub relative_path: PathBuf,
    pub name: String,
    pub size: u64,
    pub title_id: Option<String>,
    pub version: Option<u32>,
    pub kind: ContentKind,
}

/// All file versions for a given base title ID.
#[derive(Debug, Clone, Serialize)]
pub struct TitleVersions {
    pub title_id: String,
    pub files: Vec<ContentFile>,
}

/// Sorted index of content files with lookup by title ID.
#[derive(Debug, Clone)]
pub struct Catalog {
    files: Vec<ContentFile>,
    titles: BTreeMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ParsedFilename {
    pub title_id: Option<[char; 16]>,
    pub version: Option<u32>,
}

static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<title>[0-9a-f]{16})")
        .unwrap_or_else(|e| panic!("title regex must be valid: {e}"))
});

static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\[v(?P<version>\d+)\]|(?:^|[^\w])v(?P<version_2>\d+)")
        .unwrap_or_else(|e| panic!("version regex must be valid: {e}"))
});

impl Catalog {
    /// Build a catalog from scanned files. Sorts by title_id, version, name.
    pub fn from_files(mut files: Vec<ContentFile>) -> Self {
        files.sort_by(|left, right| {
            left.title_id
                .cmp(&right.title_id)
                .then(left.version.cmp(&right.version))
                .then(left.name.cmp(&right.name))
        });

        let mut titles: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (idx, file) in files.iter().enumerate() {
            if let Some(title_id) = &file.title_id {
                titles.entry(title_id.to_string()).or_default().push(idx);
            }
        }

        Self { files, titles }
    }

    pub fn files(&self) -> &[ContentFile] {
        &self.files
    }

    pub fn files_by_kind(&self, kind: ContentKind) -> Vec<&ContentFile> {
        self.files
            .iter()
            .filter(|file| file.kind == kind)
            .collect::<Vec<_>>()
    }

    pub fn search(&self, query: &str) -> Vec<&ContentFile> {
        let q = query.to_ascii_lowercase();
        self.files
            .iter()
            .filter(|file| {
                file.name.to_ascii_lowercase().contains(&q)
                    || file
                        .title_id
                        .as_ref()
                        .map(|title| title.to_ascii_lowercase().contains(&q))
                        .unwrap_or(false)
            })
            .collect::<Vec<_>>()
    }

    /// Get all versions (base, update, DLC) for a base title ID.
    pub fn versions(&self, title_id: &str) -> Option<TitleVersions> {
        let key = title_id.to_ascii_uppercase();
        self.titles.get(&key).map(|indices| TitleVersions {
            title_id: key,
            files: indices
                .iter()
                .filter_map(|index| self.files.get(*index))
                .cloned()
                .collect::<Vec<_>>(),
        })
    }
}

pub fn parse_filename_metadata(name: &str) -> ParsedFilename {
    let title_id = TITLE_RE
        .captures(name)
        .and_then(|c| c.name("title"))
        .and_then(|m| to_upper_hex_chars(m.as_str()));

    let version = VERSION_RE
        .captures(name)
        .and_then(|c| {
            c.name("version")
                .or_else(|| c.name("version_2"))
                .map(|m| m.as_str())
        })
        .and_then(|raw| raw.parse::<u32>().ok());

    ParsedFilename { title_id, version }
}

pub fn classify_title_id(title_id: Option<&str>) -> ContentKind {
    let Some(title_id) = title_id else {
        return ContentKind::Unknown;
    };

    let normalized = title_id.to_ascii_uppercase();
    if normalized.ends_with("000") {
        ContentKind::Base
    } else if normalized.ends_with("800") {
        ContentKind::Update
    } else {
        ContentKind::Dlc
    }
}

pub fn to_display_title_id(raw: Option<[char; 16]>) -> Option<String> {
    raw.map(|chars| chars.into_iter().collect::<String>())
}

fn to_upper_hex_chars(raw: &str) -> Option<[char; 16]> {
    if raw.len() != 16 {
        return None;
    }

    let mut output = ['0'; 16];
    for (index, ch) in raw.chars().enumerate() {
        let upper = ch.to_ascii_uppercase();
        if !upper.is_ascii_hexdigit() {
            return None;
        }
        if let Some(slot) = output.get_mut(index) {
            *slot = upper;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{classify_title_id, parse_filename_metadata, Catalog, ContentFile, ContentKind};

    #[test]
    fn parse_filename_extracts_title_id_and_version() {
        let parsed = parse_filename_metadata("My Game [0100ABCD12340000][v65536].nsp");

        assert_eq!(
            parsed
                .title_id
                .map(|chars| chars.into_iter().collect::<String>()),
            Some(String::from("0100ABCD12340000"))
        );
        assert_eq!(parsed.version, Some(65536));
    }

    #[test]
    fn parse_filename_accepts_plain_v_version() {
        let parsed = parse_filename_metadata("0100ABCD12340800 v131072.nsp");

        assert_eq!(
            parsed
                .title_id
                .map(|chars| chars.into_iter().collect::<String>()),
            Some(String::from("0100ABCD12340800"))
        );
        assert_eq!(parsed.version, Some(131072));
    }

    #[test]
    fn classify_title_id_heuristics() {
        assert_eq!(
            classify_title_id(Some("0100ABCD12340000")),
            ContentKind::Base
        );
        assert_eq!(
            classify_title_id(Some("0100ABCD12340800")),
            ContentKind::Update
        );
        assert_eq!(
            classify_title_id(Some("0100ABCD12340001")),
            ContentKind::Dlc
        );
        assert_eq!(classify_title_id(None), ContentKind::Unknown);
    }

    #[test]
    fn catalog_groups_versions_by_title() {
        let files = vec![
            ContentFile {
                relative_path: PathBuf::from("a/base.nsp"),
                name: String::from("base.nsp"),
                size: 1,
                title_id: Some(String::from("0100ABCD12340000")),
                version: Some(0),
                kind: ContentKind::Base,
            },
            ContentFile {
                relative_path: PathBuf::from("a/update.nsp"),
                name: String::from("update.nsp"),
                size: 1,
                title_id: Some(String::from("0100ABCD12340000")),
                version: Some(65536),
                kind: ContentKind::Update,
            },
        ];

        let catalog = Catalog::from_files(files);
        let versions = catalog.versions("0100ABCD12340000");

        let versions = versions.map(|item| item.files).unwrap_or_default();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, Some(0));
        assert_eq!(versions[1].version, Some(65536));
    }
}
