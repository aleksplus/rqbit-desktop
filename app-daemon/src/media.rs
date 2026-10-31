//! Media serving for browser clients and external players: Range-capable
//! streaming of torrent files and per-torrent M3U8 playlists. These mirror
//! librqbit's own HTTP API handlers (`http_api::handlers::{streaming,
//! playlist}`) but live on the same port as the WS protocol, so the web
//! client can build same-origin media URLs. Reads block until the needed
//! piece is downloaded (`FileStream` is availability-aware), so a torrent
//! that is still downloading streams too.

use std::io::SeekFrom;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{self, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _};

use crate::server::RouterState;

/// The media routes, merged into the daemon router. State is the shared
/// [`RouterState`] so the handlers read the Api through the same swappable
/// slot the WS connections use.
pub fn router() -> axum::Router<RouterState> {
    axum::Router::new()
        .route("/torrents/{id}/stream/{file_id}", get(stream_file))
        .route(
            "/torrents/{id}/stream/{file_id}/{*filename}",
            get(stream_file),
        )
        .route("/torrents/{id}/playlist", get(torrent_playlist))
        .route("/torrents/by-hash/{hash}/artwork", get(torrent_artwork))
}

/// The handler answers both stream routes; the filename segment is
/// decorative (players use it to pick a download name) — only `file_id`
/// selects content, so the value can never escape the session's storage.
#[derive(serde::Deserialize)]
struct StreamPath {
    id: u64,
    file_id: usize,
    #[serde(rename = "filename")]
    _filename: Option<Arc<str>>,
}

async fn stream_file(
    State(state): State<RouterState>,
    Path(StreamPath { id, file_id, .. }): Path<StreamPath>,
    headers: HeaderMap,
) -> Result<Response, MediaError> {
    let api = state.0.get();
    let mut stream = api.api_stream(torrent_id(id), file_id).await?;

    let mut status = StatusCode::OK;
    let mut output_headers = HeaderMap::new();
    output_headers.insert(
        http::header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );
    if let Ok(mime) = api.torrent_file_mime_type(torrent_id(id), file_id) {
        output_headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static(mime));
    }

    let range = headers
        .get(http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_header);

    let body: Box<dyn AsyncRead + Send + Unpin> = match range {
        Some((start, end)) => {
            let len = stream.len();
            if start >= len || end.is_some_and(|end| end <= start || end > len) {
                return Err(MediaError::Status(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "bad range".to_string(),
                ));
            }
            status = StatusCode::PARTIAL_CONTENT;
            let end = end.unwrap_or(len);
            stream.seek(SeekFrom::Start(start)).await?;
            let to_take = end - start;
            output_headers.insert(http::header::CONTENT_LENGTH, content_length_value(to_take)?);
            output_headers.insert(
                http::header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end - 1, len))
                    .map_err(|e| MediaError::internal(e.to_string()))?,
            );
            Box::new(stream.take(to_take))
        }
        None => {
            output_headers.insert(
                http::header::CONTENT_LENGTH,
                content_length_value(stream.len())?,
            );
            Box::new(stream)
        }
    };

    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(body, 65536));
    Ok((status, (output_headers, body)).into_response())
}

async fn torrent_playlist(
    State(state): State<RouterState>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Result<Response, MediaError> {
    let host = host_header(&headers)?;
    let api = state.0.get();
    let details = api.api_torrent_details(torrent_id(id))?;
    let files = details.files.unwrap_or_default();
    let items = playlist_items(files.iter().map(|f| f.name.as_str()));

    let body = build_playlist_body(&host, id, &items);
    Ok((
        [
            (
                http::header::CONTENT_TYPE,
                "application/mpegurl; charset=utf-8",
            ),
            (
                http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"rqbit-playlist.m3u8\"",
            ),
        ],
        body,
    )
        .into_response())
}

fn torrent_id(id: u64) -> librqbit::api::TorrentIdOrHash {
    librqbit::api::TorrentIdOrHash::Id(id as usize)
}

/// Serve the resolved artwork bytes for a torrent, by info-hash. When the
/// artwork is advertised but not extracted yet (a preview frame), the
/// handler waits on the provider's worker for a bounded time so the first
/// UI request usually gets the image inline. 404 until anything is ready.
async fn torrent_artwork(
    State(state): State<RouterState>,
    Path(hash): Path<String>,
) -> Result<Response, MediaError> {
    let Some(artwork) = state.4.as_ref() else {
        return Err(MediaError::Status(
            StatusCode::NOT_FOUND,
            "no artwork provider".to_string(),
        ));
    };
    match artwork.request_bytes(&hash).await {
        Some((bytes, content_type)) => {
            let header = http::HeaderValue::from_static(content_type);
            Ok(([(http::header::CONTENT_TYPE, header)], bytes.to_vec()).into_response())
        }
        None => Err(MediaError::Status(
            StatusCode::NOT_FOUND,
            "no artwork".to_string(),
        )),
    }
}

fn host_header(headers: &HeaderMap) -> Result<String, MediaError> {
    headers
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| MediaError::Status(StatusCode::BAD_REQUEST, "missing Host header".into()))
}

/// The playable files of a torrent as `(file index, URL-encoded name)`,
/// sorted by name — the same selection (mime video/audio) and order
/// librqbit's playlist endpoint uses.
fn playlist_items<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(usize, String)> {
    let mut items = names
        .enumerate()
        .filter(|(_, name)| is_playable(name))
        .map(|(idx, name)| (idx, urlencoding::encode(name).into_owned()))
        .collect::<Vec<_>>();
    items.sort_by(|left, right| left.1.cmp(&right.1));
    items
}

fn is_playable(name: &str) -> bool {
    mime_guess::from_path(name)
        .first()
        .map(|mime| {
            mime.type_() == mime_guess::mime::VIDEO || mime.type_() == mime_guess::mime::AUDIO
        })
        .unwrap_or(false)
}

fn build_playlist_body(host: &str, torrent_id: u64, items: &[(usize, String)]) -> String {
    let body = items
        .iter()
        .map(|(file_idx, filename)| {
            format!("http://{host}/torrents/{torrent_id}/stream/{file_idx}/{filename}")
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    format!("#EXTM3U\r\n{body}") // https://en.wikipedia.org/wiki/M3U
}

/// Parses a single-range `Range: bytes=start-end` header value into an
/// exclusive end (`None` = to EOF). Malformed or suffix ranges (`bytes=-N`)
/// yield `None`, serving the whole file like librqbit's handler does.
fn parse_range_header(value: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = match end {
        "" => None,
        end => Some(end.parse::<u64>().ok()?.saturating_add(1)),
    };
    Some((start, end))
}

fn content_length_value(len: u64) -> Result<HeaderValue, MediaError> {
    HeaderValue::from_str(&len.to_string()).map_err(|e| MediaError::internal(e.to_string()))
}

/// Handler error: an Api failure mapped to its HTTP status, or a raw one.
enum MediaError {
    Api(librqbit::ApiError),
    Status(StatusCode, String),
}

impl MediaError {
    fn internal(msg: String) -> Self {
        Self::Status(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl From<librqbit::ApiError> for MediaError {
    fn from(e: librqbit::ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<std::io::Error> for MediaError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(e.to_string())
    }
}

impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        match self {
            MediaError::Api(e) => (e.status(), format!("{e}\n")).into_response(),
            MediaError::Status(status, msg) => (status, format!("{msg}\n")).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_parses_start_end() {
        assert_eq!(parse_range_header("bytes=0-99"), Some((0, Some(100))));
        assert_eq!(parse_range_header("bytes=100-"), Some((100, None)));
        assert_eq!(
            parse_range_header("bytes=18446744073709551614-"),
            Some((u64::MAX - 1, None))
        );
    }

    #[test]
    fn range_header_rejects_garbage() {
        // Suffix ranges are not supported (whole file is served instead).
        assert_eq!(parse_range_header("bytes=-50"), None);
        assert_eq!(parse_range_header("bytes=abc-"), None);
        assert_eq!(parse_range_header("bytes=1-2-3"), None);
        assert_eq!(parse_range_header("items=0-5"), None);
        assert_eq!(parse_range_header(""), None);
    }

    #[test]
    fn playlist_body_lists_stream_urls() {
        let items = vec![
            (1, "b.mkv".to_string()),
            (0, "a%20folder%2Fa.mkv".to_string()),
        ];
        assert_eq!(
            build_playlist_body("localhost:3080", 7, &items),
            "#EXTM3U\r\n\
             http://localhost:3080/torrents/7/stream/1/b.mkv\r\n\
             http://localhost:3080/torrents/7/stream/0/a%20folder%2Fa.mkv"
        );
    }

    #[test]
    fn playlist_items_keep_media_only_sorted_by_name() {
        let names = ["b.mp4", "cover.jpg", "a.mkv", "notes.txt", "track.flac"];
        assert_eq!(
            playlist_items(names.into_iter()),
            vec![
                (2, "a.mkv".to_string()),
                (0, "b.mp4".to_string()),
                (4, "track.flac".to_string()),
            ]
        );
    }

    #[test]
    fn playlist_items_url_encode_names() {
        assert_eq!(
            playlist_items(["some dir/video (2024).mp4"].into_iter()),
            vec![(0, "some%20dir%2Fvideo%20%282024%29.mp4".to_string())]
        );
    }
}
