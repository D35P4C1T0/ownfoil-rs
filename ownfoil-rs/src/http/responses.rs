use std::collections::HashMap;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};

use crate::catalog::{ContentFile, ContentKind};

use crate::serve_files::FileServeError;

use super::error::ApiError;

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'{')
    .add(b'}');

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_files: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiEntry {
    pub id: String,
    pub name: String,
    pub title_id: Option<String>,
    #[serde(rename = "titleid", skip_serializing_if = "Option::is_none")]
    pub titleid: Option<String>,
    #[serde(rename = "titleId", skip_serializing_if = "Option::is_none")]
    pub title_id_camel: Option<String>,
    pub version: Option<u32>,
    #[serde(rename = "ver", skip_serializing_if = "Option::is_none")]
    pub ver: Option<u32>,
    pub kind: ContentKind,
    #[serde(rename = "type")]
    pub content_type: ContentKind,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    pub total: usize,
    pub success: &'static str,
    pub files: Vec<ShopFile>,
    pub directories: Vec<String>,
    pub entries: Vec<ApiEntry>,
    pub sections: Vec<SectionInfo>,
}

#[derive(Debug, Serialize)]
pub struct SectionsResponse {
    pub sections: Vec<SectionInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SectionInfo {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ShopRootResponse {
    pub success: &'static str,
    pub files: Vec<ShopRootFile>,
}

#[derive(Debug, Serialize)]
pub struct ShopRootFile {
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct ShopSectionsResponse {
    pub sections: Vec<ShopSection>,
}

#[derive(Debug, Serialize)]
pub struct ShopSection {
    pub id: &'static str,
    pub title: &'static str,
    pub items: Vec<ShopSectionItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShopSectionItem {
    pub name: String,
    pub title_name: String,
    pub title_id: Option<String>,
    pub app_id: String,
    pub app_version: String,
    pub app_type: &'static str,
    pub category: String,
    pub icon_url: String,
    #[serde(rename = "iconUrl")]
    pub icon_url_camel: String,
    pub url: String,
    pub size: u64,
    pub file_id: usize,
    pub filename: String,
    pub download_count: u64,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub success: &'static str,
    pub files: Vec<ShopFile>,
    pub entries: Vec<ApiEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShopFile {
    pub id: String,
    pub url: String,
    pub size: u64,
    pub name: String,
    pub title_id: Option<String>,
    #[serde(rename = "titleid", skip_serializing_if = "Option::is_none")]
    pub titleid: Option<String>,
    #[serde(rename = "titleId", skip_serializing_if = "Option::is_none")]
    pub title_id_camel: Option<String>,
    pub version: Option<u32>,
    #[serde(rename = "ver", skip_serializing_if = "Option::is_none")]
    pub ver: Option<u32>,
    pub kind: ContentKind,
    #[serde(rename = "type")]
    pub content_type: ContentKind,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Debug, Deserialize)]
pub struct ShopSectionsQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SavesListResponse {
    pub success: bool,
    pub saves: Vec<SavedItem>,
}

#[derive(Debug, Serialize)]
pub struct SavedItem {
    pub name: String,
    pub title_id: String,
    pub save_id: String,
    pub note: String,
    pub created_at: String,
    pub created_ts: u64,
    pub download_url: String,
    pub size: u64,
}

impl From<&ApiEntry> for ShopFile {
    fn from(entry: &ApiEntry) -> Self {
        ShopFile {
            id: entry.id.clone(),
            url: entry.url.clone(),
            size: entry.size,
            name: entry.name.clone(),
            title_id: entry.title_id.clone(),
            titleid: entry.title_id.clone(),
            title_id_camel: entry.title_id.clone(),
            version: entry.version,
            ver: entry.version,
            kind: entry.kind,
            content_type: entry.kind,
        }
    }
}

pub fn map_to_entries<'a>(files: impl IntoIterator<Item = &'a ContentFile>) -> Vec<ApiEntry> {
    files.into_iter().map(entry_to_api).collect()
}

pub fn map_shop_files(entries: &[ApiEntry]) -> Vec<ShopFile> {
    entries.iter().map(ShopFile::from).collect()
}

pub fn entry_to_api(file: &ContentFile) -> ApiEntry {
    let rel = file.relative_path.to_string_lossy();
    let encoded_segments = rel
        .split('/')
        .map(|segment| utf8_percent_encode(segment, PATH_SEGMENT_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/");

    ApiEntry {
        id: rel.to_string(),
        name: file.name.clone(),
        title_id: file.title_id.clone(),
        titleid: file.title_id.clone(),
        title_id_camel: file.title_id.clone(),
        version: file.version,
        ver: file.version,
        kind: file.kind,
        content_type: file.kind,
        size: file.size,
        url: format!("/download/{encoded_segments}"),
    }
}

pub fn build_catalog_response(entries: Vec<ApiEntry>) -> CatalogResponse {
    CatalogResponse {
        success: "ok",
        total: entries.len(),
        files: map_shop_files(&entries),
        directories: Vec::new(),
        entries,
        sections: catalog_sections(),
    }
}

pub fn build_shop_root_files(files: &[ContentFile]) -> Vec<ShopRootFile> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| ShopRootFile {
            url: shop_game_url(index + 1, &file.name),
            size: file.size,
        })
        .collect()
}

pub fn build_shop_sections_payload(files: &[ContentFile], limit: usize) -> ShopSectionsResponse {
    let indexed: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i + 1, f))
        .collect();

    let base_items = collect_base_items(&indexed);
    let update_items_full = collect_latest_by_key(&indexed, ContentKind::Update, |item| {
        item.title_id.clone().unwrap_or_else(|| item.app_id.clone())
    });
    let dlc_items_full =
        collect_latest_by_key(&indexed, ContentKind::Dlc, |item| item.app_id.clone());

    let mut all_items: Vec<_> = base_items
        .iter()
        .chain(update_items_full.iter())
        .chain(dlc_items_full.iter())
        .cloned()
        .collect();
    all_items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let all_total = all_items.len();

    let new_items = base_items.iter().take(limit).cloned().collect::<Vec<_>>();
    let new_items = if new_items.is_empty() {
        all_items.iter().take(limit).cloned().collect()
    } else {
        new_items
    };
    let recommended_items = if new_items.is_empty() {
        all_items.iter().take(limit).cloned().collect()
    } else {
        new_items.clone()
    };

    ShopSectionsResponse {
        sections: vec![
            ShopSection {
                id: "new",
                title: "New",
                items: new_items,
                total: None,
                truncated: None,
            },
            ShopSection {
                id: "recommended",
                title: "Recommended",
                items: recommended_items,
                total: None,
                truncated: None,
            },
            ShopSection {
                id: "updates",
                title: "Updates",
                items: update_items_full.iter().take(limit).cloned().collect(),
                total: None,
                truncated: None,
            },
            ShopSection {
                id: "dlc",
                title: "DLC",
                items: dlc_items_full.iter().take(limit).cloned().collect(),
                total: None,
                truncated: None,
            },
            ShopSection {
                id: "all",
                title: "All",
                items: all_items,
                total: Some(all_total),
                truncated: Some(false),
            },
        ],
    }
}

fn collect_base_items(indexed: &[(usize, &ContentFile)]) -> Vec<ShopSectionItem> {
    let mut items: Vec<_> = indexed
        .iter()
        .filter_map(|(idx, file)| {
            matches!(file.kind, ContentKind::Base | ContentKind::Unknown)
                .then(|| to_shop_section_item(*idx, file))
        })
        .collect();
    items.sort_by(|a, b| b.file_id.cmp(&a.file_id));
    items
}

fn collect_latest_by_key<F>(
    indexed: &[(usize, &ContentFile)],
    kind: ContentKind,
    key_fn: F,
) -> Vec<ShopSectionItem>
where
    F: Fn(&ShopSectionItem) -> String,
{
    let mut latest: HashMap<String, ShopSectionItem> = HashMap::new();
    for (idx, file) in indexed.iter().filter(|(_, f)| f.kind == kind).copied() {
        let item = to_shop_section_item(idx, file);
        let key = key_fn(&item);
        let keep = latest.get(&key).map_or(true, |cur| {
            parse_version_number(&item.app_version) > parse_version_number(&cur.app_version)
        });
        if keep {
            latest.insert(key, item);
        }
    }
    let mut out: Vec<_> = latest.into_values().collect();
    out.sort_by(|a, b| {
        parse_version_number(&b.app_version).cmp(&parse_version_number(&a.app_version))
    });
    out
}

fn to_shop_section_item(file_id: usize, file: &ContentFile) -> ShopSectionItem {
    let app_id = file
        .title_id
        .as_ref()
        .map(String::from)
        .unwrap_or_else(|| file.name.clone());
    let base_title_id = derive_base_title_id(file.kind, file.title_id.as_deref());
    let icon_url = base_title_id
        .as_deref()
        .map(shop_icon_url)
        .unwrap_or_default();
    let app_version = file
        .version
        .map(|v| v.to_string())
        .unwrap_or_else(|| String::from("0"));

    ShopSectionItem {
        name: file.name.clone(),
        title_name: file.name.clone(),
        title_id: base_title_id,
        app_id,
        app_version,
        app_type: app_type_for_kind(file.kind),
        category: String::new(),
        icon_url: icon_url.clone(),
        icon_url_camel: icon_url,
        url: shop_game_url(file_id, &file.name),
        size: file.size,
        file_id,
        filename: file.name.clone(),
        download_count: 0,
    }
}

fn app_type_for_kind(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Base | ContentKind::Unknown => "BASE",
        ContentKind::Update => "UPDATE",
        ContentKind::Dlc => "DLC",
    }
}

fn derive_base_title_id(kind: ContentKind, title_id: Option<&str>) -> Option<String> {
    let raw = title_id?;
    if raw.len() != 16 || !raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let normalized = raw.to_ascii_uppercase();
    match kind {
        ContentKind::Base | ContentKind::Unknown => Some(normalized),
        ContentKind::Update => {
            let mut chars = normalized.chars().collect::<Vec<_>>();
            let len = chars.len();
            chars[len - 3] = '0';
            chars[len - 2] = '0';
            chars[len - 1] = '0';
            Some(chars.into_iter().collect::<String>())
        }
        ContentKind::Dlc => {
            let high = &normalized[..13];
            let high_value = u64::from_str_radix(high, 16).ok()?;
            let base_high = high_value.checked_sub(1)?;
            Some(format!("{base_high:013X}000"))
        }
    }
}

fn parse_version_number(raw: &str) -> u64 {
    raw.parse::<u64>().unwrap_or(0)
}

fn shop_game_url(file_id: usize, filename: &str) -> String {
    format!("/api/get_game/{file_id}#{filename}")
}

fn shop_icon_url(title_id: &str) -> String {
    format!("/api/shop/icon/{title_id}.png")
}

pub fn catalog_sections() -> Vec<SectionInfo> {
    vec![
        SectionInfo {
            id: "new",
            label: "New",
        },
        SectionInfo {
            id: "recommended",
            label: "Recommended",
        },
        SectionInfo {
            id: "updates",
            label: "Updates",
        },
        SectionInfo {
            id: "dlc",
            label: "DLC",
        },
        SectionInfo {
            id: "all",
            label: "All",
        },
    ]
}

pub fn static_png_response() -> axum::response::Response {
    const PLACEHOLDER_PNG: [u8; 68] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
        0xff, 0x1f, 0x00, 0x03, 0x03, 0x02, 0x00, 0xee, 0xd9, 0xda, 0x2a, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    use axum::body::Body;
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use axum::http::HeaderValue;

    let mut response = axum::response::Response::new(Body::from(PLACEHOLDER_PNG.to_vec()));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    response
}

pub fn map_file_error(error: FileServeError) -> ApiError {
    match error {
        FileServeError::InvalidPath => ApiError::InvalidPath,
        FileServeError::NotFound => ApiError::NotFound,
        FileServeError::InvalidRange => ApiError::InvalidRange,
        FileServeError::Io(_) | FileServeError::HeaderValue(_) => ApiError::Internal,
    }
}
