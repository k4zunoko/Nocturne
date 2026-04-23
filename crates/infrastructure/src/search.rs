use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use nocturne_core::{PortError, SearchPort};
use nocturne_domain::Song;

use crate::recover_lock;

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
                fixtures: default_fixtures(),
                failures: BTreeMap::new(),
            })),
        }
    }
}

impl LocalSearchRuntime {
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
        let state = recover_lock(&self.inner);
        let query = normalize_query(query);

        if let Some(failure) = state.failures.get(&query) {
            return Err(failure.clone());
        }

        if let Some(results) = state.fixtures.get(&query) {
            return Ok(results.clone());
        }

        Ok(generate_results(query.as_str()))
    }

    fn enqueue(&self, job_id: &str, query: &str) {
        recover_lock(&self.inner).pending.push_back(PendingSearchJob {
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

fn default_fixtures() -> BTreeMap<String, Vec<Song>> {
    let mut fixtures = BTreeMap::new();
    fixtures.insert(
        normalize_query("utada traveling"),
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
    fixtures.insert(
        normalize_query("yoasobi idol"),
        vec![
            song(
                "song_yoasobi_idol_1",
                "アイドル",
                "YOASOBI",
                214_000,
                "https://youtube.com/watch?v=yoasobi-idol-1",
            ),
            song(
                "song_yoasobi_idol_2",
                "Idol (Instrumental)",
                "YOASOBI",
                214_000,
                "https://youtube.com/watch?v=yoasobi-idol-2",
            ),
        ],
    );
    fixtures
}

fn generate_results(query: &str) -> Vec<Song> {
    let slug = slugify(query);

    (1_u64..=3)
        .map(|index| {
            song(
                &format!("song_{slug}_{index}"),
                &format!("{query} / Stub Result {index}"),
                "Nocturne Stub Search",
                180_000 + (index * 15_000),
                &format!("https://youtube.com/watch?v={slug}-{index}"),
            )
        })
        .collect()
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

fn slugify(query: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "stub-search".to_owned()
    } else {
        slug.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_jobs_are_drained_and_resolved() {
        let runtime = LocalSearchRuntime::default();
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
}
