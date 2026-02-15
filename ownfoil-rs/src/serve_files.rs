use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use axum::body::Body;
use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt};
use std::io;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum FileServeError {
    #[error("invalid path")]
    InvalidPath,
    #[error("file not found")]
    NotFound,
    #[error("unsupported range")]
    InvalidRange,
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid header value")]
    HeaderValue(#[from] axum::http::header::InvalidHeaderValue),
}

#[derive(Debug, Clone, Copy)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

pub fn sanitize_relative_path(requested_path: &str) -> Result<PathBuf, FileServeError> {
    let requested = Path::new(requested_path.trim_start_matches('/'));
    let mut sanitized = PathBuf::new();

    for component in requested.components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(FileServeError::InvalidPath);
            }
        }
    }

    if sanitized.as_os_str().is_empty() {
        return Err(FileServeError::InvalidPath);
    }

    Ok(sanitized)
}

/// Context for download logging (IP, title). When provided, logs progress during transfer.
pub struct DownloadLogContext {
    pub ip: std::net::SocketAddr,
    pub title: String,
}

fn wrap_with_progress_log<S>(
    stream: S,
    total: u64,
    ip: std::net::SocketAddr,
    title: String,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send,
{
    let sent = AtomicU64::new(0);
    let last_pct = AtomicU8::new(0);
    stream.map(move |item| {
        if let Ok(ref chunk) = item {
            let new_sent =
                sent.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
            let pct = if total > 0 {
                ((new_sent as f64 / total as f64) * 100.0) as u8
            } else {
                100
            };
            let prev = last_pct.load(Ordering::Relaxed);
            if pct >= prev.saturating_add(25) || new_sent >= total {
                last_pct.store(pct.min(100), Ordering::Relaxed);
                info!(
                    ip = %ip,
                    title = %title,
                    progress = %format!("{}%", pct.min(100)),
                    sent = new_sent,
                    total,
                    "content download"
                );
            }
        }
        item
    })
}

pub async fn stream_with_range_support(
    root: &Path,
    requested_path: &Path,
    headers: &HeaderMap,
    log_context: Option<&DownloadLogContext>,
) -> Result<Response, FileServeError> {
    let path = root.join(requested_path);
    let metadata = tokio::fs::metadata(&path).await.map_err(|e| {
        warn!(
            path = %requested_path.display(),
            error = %e,
            "download target not found or inaccessible"
        );
        FileServeError::NotFound
    })?;

    if !metadata.is_file() {
        return Err(FileServeError::NotFound);
    }

    let file_size = metadata.len();
    let maybe_range = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| parse_range_header(value, file_size));

    let mut file = File::open(&path).await?;

    let (status, content_length, content_range, body): (StatusCode, u64, Option<String>, Body) =
        match maybe_range {
            Some(Ok(range)) => {
                file.seek(SeekFrom::Start(range.start)).await?;
                let limited = file.take(range.len());
                let stream = ReaderStream::new(limited);
                let (stream, content_len) = if let Some(ctx) = log_context {
                    info!(
                        ip = %ctx.ip,
                        title = %ctx.title,
                        progress = "0%",
                        sent = 0u64,
                        total = range.len(),
                        "content download"
                    );
                    (
                        Body::from_stream(wrap_with_progress_log(
                            stream,
                            range.len(),
                            ctx.ip,
                            ctx.title.clone(),
                        )),
                        range.len(),
                    )
                } else {
                    (Body::from_stream(stream), range.len())
                };
                debug!(
                    path = %requested_path.display(),
                    start = range.start,
                    end = range.end,
                    file_size,
                    "serving ranged download"
                );
                (
                    StatusCode::PARTIAL_CONTENT,
                    content_len,
                    Some(format!("bytes {}-{}/{}", range.start, range.end, file_size)),
                    stream,
                )
            }
            Some(Err(_)) => {
                warn!(
                    path = %requested_path.display(),
                    file_size,
                    "invalid byte range requested"
                );
                let mut response = Response::new(Body::from(Vec::<u8>::new()));
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{file_size}"))?,
                );
                return Ok(response);
            }
            None => {
                let stream = ReaderStream::new(file);
                let (stream, content_len) = if let Some(ctx) = log_context {
                    info!(
                        ip = %ctx.ip,
                        title = %ctx.title,
                        progress = "0%",
                        sent = 0u64,
                        total = file_size,
                        "content download"
                    );
                    (
                        Body::from_stream(wrap_with_progress_log(
                            stream,
                            file_size,
                            ctx.ip,
                            ctx.title.clone(),
                        )),
                        file_size,
                    )
                } else {
                    (Body::from_stream(stream), file_size)
                };
                debug!(
                    path = %requested_path.display(),
                    file_size,
                    "serving full download"
                );
                (StatusCode::OK, content_len, None, stream)
            }
        };

    let mut response = Response::new(body);
    *response.status_mut() = status;

    response
        .headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string())?,
    );

    if let Some(value) = content_range {
        response
            .headers_mut()
            .insert(CONTENT_RANGE, HeaderValue::from_str(&value)?);
    }

    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_str(&content_type)?);

    Ok(response)
}

fn parse_range_header(value: &str, file_size: u64) -> Result<ByteRange, FileServeError> {
    if file_size == 0 || !value.starts_with("bytes=") {
        return Err(FileServeError::InvalidRange);
    }

    let raw = &value[6..];
    if raw.contains(',') {
        return Err(FileServeError::InvalidRange);
    }

    let (raw_start, raw_end) = raw.split_once('-').ok_or(FileServeError::InvalidRange)?;

    if raw_start.is_empty() {
        let suffix = raw_end
            .parse::<u64>()
            .map_err(|_| FileServeError::InvalidRange)?;
        if suffix == 0 {
            return Err(FileServeError::InvalidRange);
        }
        let start = file_size.saturating_sub(suffix);
        let end = file_size.saturating_sub(1);
        return Ok(ByteRange { start, end });
    }

    let start = raw_start
        .parse::<u64>()
        .map_err(|_| FileServeError::InvalidRange)?;

    let end = if raw_end.is_empty() {
        file_size.saturating_sub(1)
    } else {
        raw_end
            .parse::<u64>()
            .map_err(|_| FileServeError::InvalidRange)?
    };

    if start >= file_size || end >= file_size || start > end {
        return Err(FileServeError::InvalidRange);
    }

    Ok(ByteRange { start, end })
}

#[cfg(test)]
mod tests {
    use super::sanitize_relative_path;

    #[test]
    fn sanitize_prevents_traversal() {
        assert!(sanitize_relative_path("../etc/passwd").is_err());
        assert!(sanitize_relative_path("/../../abc").is_err());
        assert!(sanitize_relative_path("games/file.nsp").is_ok());
    }

    #[test]
    fn sanitize_rejects_parent_dir_components() {
        assert!(sanitize_relative_path("a/../b").is_err());
        assert!(sanitize_relative_path("..").is_err());
        assert!(sanitize_relative_path("a/../../b").is_err());
    }

    #[test]
    fn sanitize_accepts_valid_paths() {
        assert!(sanitize_relative_path("games/file.nsp").is_ok());
        assert!(sanitize_relative_path("subdir/nested/game.xci").is_ok());
        assert!(sanitize_relative_path("single.nsp").is_ok());
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_relative_path("").is_err());
        assert!(sanitize_relative_path("/").is_err());
    }
}
