use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use nocturne_core::{PortError, SearchPort};
use nocturne_domain::Song;
use serde_json::Value;

use crate::recover_lock;
use crate::yt_dlp::LocalYtDlpManager;

const SEARCH_RESULT_LIMIT: usize = 5;
const YOUTUBE_CANONICAL_HOST: &str = "www.youtube.com";
const YOUTUBE_WATCH_PATH: &str = "/watch";
const YOUTU_BE_HOST: &str = "youtu.be";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSearchJob {
    pub job_id: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingYoutubeImportJob {
    pub job_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSearchFailure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoutubeImportResolution {
    pub canonical_url: String,
    pub songs: Vec<Song>,
    pub total_count: u64,
    pub failed_count: u64,
}

#[derive(Debug, Clone)]
struct LocalSearchState {
    pending: VecDeque<PendingSearchJob>,
    pending_youtube_imports: VecDeque<PendingYoutubeImportJob>,
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
                pending_youtube_imports: VecDeque::new(),
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
                pending_youtube_imports: VecDeque::new(),
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

    #[must_use]
    pub fn drain_pending_youtube_imports(&self) -> Vec<PendingYoutubeImportJob> {
        let mut state = recover_lock(&self.inner);
        state.pending_youtube_imports.drain(..).collect()
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

    pub fn resolve_youtube_url(&self, url: &str) -> Result<YoutubeImportResolution, LocalSearchFailure> {
        let url = url.trim();
        let normalized_url = normalize_query(url);
        let yt_dlp = {
            let state = recover_lock(&self.inner);

            if let Some(failure) = state.failures.get(&normalized_url) {
                return Err(failure.clone());
            }

            if let Some(results) = state.fixtures.get(&normalized_url) {
                return Ok(YoutubeImportResolution {
                    canonical_url: url.to_owned(),
                    total_count: u64::try_from(results.len()).unwrap_or(u64::MAX),
                    failed_count: 0,
                    songs: results.clone(),
                });
            }

            state.yt_dlp.clone()
        };

        resolve_youtube_url_with_yt_dlp(url, &yt_dlp)
    }

    fn enqueue(&self, job_id: &str, query: &str) {
        recover_lock(&self.inner)
            .pending
            .push_back(PendingSearchJob {
                job_id: job_id.to_owned(),
                query: query.to_owned(),
            });
    }

    pub fn enqueue_youtube_import(&self, job_id: &str, url: &str) {
        recover_lock(&self.inner)
            .pending_youtube_imports
            .push_back(PendingYoutubeImportJob {
                job_id: job_id.to_owned(),
                url: url.to_owned(),
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

    fn start_youtube_import(&mut self, job_id: &str, url: &str) -> Result<(), PortError> {
        self.runtime.enqueue_youtube_import(job_id, url);
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
        .map_err(map_search_spawn_error)?;

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

fn resolve_youtube_url_with_yt_dlp(
    url: &str,
    yt_dlp: &LocalYtDlpManager,
) -> Result<YoutubeImportResolution, LocalSearchFailure> {
    let resolved = parse_supported_youtube_url(url)?;
    let output = yt_dlp
        .execute([
            "--dump-single-json",
            "--no-warnings",
            "--quiet",
            "--skip-download",
            resolved.canonical_url.as_str(),
        ])
        .map_err(map_youtube_import_spawn_error)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !stderr.is_empty() {
            eprintln!("yt-dlp youtube import failed: {stderr}");
        }
        return Err(LocalSearchFailure {
            code: String::from("yt_dlp_failed"),
            message: String::from("YouTube URL resolution failed to return metadata."),
        });
    }

    parse_yt_dlp_youtube_output(&output.stdout, &resolved.canonical_url)
}

fn map_search_spawn_error(error: std::io::Error) -> LocalSearchFailure {
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

fn map_youtube_import_spawn_error(error: std::io::Error) -> LocalSearchFailure {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "yt_dlp_missing"
    } else {
        "yt_dlp_spawn_failed"
    };

    LocalSearchFailure {
        code: String::from(code),
        message: String::from("YouTube URL resolution is unavailable on this backend."),
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

fn parse_yt_dlp_youtube_output(
    payload: &[u8],
    canonical_url: &str,
) -> Result<YoutubeImportResolution, LocalSearchFailure> {
    let root: Value = serde_json::from_slice(payload).map_err(|_error| LocalSearchFailure {
        code: String::from("yt_dlp_invalid_json"),
        message: String::from("Search provider returned unreadable data."),
    })?;

    if let Some(entries) = root.get("entries").and_then(Value::as_array) {
        let playlist_count = root
            .get("playlist_count")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| u64::try_from(entries.len()).unwrap_or(u64::MAX));
        let songs = entries
            .iter()
            .filter_map(|entry| song_from_yt_dlp_entry(entry).ok())
            .collect::<Vec<_>>();
        let queued_count = u64::try_from(songs.len()).unwrap_or(u64::MAX);
        let failed_count = playlist_count.saturating_sub(queued_count);

        if songs.is_empty() {
            return Err(LocalSearchFailure {
                code: String::from("yt_dlp_failed"),
                message: String::from("The backend failed to resolve this YouTube playlist."),
            });
        }

        return Ok(YoutubeImportResolution {
            canonical_url: canonical_url.to_owned(),
            songs,
            total_count: playlist_count,
            failed_count,
        });
    }

    Ok(YoutubeImportResolution {
        canonical_url: canonical_url.to_owned(),
        songs: vec![song_from_yt_dlp_video(&root)?],
        total_count: 1,
        failed_count: 0,
    })
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

fn song_from_yt_dlp_video(entry: &Value) -> Result<Song, LocalSearchFailure> {
    let video_id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .filter(|value| valid_video_id(value))
        .ok_or_else(|| LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("YouTube URL resolution returned incomplete result data."),
        })?;
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("YouTube URL resolution did not return a title."),
        })?;
    let channel_name = entry
        .get("channel")
        .or_else(|| entry.get("uploader"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("YouTube URL resolution did not return a channel name."),
        })?;
    let duration_ms = duration_ms(entry.get("duration"));
    if duration_ms == 0 {
        return Err(LocalSearchFailure {
            code: String::from("yt_dlp_invalid_response"),
            message: String::from("YouTube URL resolution did not return a duration."),
        });
    }

    Ok(song(
        &format!("youtube:{video_id}"),
        title,
        channel_name,
        duration_ms,
        &canonical_youtube_watch_url(video_id),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedYoutubeVideoUrl {
    canonical_url: String,
}

#[must_use]
pub fn canonicalize_supported_youtube_url(url: &str) -> Result<String, LocalSearchFailure> {
    parse_supported_youtube_url(url).map(|resolved| resolved.canonical_url)
}

fn parse_supported_youtube_url(url: &str) -> Result<ResolvedYoutubeVideoUrl, LocalSearchFailure> {
    let parsed = reqwest::Url::parse(url).map_err(|_| LocalSearchFailure {
        code: String::from("youtube_url_invalid"),
        message: String::from("Input is not a valid YouTube URL."),
    })?;

    let Some(host) = parsed.host_str().map(|host| host.to_ascii_lowercase()) else {
        return Err(LocalSearchFailure {
            code: String::from("youtube_url_invalid"),
            message: String::from("Input is not a valid YouTube URL."),
        });
    };

    let canonical_url = if host == YOUTU_BE_HOST {
        let video_id = parsed.path().trim_start_matches('/');
        if !valid_video_id(video_id) {
            return Err(LocalSearchFailure {
                code: String::from("youtube_url_invalid"),
                message: String::from("Input is not a valid YouTube URL."),
            });
        }
        canonical_youtube_watch_url(video_id)
    } else if matches!(host.as_str(), "youtube.com" | "www.youtube.com") {
        if parsed.path() == "/playlist" {
            let playlist_id = parsed
                .query_pairs()
                .find_map(|(key, value)| (key == "list").then_some(value.into_owned()))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| LocalSearchFailure {
                    code: String::from("youtube_url_invalid"),
                    message: String::from("Input is not a valid YouTube URL."),
                })?;
            format!("https://{YOUTUBE_CANONICAL_HOST}/playlist?list={playlist_id}")
        } else {
            if parsed.path() != YOUTUBE_WATCH_PATH {
                return Err(LocalSearchFailure {
                    code: String::from("youtube_url_unsupported"),
                    message: String::from("This YouTube URL format is not supported yet."),
                });
            }

            let video = parsed
                .query_pairs()
                .find_map(|(key, value)| (key == "v").then_some(value.into_owned()))
                .ok_or_else(|| LocalSearchFailure {
                    code: String::from("youtube_url_invalid"),
                    message: String::from("Input is not a valid YouTube URL."),
                })?;
            if !valid_video_id(&video) {
                return Err(LocalSearchFailure {
                    code: String::from("youtube_url_invalid"),
                    message: String::from("Input is not a valid YouTube URL."),
                });
            }
            canonical_youtube_watch_url(&video)
        }
    } else {
        return Err(LocalSearchFailure {
            code: String::from("youtube_url_invalid"),
            message: String::from("Input is not a valid YouTube URL."),
        });
    };

    Ok(ResolvedYoutubeVideoUrl {
        canonical_url,
    })
}

fn canonical_youtube_watch_url(video_id: &str) -> String {
    format!("https://{YOUTUBE_CANONICAL_HOST}{YOUTUBE_WATCH_PATH}?v={video_id}")
}

fn valid_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
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

    #[test]
    fn youtube_watch_url_is_normalized_to_canonical_song() {
        let payload = br#"{
            "id": "tuyZ9f6mHZk",
            "title": "Hikaru Utada - traveling",
            "channel": "Hikaru Utada",
            "duration": 295.0
        }"#;

        let result = parse_yt_dlp_youtube_output(
            payload,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk",
        )
        .unwrap();

        assert_eq!(result.songs[0].id, "youtube:tuyZ9f6mHZk");
        assert_eq!(result.songs[0].channel_name, "Hikaru Utada");
        assert_eq!(result.songs[0].duration_ms, 295_000);
        assert_eq!(
            result.songs[0].source_url,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
        );
        assert_eq!(result.total_count, 1);
        assert_eq!(result.failed_count, 0);
    }

    #[test]
    fn supported_youtube_url_parser_accepts_watch_and_short_urls() {
        assert_eq!(
            parse_supported_youtube_url("https://www.youtube.com/watch?v=tuyZ9f6mHZk")
                .unwrap()
                .canonical_url,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
        );
        assert_eq!(
            parse_supported_youtube_url("https://youtu.be/tuyZ9f6mHZk")
                .unwrap()
                .canonical_url,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
        );
        assert_eq!(
            parse_supported_youtube_url("https://youtu.be/tuyZ9f6mHZk?list=PL1234567890")
                .unwrap()
                .canonical_url,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
        );
    }

    #[test]
    fn supported_youtube_url_parser_rejects_unsupported_formats() {
        assert_eq!(
            parse_supported_youtube_url(
                "https://www.youtube.com/watch?v=tuyZ9f6mHZk&list=PL1234567890",
            )
            .unwrap()
            .canonical_url,
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
        );

        assert_eq!(
            parse_supported_youtube_url("https://www.youtube.com/playlist?list=PL1234567890")
                .unwrap()
                .canonical_url,
            "https://www.youtube.com/playlist?list=PL1234567890"
        );

        let shorts_error =
            parse_supported_youtube_url("https://www.youtube.com/shorts/tuyZ9f6mHZk").unwrap_err();
        assert_eq!(shorts_error.code, "youtube_url_unsupported");
    }

    #[test]
    fn youtube_import_failures_use_import_specific_messages() {
        let failure = map_search_spawn_error(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing yt-dlp",
        ));

        assert_eq!(failure.code, "yt_dlp_missing");
        assert_eq!(
            failure.message,
            "Search provider is unavailable on this backend."
        );

        let import_failure = map_youtube_import_spawn_error(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing yt-dlp",
        ));

        assert_eq!(import_failure.code, "yt_dlp_missing");
        assert_eq!(
            import_failure.message,
            "YouTube URL resolution is unavailable on this backend."
        );
    }
}
