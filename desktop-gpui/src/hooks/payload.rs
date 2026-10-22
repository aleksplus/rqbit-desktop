//! JSON payloads sent to (and received from) hooks.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rqbit_ui::ui::file_table::FileRow;

use super::HookEvent;

/// Identity of the torrent an event refers to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TorrentInfo {
    pub id: Option<usize>,
    pub name: Option<String>,
    pub info_hash: String,
    pub output_folder: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wrap an event payload in the common envelope (`event` +
/// `timestamp_unix_secs` + flattened payload fields).
pub(crate) fn envelope(event: HookEvent, payload: impl Serialize) -> serde_json::Value {
    #[derive(Serialize)]
    struct Envelope<'a, T: Serialize> {
        event: &'a str,
        timestamp_unix_secs: u64,
        #[serde(flatten)]
        payload: T,
    }

    serde_json::to_value(Envelope {
        event: event.as_str(),
        timestamp_unix_secs: now_unix(),
        payload,
    })
    .unwrap_or_else(|_| serde_json::json!({ "event": event.as_str() }))
}

/// Payload for [`HookEvent::TorrentAdded`].
#[derive(Serialize)]
pub struct TorrentAddedPayload {
    pub torrent: TorrentInfo,
    pub files_total: usize,
    pub files_selected: usize,
    pub selected_indices: Option<Vec<usize>>,
}

/// Payload for [`HookEvent::TorrentDeleted`].
#[derive(Serialize)]
pub struct TorrentDeletedPayload {
    pub torrent: TorrentInfo,
    /// Whether the downloaded files were removed from disk (delete vs forget).
    pub delete_files: bool,
}

/// Payload for [`HookEvent::TorrentCompleted`].
#[derive(Serialize)]
pub struct TorrentCompletedPayload {
    pub torrent: TorrentInfo,
}

/// One entry of the `files` array in a `file_selection` request.
#[derive(Serialize)]
pub(crate) struct FileSelectionFile<'a> {
    index: usize,
    name: &'a str,
    length: u64,
}

/// Payload for the [`HookEvent::FileSelection`] request. The torrent is not
/// part of the session yet, so its `id` is `None`.
#[derive(Serialize)]
pub(crate) struct FileSelectionRequest<'a> {
    pub torrent: TorrentInfo,
    pub files: Vec<FileSelectionFile<'a>>,
}

impl<'a> FileSelectionRequest<'a> {
    pub fn new(torrent: TorrentInfo, files: &'a [FileRow]) -> Self {
        Self {
            torrent,
            files: files
                .iter()
                .map(|f| FileSelectionFile {
                    index: f.file_index,
                    name: &f.name,
                    length: f.length,
                })
                .collect(),
        }
    }
}

/// Response to a `file_selection` request. All fields are optional and
/// applied in declaration order: `include`, `exclude`,
/// `include_patterns`, `exclude_patterns`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileSelectionResponse {
    /// Indices of files to select; unlisted files are deselected.
    pub include: Option<Vec<usize>>,
    /// Indices of files to deselect.
    pub exclude: Option<Vec<usize>>,
    /// Wildcard patterns (`*`, `?`) of file names to keep; non-matching
    /// files are deselected.
    pub include_patterns: Option<Vec<String>>,
    /// Wildcard patterns (`*`, `?`) of file names to deselect.
    pub exclude_patterns: Option<Vec<String>>,
}

impl FileSelectionResponse {
    /// Whether applying this response to `files` would leave nothing
    /// selected.
    pub fn selects_nothing(&self, files: &[FileRow]) -> bool {
        self.compute_selection(files).iter().all(|&b| !b)
    }

    /// Apply this response to `files` in place. Returns `true` if any row's
    /// `included` flag changed.
    pub fn apply(&self, files: &mut [FileRow]) -> bool {
        let selected = self.compute_selection(files);
        let mut changed = false;
        for (row, sel) in files.iter_mut().zip(selected) {
            if row.included != sel {
                row.included = sel;
                changed = true;
            }
        }
        changed
    }

    /// Target `included` flag per row. The base is the rows' *current*
    /// selection, so successive responses compose as a pipeline:
    /// `include` is an absolute whitelist, `exclude` and the pattern
    /// lists only remove from what is currently selected.
    fn compute_selection(&self, files: &[FileRow]) -> Vec<bool> {
        let mut selected: Vec<bool> = files.iter().map(|f| f.included).collect();
        if let Some(include) = &self.include {
            let set: HashSet<usize> = include.iter().copied().collect();
            for (i, s) in selected.iter_mut().enumerate() {
                *s = set.contains(&i);
            }
        }
        if let Some(exclude) = &self.exclude {
            for &ix in exclude {
                if let Some(s) = selected.get_mut(ix) {
                    *s = false;
                }
            }
        }
        if let Some(patterns) = &self.include_patterns {
            for (row, s) in files.iter().zip(selected.iter_mut()) {
                *s = *s && patterns.iter().any(|p| wildcard_match(p, &row.name));
            }
        }
        if let Some(patterns) = &self.exclude_patterns {
            for (row, s) in files.iter().zip(selected.iter_mut()) {
                if patterns.iter().any(|p| wildcard_match(p, &row.name)) {
                    *s = false;
                }
            }
        }
        selected
    }
}

/// Match `name` against a wildcard pattern supporting `*` (any sequence)
/// and `?` (any single character), case-insensitively.
pub(crate) fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    wildcard_match_chars(&p, &n)
}

fn wildcard_match_chars(p: &[char], n: &[char]) -> bool {
    // Iterative two-pointer matcher with backtracking on '*'.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ix: usize, name: &str, included: bool) -> FileRow {
        FileRow {
            file_index: ix,
            name: name.to_string(),
            length: 1,
            included,
            exists: false,
            progress: 0,
            custom: Default::default(),
        }
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.mkv", "movie.mkv"));
        assert!(wildcard_match("*.MKV", "movie.MkV"));
        assert!(!wildcard_match("*.mkv", "movie.mp4"));
        assert!(wildcard_match("?.txt", "a.txt"));
        assert!(!wildcard_match("?.txt", "ab.txt"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("season?/*ep??.mkv", "Season1/showep01.mkv"));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn apply_include_exclude() {
        let mut files = vec![row(0, "a", true), row(1, "b", true), row(2, "c", true)];
        let resp = FileSelectionResponse {
            include: Some(vec![0, 2]),
            ..Default::default()
        };
        assert!(resp.apply(&mut files));
        assert_eq!(
            files.iter().map(|f| f.included).collect::<Vec<_>>(),
            [true, false, true]
        );
        // Idempotent.
        assert!(!resp.apply(&mut files));

        let resp = FileSelectionResponse {
            exclude: Some(vec![0]),
            ..Default::default()
        };
        assert!(resp.apply(&mut files));
        assert_eq!(
            files.iter().map(|f| f.included).collect::<Vec<_>>(),
            [false, false, true]
        );
    }

    #[test]
    fn apply_patterns() {
        let mut files = vec![
            row(0, "movie.mkv", true),
            row(1, "sample/sample.mkv", true),
            row(2, "info.nfo", true),
            row(3, "readme.txt", true),
        ];
        let resp = FileSelectionResponse {
            include_patterns: Some(vec!["*.mkv".into()]),
            exclude_patterns: Some(vec!["sample/*".into()]),
            ..Default::default()
        };
        assert!(resp.apply(&mut files));
        assert_eq!(
            files.iter().map(|f| f.included).collect::<Vec<_>>(),
            [true, false, false, false]
        );
    }

    #[test]
    fn empty_response_keeps_selection() {
        let mut files = vec![row(0, "a", true), row(1, "b", false)];
        let resp = FileSelectionResponse::default();
        assert!(!resp.apply(&mut files));
        assert_eq!(
            files.iter().map(|f| f.included).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn selects_nothing_detection() {
        let files = vec![row(0, "a", true), row(1, "b", true)];
        let resp = FileSelectionResponse {
            exclude: Some(vec![0, 1]),
            ..Default::default()
        };
        assert!(resp.selects_nothing(&files));
        let resp = FileSelectionResponse {
            include: Some(vec![1]),
            exclude: Some(vec![1]),
            ..Default::default()
        };
        assert!(resp.selects_nothing(&files));
        assert!(!FileSelectionResponse::default().selects_nothing(&files));
        let resp = FileSelectionResponse {
            exclude_patterns: Some(vec!["*".into()]),
            ..Default::default()
        };
        assert!(resp.selects_nothing(&files));
    }

    #[test]
    fn response_deserializes_partial_json() {
        let resp: FileSelectionResponse = serde_json::from_str(r#"{"exclude": [3]}"#).unwrap();
        assert_eq!(resp.exclude, Some(vec![3]));
        assert!(resp.include.is_none());
    }
}
