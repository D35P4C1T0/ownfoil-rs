//! Persistent artwork shared by native HTTP, extraction and durable workers.
use crate::{http::AppState, storage::Storage};
use anyhow::{Context, bail};
use image::GenericImageView;
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Cursor, path::Path, sync::LazyLock, time::Duration};

const MAX_BYTES: usize = 16 * 1024 * 1024;
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default()
});
// Also bounds simultaneous HTTP cache misses, independently of worker count.
static TRANSFERS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(4));
static DOWNLOADS: LazyLock<dashmap::DashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);
static INGESTION: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
pub const KINDS: &[&str] = &["icon", "banner", "screenshot"];
pub const SIZES: &[&str] = &["original", "thumb", "client", "screen"];

pub fn source_hash(source: &str) -> String {
    hex::encode(Sha256::digest(source.as_bytes()))
}
pub fn source<'a>(title: &'a Value, kind: &str, position: usize) -> Option<&'a str> {
    let value = match kind {
        "icon" => title["iconUrl"].as_str(),
        "banner" => title["bannerUrl"].as_str(),
        "screenshot" => title["screenshots"].as_array()?.get(position)?.as_str(),
        _ => None,
    }?;
    (!value.is_empty()).then_some(value)
}

pub async fn initialize(storage: &Storage) -> anyhow::Result<()> {
    storage.with_connection(|conn| {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS media_slots(title_id TEXT NOT NULL,kind TEXT NOT NULL,position INTEGER NOT NULL,source TEXT NOT NULL,filename TEXT NOT NULL,width INTEGER NOT NULL,height INTEGER NOT NULL,PRIMARY KEY(title_id,kind,position)); CREATE TABLE IF NOT EXISTS media_assets(kind TEXT NOT NULL,filename TEXT NOT NULL,width INTEGER NOT NULL,height INTEGER NOT NULL,PRIMARY KEY(kind,filename));")?;
        Ok(())
    }).await?;
    Ok(())
}

pub fn bounds(kind: &str, size: &str) -> Option<(u32, u32)> {
    match (kind == "icon", size) {
        (true, "thumb") => Some((176, 176)),
        (false, "thumb") => Some((320, 180)),
        (true, "client") => Some((256, 256)),
        (false, "client") => Some((720, 405)),
        (true, "screen") => Some((720, 720)),
        (false, "screen") => Some((1280, 720)),
        _ => None,
    }
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Fitted positive dimensions are bounded by the original u32 dimensions.
pub fn fit(width: u32, height: u32, kind: &str, size: &str) -> (u32, u32) {
    let Some((w, h)) = bounds(kind, size) else { return (width, height) };
    let scale = (f64::from(w) / f64::from(width)).min(f64::from(h) / f64::from(height)).min(1.0);
    (
        (f64::from(width) * scale).round_ties_even().max(1.0) as u32,
        (f64::from(height) * scale).round_ties_even().max(1.0) as u32,
    )
}
fn decode(bytes: &[u8]) -> anyhow::Result<image::DynamicImage> {
    if bytes.len() > MAX_BYTES {
        bail!("Artwork exceeds download limit")
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn render(
    image: &image::DynamicImage,
    kind: &str,
    size: &str,
    format: image::ImageFormat,
) -> anyhow::Result<Vec<u8>> {
    let (w, h) = fit(image.width(), image.height(), kind, size);
    let resized = image.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    let mut bytes = Vec::new();
    if format == image::ImageFormat::Jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut bytes,
            if size == "thumb" { 90 } else { 85 },
        )
        .encode_image(&resized.to_rgb8())?;
    } else {
        resized.write_to(&mut Cursor::new(&mut bytes), format)?;
    }
    Ok(bytes)
}
async fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("Media path missing parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(error) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error.into());
    }
    Ok(())
}

/// Publish the slot only after every rendition is safely on disk.
pub async fn ingest(
    state: &AppState,
    title: &str,
    kind: &str,
    position: usize,
    source: &str,
    bytes: &[u8],
) -> anyhow::Result<Value> {
    if !KINDS.contains(&kind) {
        bail!("Invalid artwork kind")
    }
    let _guard = INGESTION.lock().await;
    let bytes = bytes.to_vec();
    let render_kind = kind.to_owned();
    let (width, height, original, renditions) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let format = image::guess_format(&bytes)?;
            let image = decode(&bytes)?;
            let (width, height) = image.dimensions();
            let renditions = ["thumb", "client", "screen"]
                .into_iter()
                .map(|size| Ok((size, render(&image, &render_kind, size, format)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok((width, height, bytes, renditions))
        })
        .await??;
    let filename = format!("{}.jpg", source_hash(source));
    let root = state.data_dir.join("media").join(kind);
    write(&root.join("original").join(&filename), &original).await?;
    for (size, bytes) in renditions {
        write(&root.join(size).join(&filename), &bytes).await?;
    }
    if let Some(storage) = &state.storage {
        initialize(storage).await?;
        let title = title.to_ascii_uppercase();
        let kind = kind.to_owned();
        let source = source.to_owned();
        let filename = filename.clone();
        storage.with_connection(move |conn| {
            conn.execute("INSERT INTO media_assets(kind,filename,width,height) VALUES(?1,?2,?3,?4) ON CONFLICT(kind,filename) DO UPDATE SET width=excluded.width,height=excluded.height",params![kind,filename,width,height])?;
            conn.execute("INSERT INTO media_slots(title_id,kind,position,source,filename,width,height) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(title_id,kind,position) DO UPDATE SET source=excluded.source,filename=excluded.filename,width=excluded.width,height=excluded.height",params![title,kind,i64::try_from(position).unwrap_or(i64::MAX),source,filename,width,height])?;
            Ok(())
        }).await?;
    }
    *state.titles_cache.write().await = None;
    Ok(
        json!({"kind":kind,"position":position,"source":source,"filename":filename,"width":width,"height":height}),
    )
}

pub async fn attach(storage: &Storage, title: &mut Value) -> anyhow::Result<()> {
    initialize(storage).await?;
    let id = title["titleId"].as_str().unwrap_or_default().to_ascii_uppercase();
    let slots = storage.with_connection(move |conn| {
        let mut stmt = conn.prepare("SELECT kind,position,source,filename,width,height FROM media_slots WHERE title_id=?1 ORDER BY kind,position")?;
        let rows = stmt.query_map([id],|r| Ok(json!({"kind":r.get::<_,String>(0)?,"position":r.get::<_,i64>(1)?,"source":r.get::<_,String>(2)?,"filename":r.get::<_,String>(3)?,"width":r.get::<_,u32>(4)?,"height":r.get::<_,u32>(5)?})))?;
        Ok(rows.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    let mut slots = slots;
    for kind in KINDS {
        let count = if *kind == "screenshot" {
            title["screenshots"].as_array().map_or(0, Vec::len)
        } else {
            1
        };
        for position in 0..count {
            let Some(url) =
                source(title, kind, position).filter(|url| url.starts_with("/api/media/"))
            else {
                continue;
            };
            let Some(filename) = url.rsplit('/').next().filter(|name| {
                name.len() == 68
                    && name.strip_suffix(".jpg").is_some()
                    && name.as_bytes().iter().take(64).all(u8::is_ascii_hexdigit)
            }) else {
                continue;
            };
            let source = url.to_owned();
            let filename = filename.to_owned();
            let kind = kind.to_string();
            let asset_kind = kind.clone();
            let asset_filename = filename.clone();
            let dimensions = storage
                .with_connection(move |conn| {
                    use rusqlite::OptionalExtension;
                    conn.query_row(
                        "SELECT width,height FROM media_assets WHERE kind=?1 AND filename=?2",
                        params![asset_kind, asset_filename],
                        |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)),
                    )
                    .optional()
                    .map_err(crate::storage::StorageError::from)
                })
                .await?;
            if let Some((width, height)) = dimensions {
                slots.retain(|slot| slot["kind"] != kind || slot["position"] != position);
                slots.push(json!({"kind":kind,"position":position,"source":source,"filename":filename,"width":width,"height":height}));
            }
        }
    }
    title["_media"] = json!(slots);
    Ok(())
}

pub async fn download(
    state: &AppState,
    title: &str,
    kind: &str,
    position: usize,
    url: &str,
) -> anyhow::Result<()> {
    if url.starts_with("/api/media/") {
        return Ok(());
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("Invalid artwork URL")
    }
    let lock = DOWNLOADS
        .entry(source_hash(url))
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    let filename = format!("{}.jpg", source_hash(url));
    let original = state.data_dir.join("media").join(kind).join("original").join(&filename);
    if let Ok(bytes) = tokio::fs::read(&original).await {
        ingest(state, title, kind, position, url, &bytes).await?;
        return Ok(());
    }
    // Retain the preexisting source cache and native URLs during migration.
    let legacy = state.data_dir.join("media").join("source").join(&filename);
    if let Ok(bytes) = tokio::fs::read(&legacy).await {
        ingest(state, title, kind, position, url, &bytes).await?;
        return Ok(());
    }
    let _permit = TRANSFERS.acquire().await?;
    let mut response = CLIENT.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
        bail!("Artwork exceeds download limit")
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            bail!("Artwork exceeds download limit")
        }
        bytes.extend_from_slice(&chunk);
    }
    ingest(state, title, kind, position, url, &bytes).await?;
    Ok(())
}

/// Grace protects newly written files until their row/override is committed.
pub async fn collect(state: &AppState) -> anyhow::Result<Value> {
    let storage = state.storage.as_ref().context("Media storage unavailable")?;
    initialize(storage).await?;
    let _guard = INGESTION.lock().await;
    let (keep,overrides) = storage.with_connection(|conn| {
        conn.execute("DELETE FROM media_slots WHERE title_id NOT IN (SELECT title_id FROM titles)",[])?;
        let mut stmt = conn.prepare("SELECT kind,filename FROM media_slots")?;
        let keep = stmt.query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<std::collections::HashSet<_>,_>>()?;
        let mut stmt = conn.prepare("SELECT record FROM title_overrides UNION ALL SELECT record FROM extracted_title_overrides UNION ALL SELECT record FROM extraction_sources")?;
        let overrides = stmt.query_map([],|r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
        Ok((keep,overrides))
    }).await?;
    let mut removed = 0;
    let mut usage = json!({});
    for kind in KINDS {
        for size in SIZES {
            let mut files = 0u64;
            let mut bytes = 0u64;
            let directory = state.data_dir.join("media").join(kind).join(size);
            if let Ok(mut entries) = tokio::fs::read_dir(&directory).await {
                while let Some(entry) = entries.next_entry().await? {
                    let metadata = entry.metadata().await?;
                    if !metadata.is_file() {
                        continue;
                    }
                    let filename = entry.file_name().to_string_lossy().into_owned();
                    let named = keep.contains(&(kind.to_string(), filename.clone()))
                        || overrides.iter().any(|text| {
                            text.contains(&format!("/{kind}/"))
                                && text.contains(&format!("/{filename}"))
                        });
                    let old = metadata.modified()?.elapsed().unwrap_or_default()
                        >= Duration::from_secs(3600);
                    if !named && old {
                        tokio::fs::remove_file(entry.path()).await?;
                        removed += 1;
                    } else {
                        files += 1;
                        bytes += metadata.len();
                    }
                }
            }
            usage[*kind][*size] = json!({"files":files,"bytes":bytes});
        }
    }
    Ok(json!({"removed":removed,"usage":usage}))
}

/// Filesystem accounting counts shared content once and includes retired files pending collection.
pub async fn usage(state: &AppState) -> anyhow::Result<Value> {
    let mut usage = json!({});
    for kind in KINDS {
        for size in SIZES {
            let mut files = 0u64;
            let mut bytes = 0u64;
            if let Ok(mut entries) =
                tokio::fs::read_dir(state.data_dir.join("media").join(kind).join(size)).await
            {
                while let Some(entry) = entries.next_entry().await? {
                    let metadata = entry.metadata().await?;
                    if metadata.is_file() {
                        files += 1;
                        bytes += metadata.len();
                    }
                }
            }
            usage[*kind][*size] = json!({"files":files,"bytes":bytes});
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendition_dimensions_preserve_ratio_and_small_images() {
        assert_eq!(fit(1920, 1080, "banner", "client"), (720, 405));
        assert_eq!(fit(512, 512, "icon", "thumb"), (176, 176));
        assert_eq!(fit(100, 50, "banner", "screen"), (100, 50));
        assert_eq!(fit(1, 8192, "banner", "thumb"), (1, 180));
        assert_eq!(fit(352, 5, "icon", "thumb"), (176, 2));
    }
    #[test]
    fn jpeg_renditions_preserve_full_chroma() -> anyhow::Result<()> {
        let image = image::DynamicImage::new_rgb8(512, 512);
        let bytes = render(&image, "icon", "client", image::ImageFormat::Jpeg)?;
        let marker = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .context("Missing JPEG baseline frame")?;
        let components = &bytes[marker + 10..marker + 19];
        assert_eq!(components[1], 0x11);
        assert_eq!(components[4], 0x11);
        assert_eq!(components[7], 0x11);
        Ok(())
    }
    #[test]
    fn limits_reject_oversized_input() {
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_err());
    }
}
