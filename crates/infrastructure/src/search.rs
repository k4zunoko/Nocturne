use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use nocturne_core::{PortError, SearchPort};
use nocturne_domain::Song;
use serde_json::Value;

use crate::recover_lock;
use crate::yt_dlp::LocalYtDlpManager;

const SEARCH_RESULT_LIMIT: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSearchJob {
    pub job_id: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSearchFailure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
struct LocalSearchState {
    pending: VecDeque<PendingSearchJob>,
    fixtures: BTreeMap<String, Vec<Song>>,
    failures: BTreeMap<String, LocalSearchFailure>,
    yt_dlp: LocalYtDlpManager,
}

#[derive(Debug, Clone)]
pub struct LocalSearchRuntime {
    inner: Arc<Mutex<LocalSearchState>>,
}

impl Default for LocalSearchRuntime {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LocalSearchState {
                pending: VecDeque::new(),
                fixtures: BTreeMap::new(),
                failures: BTreeMap::new(),
                yt_dlp: LocalYtDlpManager::default(),
            })),
        }
    }
}

impl LocalSearchRuntime {
    #[must_use]
    pub fn with_yt_dlp(yt_dlp: LocalYtDlpManager) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LocalSearchState {
                pending: VecDeque::new(),
                fixtures: BTreeMap::new(),
                failures: BTreeMap::new(),
                yt_dlp,
            })),
        }
    }

    #[must_use]
    pub fn drain_pending(&self) -> Vec<PendingSearchJob> {
        let mut state = recover_lock(&self.inner);
        state.pending.drain(..).collect()
    }

    pub fn set_fixture(&self, query: impl Into<String>, results: Vec<Song>) {
        recover_lock(&self.inner)
            .fixtures
            .insert(normalize_query(&query.into()), results);
    }

    pub fn set_failure(
        &self,
        query: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        recover_lock(&self.inner).failures.insert(
            normalize_query(&query.into()),
            LocalSearchFailure {
                code: code.into(),
                message: message.into(),
            },
        );
    }

    pub fn resolve(&self, query: &str) -> Result<Vec<Song>, LocalSearchFailure> {
        let query = query.trim();
        let normalized_query = normalize_query(query);
        let yt_dlp = {
            let state = recover_lock(&self.inner);

            if let Some(failure) = state.failures.get(&normalized_query) {
                return Err(failure.clone());
            }

            if let Some(results) = state.fixtures.get(&normalized_query) {
                return Ok(results.clone());
            }

            state.yt_dlp.clone()
        };

        search_with_yt_dlp(query, &yt_dlp)
    }

    fn enqueue(&self, job_id: &str, query: &str) {
        recover_lock(&self.inner)
            .pending
            .push_back(PendingSearchJob {
                job_id: job_id.to_owned(),
                query: query.to_owned(),
            });
    }
}

#[derive(Debug, Clone)]
pub struct LocalSearchAdapter {
    runtime: LocalSearchRuntime,
}

impl LocalSearchAdapter {
    #[must_use]
    pub fn new(runtime: LocalSearchRuntime) -> Self {
        Self { runtime }
    }
}

impl SearchPort for LocalSearchAdapter {
    fn start_search(&mut self, job_id: &str, query: &str) -> Result<(), PortError> {
        self.runtime.enqueue(job_id, query);
        Ok(())
    }
}

fn search_with_yt_dlp(
    query: &str,
    yt_dlp: &LocalYtDlpManager,
) -> Result<Vec<Song>, LocalSearchFailure> {
    let search_term = format!("ytsearch{SEARCH_RESULT_LIMIT}:{query}");
    let output = yt_dlp
        .execute([
            "--dump-single-json",
            "--flat-playlist",
            "--no-warnings",
            "--quiet",
            "--skip-download",
            &search_term,
        ])
        .map_err(map_spawn_error)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !stderr.is_empty() {
            eprintln!("yt-dlp search failed: {stderr}");
        }
        return Err(LocalSearchFailure {
            code: String::from("yt_dlp_failed"),
            message: String::from("Search provider failed to return results."),
        });
    }

    parse_yt_dlp_search_output(&output.stdout)
}

fn map_spawn_error(error: std::io::Error) -> LocalSearchFailure {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "yt_dlp_missing"
    } else {
        "yt_dlp_spawn_failed"
    };

    LocalSearchFailure {
        code: String::from(code),
        message: String::from("Search provider is unavailable on this backend."),
    }
}

fn parse_yt_dlp_search_output(payload: &[u8]) -> Result<Vec<Song>, LocalSearchFailure> {
    let root: Value = serde_json::from_slice(payload).map_err(|_error| LocalSearchFailure {
        code: String::from("yt_dlp_invalid_json"),
        message: String::from("Search provider returned unreadable data."),
    })?;

    let entries = root
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("yt-dlp search response did not contain an entries array"),
        })?;

    let results = entries
        .iter()
        .map(song_from_yt_dlp_entry)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(results)
}

fn song_from_yt_dlp_entry(entry: &Value) -> Result<Song, LocalSearchFailure> {
    let video_id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("Search provider returned incomplete result data."),
        })?;
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(video_id);
    let channel_name = entry
        .get("channel")
        .or_else(|| entry.get("uploader"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("Unknown channel");
    let source_url = entry
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
        .map(str::to_owned)
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={video_id}"));

    Ok(song(
        &format!("youtube:{video_id}"),
        title,
        channel_name,
        duration_ms(entry.get("duration")),
        &source_url,
    ))
}

fn duration_ms(value: Option<&Value>) -> u64 {
    let Some(seconds) = value.and_then(Value::as_f64) else {
        return 0;
    };

    if !seconds.is_finite() || seconds.is_sign_negative() {
        return 0;
    }

    let millis = seconds * 1_000.0;
    if millis >= u64::MAX as f64 {
        u64::MAX
    } else {
        millis.round() as u64
    }
}

fn song(id: &str, title: &str, channel_name: &str, duration_ms: u64, source_url: &str) -> Song {
    Song {
        id: id.to_owned(),
        title: title.to_owned(),
        channel_name: channel_name.to_owned(),
        duration_ms,
        source_url: source_url.to_owned(),
    }
}

fn normalize_query(query: &str) -> String {
    query.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_jobs_are_drained_and_resolved() {
        let runtime = LocalSearchRuntime::default();
        runtime.set_fixture(
            "utada traveling",
            vec![
                song(
                    "song_utada_traveling_1",
                    "traveling",
                    "宇多田ヒカル",
                    301_000,
                    "https://youtube.com/watch?v=utada-traveling-1",
                ),
                song(
                    "song_utada_traveling_2",
                    "Traveling (Live)",
                    "宇多田ヒカル",
                    322_000,
                    "https://youtube.com/watch?v=utada-traveling-2",
                ),
            ],
        );
        let mut adapter = LocalSearchAdapter::new(runtime.clone());

        adapter.start_search("job_0001", "utada traveling").unwrap();
        let pending = runtime.drain_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job_id, "job_0001");

        let results = runtime.resolve(&pending[0].query).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn failures_can_be_configured() {
        let runtime = LocalSearchRuntime::default();
        runtime.set_failure("broken query", "yt_dlp_missing", "yt-dlp not installed");

        let failure = runtime.resolve("broken query").unwrap_err();
        assert_eq!(failure.code, "yt_dlp_missing");
    }

    #[test]
    fn yt_dlp_json_is_mapped_into_song_results() {
        let payload = br#"{
            "entries": [
                {
                    "id": "tuyZ9f6mHZk",
                    "title": "Hikaru Utada - traveling",
                    "channel": "Hikaru Utada",
                    "duration": 295.0,
                    "url": "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
                },
                {
                    "id": "OCWu_pgHUcQ",
                    "title": "Hikaru Utada - traveling (Live)",
                    "uploader": "Dennis Holierhoek",
                    "duration": 315.0
                }
            ]
        }"#;

        let results = parse_yt_dlp_search_output(payload).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "youtube:tuyZ9f6mHZk");
        assert_eq!(results[0].channel_name, "Hikaru Utada");
        assert_eq!(results[0].duration_ms, 295_000);
        assert_eq!(
            results[1].source_url,
            "https://www.youtube.com/watch?v=OCWu_pgHUcQ"
        );
    }
}
