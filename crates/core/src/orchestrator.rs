use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use nocturne_domain::{PlaybackState, PlaybackStatus, QueueItem, QueueItemStatus, Song};

use crate::models::{
    BackendState, CommandReceipt, CoreEvent, CoreEventEnvelope, CoreEventKind, CoreSnapshot,
    PlaybackPositionUpdatedEvent, PlaybackStateChangedEvent, PlaybackTrackChangedEvent,
    QueueUpdateReason, QueueUpdatedEvent, SearchJobCompletedEvent, SearchJobFailedEvent,
    SearchJobRecord, SearchJobStatus, SearchResultsRecord, SystemErrorEvent,
    SystemErrorSeverity,
};
use crate::ports::{
    ClockPort, EventPublisherPort, IdGeneratorPort, IdKind, PlaybackPort, PortError, SearchPort,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    Validation {
        code: &'static str,
        message: String,
    },
    NotFound {
        kind: &'static str,
        id: String,
    },
    Conflict {
        code: &'static str,
        message: String,
    },
    Port {
        code: String,
        message: String,
    },
}

impl CoreError {
    #[must_use]
    pub fn validation(code: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }
}

impl Display for CoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { code, message } => write!(f, "validation error ({code}): {message}"),
            Self::NotFound { kind, id } => write!(f, "{kind} not found: {id}"),
            Self::Conflict { code, message } => write!(f, "conflict ({code}): {message}"),
            Self::Port { code, message } => write!(f, "port error ({code}): {message}"),
        }
    }
}

impl Error for CoreError {}

impl From<PortError> for CoreError {
    fn from(value: PortError) -> Self {
        Self::Port {
            code: value.code().to_owned(),
            message: value.message().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorState {
    backend: BackendState,
    playback: PlaybackState,
    queue: Vec<QueueItem>,
    search_jobs: Vec<SearchJobRecord>,
    search_results: BTreeMap<String, Vec<Song>>,
}

impl Default for OrchestratorState {
    fn default() -> Self {
        Self {
            backend: BackendState {
                ready: true,
                version: None,
            },
            playback: PlaybackState {
                state: PlaybackStatus::Stopped,
                position_ms: 0,
                current_queue_item_id: None,
            },
            queue: Vec::new(),
            search_jobs: Vec::new(),
            search_results: BTreeMap::new(),
        }
    }
}

pub struct Orchestrator<C, I, E, P, S> {
    clock: C,
    ids: I,
    events: E,
    playback: P,
    search: S,
    state: OrchestratorState,
}

impl<C, I, E, P, S> Orchestrator<C, I, E, P, S>
where
    C: ClockPort,
    I: IdGeneratorPort,
    E: EventPublisherPort,
    P: PlaybackPort,
    S: SearchPort,
{
    #[must_use]
    pub fn new(clock: C, ids: I, events: E, playback: P, search: S) -> Self {
        Self {
            clock,
            ids,
            events,
            playback,
            search,
            state: OrchestratorState::default(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &OrchestratorState {
        &self.state
    }

    pub fn set_backend_ready(&mut self, ready: bool) {
        self.state.backend.ready = ready;
    }

    pub fn set_backend_version(&mut self, version: Option<impl Into<String>>) {
        self.state.backend.version = version.map(Into::into);
    }

    pub fn snapshot(&mut self) -> CoreSnapshot {
        CoreSnapshot {
            backend: self.state.backend.clone(),
            playback: self.state.playback.clone(),
            current_song: self.current_song().cloned(),
            queue: self.state.queue.clone(),
            search_jobs: self.state.search_jobs.clone(),
            snapshot_id: self.ids.next_id(IdKind::Snapshot),
            timestamp: self.clock.now(),
        }
    }

    #[must_use]
    pub fn queue(&self) -> &[QueueItem] {
        &self.state.queue
    }

    #[must_use]
    pub fn search_jobs(&self) -> &[SearchJobRecord] {
        &self.state.search_jobs
    }

    #[must_use]
    pub fn search_results(&self, job_id: &str) -> Option<SearchResultsRecord> {
        let job = self
            .state
            .search_jobs
            .iter()
            .find(|job| job.job_id == job_id)?
            .clone();
        let results = self.state.search_results.get(job_id)?.clone();

        Some(SearchResultsRecord { job, results })
    }

    pub fn submit_search(&mut self, query: impl AsRef<str>) -> Result<CommandReceipt, CoreError> {
        let query = query.as_ref().trim();
        if query.is_empty() {
            return Err(CoreError::validation(
                "query_empty",
                "search query must not be empty",
            ));
        }

        let job_id = self.ids.next_id(IdKind::SearchJob);
        self.search.start_search(&job_id, query)?;

        let summary = SearchJobRecord {
            job_id: job_id.clone(),
            status: SearchJobStatus::Running,
            query: query.to_owned(),
            created_at: self.clock.now(),
            completed_at: None,
            result_count: None,
        };
        self.state.search_jobs.push(summary.clone());
        self.publish(CoreEventKind::SearchJobStarted, CoreEvent::SearchJobStarted(summary))?;

        self.command_accepted(Some(job_id), None)
    }

    pub fn complete_search(
        &mut self,
        job_id: &str,
        results: Vec<Song>,
    ) -> Result<(), CoreError> {
        let completed_at = self.clock.now();
        let result_count = u64::try_from(results.len()).map_err(|_| {
            CoreError::conflict("result_count_overflow", "search result count exceeds u64")
        })?;

        let job = {
            let job = self.find_search_job_mut(job_id)?;
            job.status = SearchJobStatus::Completed;
            job.completed_at = Some(completed_at);
            job.result_count = Some(result_count);
            job.clone()
        };

        self.state
            .search_results
            .insert(job_id.to_owned(), results.clone());

        self.publish(
            CoreEventKind::SearchJobCompleted,
            CoreEvent::SearchJobCompleted(SearchJobCompletedEvent {
                job: job.clone(),
                results,
            }),
        )
    }

    pub fn fail_search(
        &mut self,
        job_id: &str,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<(), CoreError> {
        let code = code.into();
        let message = message.into();
        let completed_at = self.clock.now();

        {
            let job = self.find_search_job_mut(job_id)?;
            if job.status != SearchJobStatus::Running {
                return Err(CoreError::conflict(
                    "search_job_not_running",
                    "cannot fail a search job that is no longer running",
                ));
            }
            job.status = SearchJobStatus::Failed;
            job.completed_at = Some(completed_at);
            job.result_count = None;
        }

        self.publish(
            CoreEventKind::SearchJobFailed,
            CoreEvent::SearchJobFailed(SearchJobFailedEvent {
                job_id: job_id.to_owned(),
                code,
                message,
            }),
        )
    }

    pub fn enqueue_song_by_id(&mut self, song_id: &str) -> Result<CommandReceipt, CoreError> {
        let song = self
            .find_song(song_id)
            .cloned()
            .ok_or_else(|| CoreError::not_found("song", song_id))?;

        self.enqueue_song(song)
    }

    pub fn enqueue_song(&mut self, song: Song) -> Result<CommandReceipt, CoreError> {
        let item = QueueItem {
            id: self.ids.next_id(IdKind::QueueItem),
            song,
            added_at: self.clock.now(),
            status: QueueItemStatus::Queued,
        };
        let queue_item_id = item.id.clone();
        self.state.queue.push(item);
        self.publish_queue_updated(QueueUpdateReason::Add)?;

        self.command_accepted(None, Some(queue_item_id))
    }

    pub fn remove_queue_item(&mut self, queue_item_id: &str) -> Result<CommandReceipt, CoreError> {
        let removed_index = self
            .state
            .queue
            .iter()
            .position(|item| item.id == queue_item_id)
            .ok_or_else(|| CoreError::not_found("queue_item", queue_item_id))?;
        let removed_current = self.state.playback.current_queue_item_id.as_deref() == Some(queue_item_id);

        self.state.queue.remove(removed_index);

        if removed_current {
            self.playback.stop()?;
            self.state.playback.state = PlaybackStatus::Stopped;
            self.state.playback.position_ms = 0;
            self.state.playback.current_queue_item_id = None;
            self.normalize_queue_for_stop();
            self.publish_playback_state_changed()?;
        }

        self.publish_queue_updated(QueueUpdateReason::Remove)?;
        self.command_accepted(None, None)
    }

    pub fn move_queue_item(
        &mut self,
        queue_item_id: &str,
        to_index: u64,
    ) -> Result<CommandReceipt, CoreError> {
        let from_index = self
            .state
            .queue
            .iter()
            .position(|item| item.id == queue_item_id)
            .ok_or_else(|| CoreError::not_found("queue_item", queue_item_id))?;
        let target_index = usize::try_from(to_index).map_err(|_| {
            CoreError::validation("index_out_of_range", "queue move target index is too large")
        })?;
        if target_index >= self.state.queue.len() {
            return Err(CoreError::validation(
                "index_out_of_range",
                "queue move target index is out of range",
            ));
        }

        let item = self.state.queue.remove(from_index);
        self.state.queue.insert(target_index, item);

        if let Some(current_index) = self.current_index() {
            self.apply_queue_statuses(Some(current_index), QueueItemStatus::Playing);
        }

        self.publish_queue_updated(QueueUpdateReason::Move)?;
        self.command_accepted(None, None)
    }

    pub fn clear_queue(&mut self) -> Result<CommandReceipt, CoreError> {
        self.state.queue.clear();
        self.playback.stop()?;
        self.state.playback.state = PlaybackStatus::Stopped;
        self.state.playback.position_ms = 0;
        self.state.playback.current_queue_item_id = None;
        self.publish_playback_state_changed()?;
        self.publish_queue_updated(QueueUpdateReason::Clear)?;

        self.command_accepted(None, None)
    }

    pub fn play(&mut self) -> Result<CommandReceipt, CoreError> {
        if self.state.queue.is_empty() {
            return Err(CoreError::conflict(
                "queue_empty",
                "cannot start playback without queued items",
            ));
        }

        if self.state.playback.state == PlaybackStatus::Paused
            && self.current_index().is_some()
        {
            self.playback.resume()?;
            self.state.playback.state = PlaybackStatus::Playing;
            self.publish_playback_state_changed()?;
            return self.command_accepted(None, None);
        }

        let current_index = self.current_index().unwrap_or(0);
        self.start_track(current_index, self.state.playback.position_ms)?;

        self.command_accepted(None, None)
    }

    pub fn pause(&mut self) -> Result<CommandReceipt, CoreError> {
        if self.state.playback.state != PlaybackStatus::Playing {
            return Err(CoreError::conflict(
                "playback_not_playing",
                "cannot pause while playback is not running",
            ));
        }

        self.playback.pause()?;
        self.state.playback.state = PlaybackStatus::Paused;
        self.publish_playback_state_changed()?;

        self.command_accepted(None, None)
    }

    pub fn play_pause(&mut self) -> Result<CommandReceipt, CoreError> {
        match self.state.playback.state {
            PlaybackStatus::Playing => self.pause(),
            PlaybackStatus::Paused | PlaybackStatus::Stopped => self.play(),
        }
    }

    pub fn stop(&mut self) -> Result<CommandReceipt, CoreError> {
        self.playback.stop()?;
        self.state.playback.state = PlaybackStatus::Stopped;
        self.state.playback.position_ms = 0;
        self.state.playback.current_queue_item_id = None;
        self.normalize_queue_for_stop();
        self.publish_playback_state_changed()?;
        self.publish_queue_updated(QueueUpdateReason::CurrentChanged)?;

        self.command_accepted(None, None)
    }

    pub fn next(&mut self) -> Result<CommandReceipt, CoreError> {
        let next_index = match self.current_index() {
            Some(current_index) if current_index + 1 < self.state.queue.len() => current_index + 1,
            Some(_) => {
                self.stop()?;
                return self.command_accepted(None, None);
            }
            None if !self.state.queue.is_empty() => 0,
            None => {
                return Err(CoreError::conflict(
                    "queue_empty",
                    "cannot skip forward without queued items",
                ));
            }
        };

        self.start_track(next_index, 0)?;
        self.command_accepted(None, None)
    }

    pub fn previous(&mut self) -> Result<CommandReceipt, CoreError> {
        if self.state.queue.is_empty() {
            return Err(CoreError::conflict(
                "queue_empty",
                "cannot skip backward without queued items",
            ));
        }

        let target_index = match self.current_index() {
            Some(0) | None => 0,
            Some(current_index) => current_index - 1,
        };

        self.start_track(target_index, 0)?;
        self.command_accepted(None, None)
    }

    pub fn seek(&mut self, position_ms: u64) -> Result<CommandReceipt, CoreError> {
        if self.state.playback.current_queue_item_id.is_none() {
            return Err(CoreError::conflict(
                "no_current_track",
                "cannot seek without a current track",
            ));
        }

        self.playback.seek(position_ms)?;
        self.note_position(position_ms)?;
        self.command_accepted(None, None)
    }

    pub fn note_position(&mut self, position_ms: u64) -> Result<(), CoreError> {
        self.state.playback.position_ms = position_ms;
        self.publish(
            CoreEventKind::PlaybackPositionUpdated,
            CoreEvent::PlaybackPositionUpdated(PlaybackPositionUpdatedEvent {
                position_ms,
            }),
        )
    }

    pub fn finish_current_track(&mut self) -> Result<(), CoreError> {
        let Some(current_index) = self.current_index() else {
            return Ok(());
        };

        if let Some(item) = self.state.queue.get_mut(current_index) {
            item.status = QueueItemStatus::Finished;
        }

        if current_index + 1 < self.state.queue.len() {
            self.start_track(current_index + 1, 0)?;
        } else {
            self.playback.stop()?;
            self.state.playback.state = PlaybackStatus::Stopped;
            self.state.playback.position_ms = 0;
            self.state.playback.current_queue_item_id = None;
            self.normalize_queue_for_stop();
            self.publish_playback_state_changed()?;
            self.publish_queue_updated(QueueUpdateReason::CurrentChanged)?;
        }

        Ok(())
    }

    pub fn emit_system_error(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        severity: SystemErrorSeverity,
    ) -> Result<(), CoreError> {
        self.publish(
            CoreEventKind::SystemError,
            CoreEvent::SystemError(SystemErrorEvent {
                code: code.into(),
                message: message.into(),
                severity,
            }),
        )
    }

    fn start_track(&mut self, index: usize, position_ms: u64) -> Result<(), CoreError> {
        let item = self
            .state
            .queue
            .get(index)
            .cloned()
            .ok_or_else(|| CoreError::conflict("queue_empty", "no queue item available to play"))?;
        let track_changed = self.state.playback.current_queue_item_id.as_deref() != Some(item.id.as_str());

        self.playback.start(&item, position_ms)?;
        self.state.playback.state = PlaybackStatus::Playing;
        self.state.playback.position_ms = position_ms;
        self.state.playback.current_queue_item_id = Some(item.id.clone());
        self.apply_queue_statuses(Some(index), QueueItemStatus::Playing);

        if track_changed {
            self.publish(
                CoreEventKind::PlaybackTrackChanged,
                CoreEvent::PlaybackTrackChanged(PlaybackTrackChangedEvent {
                    queue_item_id: item.id.clone(),
                    song: item.song.clone(),
                }),
            )?;
        }

        self.publish_playback_state_changed()?;
        self.publish_queue_updated(QueueUpdateReason::CurrentChanged)
    }

    fn publish_playback_state_changed(&mut self) -> Result<(), CoreError> {
        self.publish(
            CoreEventKind::PlaybackStateChanged,
            CoreEvent::PlaybackStateChanged(PlaybackStateChangedEvent {
                state: self.state.playback.state,
                current_queue_item_id: self.state.playback.current_queue_item_id.clone(),
                position_ms: self.state.playback.position_ms,
            }),
        )
    }

    fn publish_queue_updated(&mut self, reason: QueueUpdateReason) -> Result<(), CoreError> {
        self.publish(
            CoreEventKind::QueueUpdated,
            CoreEvent::QueueUpdated(QueueUpdatedEvent {
                reason,
                items: self.state.queue.clone(),
            }),
        )
    }

    fn publish(&mut self, kind: CoreEventKind, event: CoreEvent) -> Result<(), CoreError> {
        let envelope = CoreEventEnvelope::new(self.ids.next_id(IdKind::Event), kind, self.clock.now(), event);
        self.events.publish(envelope)?;
        Ok(())
    }

    fn command_accepted(
        &mut self,
        job_id: Option<String>,
        queue_item_id: Option<String>,
    ) -> Result<CommandReceipt, CoreError> {
        Ok(CommandReceipt {
            command_id: self.ids.next_id(IdKind::Command),
            accepted_at: self.clock.now(),
            job_id,
            queue_item_id,
        })
    }

    fn current_song(&self) -> Option<&Song> {
        let current_id = self.state.playback.current_queue_item_id.as_deref()?;
        self.state
            .queue
            .iter()
            .find(|item| item.id == current_id)
            .map(|item| &item.song)
    }

    fn current_index(&self) -> Option<usize> {
        let current_id = self.state.playback.current_queue_item_id.as_deref()?;
        self.state.queue.iter().position(|item| item.id == current_id)
    }

    fn find_search_job_mut(&mut self, job_id: &str) -> Result<&mut SearchJobRecord, CoreError> {
        self.state
            .search_jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or_else(|| CoreError::not_found("search_job", job_id))
    }

    fn find_song(&self, song_id: &str) -> Option<&Song> {
        self.state
            .search_results
            .values()
            .flat_map(|songs| songs.iter())
            .find(|song| song.id == song_id)
    }

    fn normalize_queue_for_stop(&mut self) {
        for item in &mut self.state.queue {
            if matches!(item.status, QueueItemStatus::Playing | QueueItemStatus::Loading) {
                item.status = QueueItemStatus::Queued;
            }
        }
    }

    fn apply_queue_statuses(&mut self, current_index: Option<usize>, current_status: QueueItemStatus) {
        match current_index {
            Some(current_index) => {
                for (index, item) in self.state.queue.iter_mut().enumerate() {
                    item.status = if index < current_index {
                        QueueItemStatus::Finished
                    } else if index == current_index {
                        current_status
                    } else {
                        QueueItemStatus::Queued
                    };
                }
            }
            None => self.normalize_queue_for_stop(),
        }
    }
}

impl OrchestratorState {
    #[must_use]
    pub fn backend(&self) -> &BackendState {
        &self.backend
    }

    #[must_use]
    pub fn playback(&self) -> &PlaybackState {
        &self.playback
    }

    #[must_use]
    pub fn queue(&self) -> &[QueueItem] {
        &self.queue
    }

    #[must_use]
    pub fn search_jobs(&self) -> &[SearchJobRecord] {
        &self.search_jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ports::{ClockPort, EventPublisherPort, IdGeneratorPort, IdKind, PlaybackPort, SearchPort};

    #[derive(Default)]
    struct StubClock;

    impl ClockPort for StubClock {
        fn now(&self) -> String {
            "2026-04-23T12:34:56Z".to_owned()
        }
    }

    #[derive(Default)]
    struct StubIds {
        next: u64,
    }

    impl IdGeneratorPort for StubIds {
        fn next_id(&mut self, kind: IdKind) -> String {
            self.next += 1;
            let prefix = match kind {
                IdKind::Command => "cmd",
                IdKind::Event => "evt",
                IdKind::Snapshot => "snap",
                IdKind::QueueItem => "queue_item",
                IdKind::SearchJob => "job",
            };
            format!("{prefix}_{:04}", self.next)
        }
    }

    #[derive(Default)]
    struct StubEvents {
        published: Vec<CoreEventEnvelope<CoreEvent>>,
    }

    impl EventPublisherPort for StubEvents {
        fn publish(
            &mut self,
            event: CoreEventEnvelope<CoreEvent>,
        ) -> Result<(), PortError> {
            self.published.push(event);
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubPlayback {
        started: Vec<String>,
        paused: usize,
        resumed: usize,
        stopped: usize,
        seeks: Vec<u64>,
    }

    impl PlaybackPort for StubPlayback {
        fn start(&mut self, item: &QueueItem, position_ms: u64) -> Result<(), PortError> {
            self.started.push(format!("{}@{}", item.id, position_ms));
            Ok(())
        }

        fn pause(&mut self) -> Result<(), PortError> {
            self.paused += 1;
            Ok(())
        }

        fn resume(&mut self) -> Result<(), PortError> {
            self.resumed += 1;
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PortError> {
            self.stopped += 1;
            Ok(())
        }

        fn seek(&mut self, position_ms: u64) -> Result<(), PortError> {
            self.seeks.push(position_ms);
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubSearch {
        started: Vec<(String, String)>,
    }

    impl SearchPort for StubSearch {
        fn start_search(&mut self, job_id: &str, query: &str) -> Result<(), PortError> {
            self.started.push((job_id.to_owned(), query.to_owned()));
            Ok(())
        }
    }

    fn song(id: &str) -> Song {
        Song {
            id: id.to_owned(),
            title: format!("Song {id}"),
            channel_name: "Channel".to_owned(),
            duration_ms: 180_000,
            source_url: format!("https://example.com/{id}"),
        }
    }

    #[test]
    fn enqueue_and_play_updates_state() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        assert_eq!(orchestrator.queue().len(), 1);

        orchestrator.play().unwrap();
        assert_eq!(orchestrator.state().playback().state, PlaybackStatus::Playing);
        assert_eq!(
            orchestrator.state().playback().current_queue_item_id.as_deref(),
            Some("queue_item_0001")
        );
    }

    #[test]
    fn search_lifecycle_is_tracked() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        let accepted = orchestrator.submit_search("utada traveling").unwrap();
        let job_id = accepted.job_id.unwrap();

        orchestrator
            .complete_search(&job_id, vec![song("song_1"), song("song_2")])
            .unwrap();

        let results = orchestrator.search_results(&job_id).unwrap();
        assert_eq!(results.results.len(), 2);
        assert_eq!(results.job.status, SearchJobStatus::Completed);
    }

    #[test]
    fn full_search_to_play_flow_emits_expected_events() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        let accepted = orchestrator.submit_search("utada traveling").unwrap();
        let job_id = accepted.job_id.clone().unwrap();
        orchestrator.complete_search(&job_id, vec![song("song_1")]).unwrap();
        orchestrator.enqueue_song_by_id("song_1").unwrap();
        orchestrator.play().unwrap();
        orchestrator.seek(12_345).unwrap();
        orchestrator.stop().unwrap();

        let snapshot = orchestrator.snapshot();
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.search_jobs.len(), 1);
        assert_eq!(snapshot.playback.state, PlaybackStatus::Stopped);
        assert_eq!(snapshot.queue[0].status, QueueItemStatus::Queued);
    }

    struct FailingPlayback;

    impl PlaybackPort for FailingPlayback {
        fn start(&mut self, _item: &QueueItem, _position_ms: u64) -> Result<(), PortError> {
            Err(PortError::new("playback_start_failed", "stub start failure"))
        }

        fn pause(&mut self) -> Result<(), PortError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), PortError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PortError> {
            Ok(())
        }

        fn seek(&mut self, _position_ms: u64) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[test]
    fn playback_port_errors_are_mapped_to_core_errors() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            FailingPlayback,
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let error = orchestrator.play().unwrap_err();

        match error {
            CoreError::Port { code, message } => {
                assert_eq!(code, "playback_start_failed");
                assert_eq!(message, "stub start failure");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
