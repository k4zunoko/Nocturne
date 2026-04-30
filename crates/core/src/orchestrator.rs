use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use nocturne_domain::{
    AudioSettings, PlaybackState, PlaybackStatus, QueueItem, QueueItemStatus, Song,
};

use crate::models::{
    BackendState, CommandReceipt, CoreEvent, CoreEventEnvelope, CoreEventKind, CoreSnapshot,
    PlaybackProgressEvent, SearchJobCompletedEvent, SearchJobFailedEvent, SearchJobRecord,
    SearchJobStatus, SearchResultsRecord, StateUpdatedEvent, SystemErrorEvent,
    SystemErrorSeverity,
};
use crate::ports::{
    ClockPort, EventPublisherPort, IdGeneratorPort, IdKind, PlaybackPort, PortError, SearchPort,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    Validation { code: &'static str, message: String },
    NotFound { kind: &'static str, id: String },
    Conflict { code: &'static str, message: String },
    Port { code: String, message: String },
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
    audio: AudioSettings,
    queue: Vec<QueueItem>,
    search_jobs: Vec<SearchJobRecord>,
    search_results: BTreeMap<String, Vec<Song>>,
    revision: u64,
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
                playback_session_id: None,
            },
            audio: AudioSettings::default(),
            queue: Vec::new(),
            search_jobs: Vec::new(),
            search_results: BTreeMap::new(),
            revision: 0,
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

    #[must_use]
    pub fn backend_state(&self) -> &BackendState {
        &self.state.backend
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
            audio: self.state.audio,
            current_song: self.current_song().cloned(),
            queue: self.state.queue.clone(),
            search_jobs: self.state.search_jobs.clone(),
            revision: self.state.revision,
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

    pub fn hydrate_audio_settings(&mut self, settings: AudioSettings) -> Result<(), CoreError> {
        let settings = settings.clamped();
        self.playback.set_volume(settings.gain())?;
        self.state.audio = settings;
        Ok(())
    }

    pub fn set_volume(&mut self, volume_percent: u8) -> Result<CommandReceipt, CoreError> {
        if volume_percent > 100 {
            return Err(CoreError::validation(
                "volume_out_of_range",
                "volume must be between 0 and 100",
            ));
        }

        let settings = AudioSettings::new(volume_percent);
        self.playback.set_volume(settings.gain())?;
        self.state.audio = settings;
        self.publish_state_updated()?;
        self.command_accepted(None, None)
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
        self.publish(
            CoreEventKind::SearchJobStarted,
            CoreEvent::SearchJobStarted(summary),
        )?;

        self.command_accepted(Some(job_id), None)
    }

    pub fn complete_search(&mut self, job_id: &str, results: Vec<Song>) -> Result<(), CoreError> {
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
        self.publish_state_updated()?;

        if self.should_autoplay_on_enqueue() {
            self.start_track(0, 0)?;
        }

        self.command_accepted(None, Some(queue_item_id))
    }

    pub fn remove_queue_item(&mut self, queue_item_id: &str) -> Result<CommandReceipt, CoreError> {
        let removed_index = self
            .state
            .queue
            .iter()
            .position(|item| item.id == queue_item_id)
            .ok_or_else(|| CoreError::not_found("queue_item", queue_item_id))?;
        let removed_current =
            self.state.playback.current_queue_item_id.as_deref() == Some(queue_item_id);

        if removed_current {
            return Err(CoreError::conflict(
                "current_queue_item_not_removable",
                "cannot remove the currently playing queue item",
            ));
        }

        self.state.queue.remove(removed_index);

        self.publish_state_updated()?;
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

        if let Some(current_index) = self.current_index() {
            if from_index == current_index {
                return Err(CoreError::conflict(
                    "current_queue_item_not_movable",
                    "cannot move the currently playing queue item",
                ));
            }

            if target_index <= current_index {
                return Err(CoreError::conflict(
                    "queue_move_before_current_not_allowed",
                    "cannot move a queued item before the currently playing item",
                ));
            }
        }

        let item = self.state.queue.remove(from_index);
        self.state.queue.insert(target_index, item);

        if let Some(current_index) = self.current_index() {
            self.apply_queue_statuses(Some(current_index), QueueItemStatus::Playing);
        }

        self.publish_state_updated()?;
        self.command_accepted(None, None)
    }

    pub fn clear_queue(&mut self) -> Result<CommandReceipt, CoreError> {
        self.playback.stop()?;
        self.state.queue.clear();
        self.state.playback.state = PlaybackStatus::Stopped;
        self.state.playback.position_ms = 0;
        self.state.playback.current_queue_item_id = None;
        self.state.playback.playback_session_id = None;
        self.publish_state_updated()?;

        self.command_accepted(None, None)
    }

    pub fn play(&mut self) -> Result<CommandReceipt, CoreError> {
        if self.state.queue.is_empty() {
            return Err(CoreError::conflict(
                "queue_empty",
                "cannot start playback without queued items",
            ));
        }

        if self.state.playback.state == PlaybackStatus::Loading {
            return Err(CoreError::conflict(
                "playback_start_in_progress",
                "cannot start playback while another track is still loading",
            ));
        }

        if self.state.playback.state == PlaybackStatus::Paused && self.current_index().is_some() {
            self.playback.resume()?;
            self.state.playback.state = PlaybackStatus::Playing;
            self.publish_state_updated()?;
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
        self.publish_state_updated()?;

        self.command_accepted(None, None)
    }

    pub fn play_pause(&mut self) -> Result<CommandReceipt, CoreError> {
        match self.state.playback.state {
            PlaybackStatus::Playing => self.pause(),
            PlaybackStatus::Loading => Err(CoreError::conflict(
                "playback_start_in_progress",
                "cannot toggle playback while a track is still loading",
            )),
            PlaybackStatus::Paused | PlaybackStatus::Stopped => self.play(),
        }
    }

    pub fn stop(&mut self) -> Result<CommandReceipt, CoreError> {
        self.playback.stop()?;
        self.state.playback.state = PlaybackStatus::Stopped;
        self.state.playback.position_ms = 0;
        self.state.playback.current_queue_item_id = None;
        self.state.playback.playback_session_id = None;
        self.normalize_queue_for_stop();
        self.publish_state_updated()?;

        self.command_accepted(None, None)
    }

    pub fn next(&mut self) -> Result<CommandReceipt, CoreError> {
        match self.current_index() {
            Some(current_index) => {
                self.state.queue.remove(current_index);

                if self.state.queue.is_empty() {
                    self.playback.stop()?;
                    self.state.playback.state = PlaybackStatus::Stopped;
                    self.state.playback.position_ms = 0;
                    self.state.playback.current_queue_item_id = None;
                    self.state.playback.playback_session_id = None;
                    self.publish_state_updated()?;
                } else {
                    self.start_track(current_index.min(self.state.queue.len() - 1), 0)?;
                }
            }
            None if !self.state.queue.is_empty() => {
                self.start_track(0, 0)?;
            }
            None => {
                return Err(CoreError::conflict(
                    "queue_empty",
                    "cannot skip forward without queued items",
                ));
            }
        }

        self.command_accepted(None, None)
    }

    pub fn previous(&mut self) -> Result<CommandReceipt, CoreError> {
        if self.state.queue.is_empty() {
            return Err(CoreError::conflict(
                "queue_empty",
                "cannot skip backward without queued items",
            ));
        }

        let target_index = self.current_index().unwrap_or(0);

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

        if self.state.playback.state == PlaybackStatus::Loading {
            return Err(CoreError::conflict(
                "playback_start_in_progress",
                "cannot seek while a track is still loading",
            ));
        }

        self.playback.seek(position_ms)?;
        let playback_session_id = self
            .state
            .playback
            .playback_session_id
            .clone()
            .ok_or_else(|| CoreError::conflict("no_playback_session", "cannot seek without an active playback session"))?;
        self.report_playback_progress(&playback_session_id, position_ms)?;
        self.command_accepted(None, None)
    }

    pub fn report_playback_progress(
        &mut self,
        playback_session_id: &str,
        position_ms: u64,
    ) -> Result<bool, CoreError> {
        if self.state.playback.playback_session_id.as_deref() != Some(playback_session_id) {
            return Ok(false);
        }

        if self.state.playback.position_ms == position_ms {
            return Ok(false);
        }

        self.state.playback.position_ms = position_ms;
        self.publish(
            CoreEventKind::PlaybackProgress,
            CoreEvent::PlaybackProgress(PlaybackProgressEvent {
                playback_session_id: playback_session_id.to_owned(),
                position_ms,
            }),
        )?;
        Ok(true)
    }

    pub fn finish_current_track(&mut self, playback_session_id: &str, position_ms: u64) -> Result<bool, CoreError> {
        if self.state.playback.playback_session_id.as_deref() != Some(playback_session_id) {
            return Ok(false);
        }

        self.state.playback.position_ms = position_ms;
        let Some(current_index) = self.current_index() else {
            return Ok(false);
        };

        self.state.queue.remove(current_index);

        if self.state.queue.is_empty() {
            self.playback.stop()?;
            self.state.playback.state = PlaybackStatus::Stopped;
            self.state.playback.position_ms = 0;
            self.state.playback.current_queue_item_id = None;
            self.state.playback.playback_session_id = None;
            self.publish_state_updated()?;
        } else {
            self.start_track(current_index.min(self.state.queue.len() - 1), 0)?;
        }

        Ok(true)
    }

    pub fn confirm_playback_started(
        &mut self,
        playback_session_id: &str,
        queue_item_id: &str,
        position_ms: u64,
    ) -> Result<bool, CoreError> {
        if self.state.playback.state != PlaybackStatus::Loading
            || self.state.playback.current_queue_item_id.as_deref() != Some(queue_item_id)
            || self.state.playback.playback_session_id.as_deref() != Some(playback_session_id)
        {
            return Ok(false);
        }

        self.state.playback.state = PlaybackStatus::Playing;
        self.state.playback.position_ms = position_ms;

        if let Some(current_index) = self.current_index() {
            self.apply_queue_statuses(Some(current_index), QueueItemStatus::Playing);
        }

        self.publish_state_updated()?;
        Ok(true)
    }

    pub fn report_playback_start_failed(
        &mut self,
        playback_session_id: &str,
        queue_item_id: &str,
    ) -> Result<bool, CoreError> {
        if self.state.playback.state != PlaybackStatus::Loading
            || self.state.playback.current_queue_item_id.as_deref() != Some(queue_item_id)
            || self.state.playback.playback_session_id.as_deref() != Some(playback_session_id)
        {
            return Ok(false);
        }

        self.state.playback.state = PlaybackStatus::Stopped;
        self.state.playback.position_ms = 0;
        self.state.playback.current_queue_item_id = None;
        self.state.playback.playback_session_id = None;

        if let Some(queue_item) = self
            .state
            .queue
            .iter_mut()
            .find(|item| item.id == queue_item_id)
        {
            queue_item.status = QueueItemStatus::Failed;
        }

        self.publish_state_updated()?;
        Ok(true)
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
        let item =
            self.state.queue.get(index).cloned().ok_or_else(|| {
                CoreError::conflict("queue_empty", "no queue item available to play")
            })?;
        let playback_session_id = self.ids.next_id(IdKind::PlaybackSession);

        self.state.playback.state = PlaybackStatus::Loading;
        self.state.playback.position_ms = position_ms;
        self.state.playback.current_queue_item_id = Some(item.id.clone());
        self.state.playback.playback_session_id = Some(playback_session_id.clone());
        self.apply_queue_statuses(Some(index), QueueItemStatus::Loading);

        self.publish_state_updated()?;

        if let Err(error) = self.playback.start(&item, &playback_session_id, position_ms) {
            self.state.playback.state = PlaybackStatus::Stopped;
            self.state.playback.position_ms = 0;
            self.state.playback.current_queue_item_id = None;
            self.state.playback.playback_session_id = None;

            if let Some(queue_item) = self.state.queue.get_mut(index) {
                queue_item.status = QueueItemStatus::Failed;
            }

            self.publish_state_updated()?;
            return Err(error.into());
        }

        Ok(())
    }

    fn publish_state_updated(&mut self) -> Result<(), CoreError> {
        self.state.revision = self.state.revision.saturating_add(1);
        let snapshot = self.snapshot();
        self.publish(
            CoreEventKind::StateUpdated,
            CoreEvent::StateUpdated(StateUpdatedEvent { snapshot }),
        )
    }

    fn publish(&mut self, kind: CoreEventKind, event: CoreEvent) -> Result<(), CoreError> {
        let envelope = CoreEventEnvelope::new(
            self.ids.next_id(IdKind::Event),
            kind,
            self.clock.now(),
            event,
        );
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
        self.state
            .queue
            .iter()
            .position(|item| item.id == current_id)
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
            if matches!(
                item.status,
                QueueItemStatus::Playing | QueueItemStatus::Loading
            ) {
                item.status = QueueItemStatus::Queued;
            }
        }
    }

    fn should_autoplay_on_enqueue(&self) -> bool {
        self.state.playback.state == PlaybackStatus::Stopped
            && self.state.playback.current_queue_item_id.is_none()
            && !self.state.queue.is_empty()
    }

    fn apply_queue_statuses(
        &mut self,
        current_index: Option<usize>,
        current_status: QueueItemStatus,
    ) {
        match current_index {
            Some(current_index) => {
                for (index, item) in self.state.queue.iter_mut().enumerate() {
                    item.status = if index == current_index {
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
    pub fn audio(&self) -> &AudioSettings {
        &self.audio
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

    use crate::ports::{
        ClockPort, EventPublisherPort, IdGeneratorPort, IdKind, PlaybackPort, SearchPort,
    };

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
                IdKind::PlaybackSession => "playback_session",
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
        fn publish(&mut self, event: CoreEventEnvelope<CoreEvent>) -> Result<(), PortError> {
            self.published.push(event);
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubPlayback {
        started: Vec<String>,
        volumes: Vec<f32>,
        paused: usize,
        resumed: usize,
        stopped: usize,
        seeks: Vec<u64>,
        fail_stop: bool,
    }

    impl PlaybackPort for StubPlayback {
        fn start(
            &mut self,
            item: &QueueItem,
            playback_session_id: &str,
            position_ms: u64,
        ) -> Result<(), PortError> {
            self.started
                .push(format!("{}:{}@{}", playback_session_id, item.id, position_ms));
            Ok(())
        }

        fn set_volume(&mut self, gain: f32) -> Result<(), PortError> {
            self.volumes.push(gain);
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
            if self.fail_stop {
                return Err(PortError::new(
                    "playback_stop_failed",
                    "stub stop failure",
                ));
            }
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

    fn current_session_id<C, I, E, P, S>(orchestrator: &Orchestrator<C, I, E, P, S>) -> String
    where
        C: ClockPort,
        I: IdGeneratorPort,
        E: EventPublisherPort,
        P: PlaybackPort,
        S: SearchPort,
    {
        orchestrator
            .state()
            .playback()
            .playback_session_id
            .clone()
            .expect("expected playback session id")
    }

    #[test]
    fn enqueue_autoplays_when_stopped() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Loading
        );
        assert_eq!(
            orchestrator
                .state()
                .playback()
                .current_queue_item_id
                .as_deref(),
            Some("queue_item_0001")
        );
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Loading);
        let playback_session_id = current_session_id(&orchestrator);
        assert_eq!(
            orchestrator.playback.started,
            vec![format!("{playback_session_id}:queue_item_0001@0")]
        );

        orchestrator
            .confirm_playback_started(&playback_session_id, "queue_item_0001", 0)
            .unwrap();

        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Playing
        );
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
    }

    #[test]
    fn enqueue_does_not_resume_when_paused() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let first_queue_item_id = orchestrator
            .state()
            .playback()
            .current_queue_item_id
            .clone()
            .unwrap();
        let first_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&first_session_id, &first_queue_item_id, 0)
            .unwrap();
        orchestrator.pause().unwrap();

        orchestrator.enqueue_song(song("song_2")).unwrap();

        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Paused
        );
        assert_eq!(
            orchestrator
                .state()
                .playback()
                .current_queue_item_id
                .as_deref(),
            Some(first_queue_item_id.as_str())
        );
        assert_eq!(orchestrator.queue().len(), 2);
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
        assert_eq!(orchestrator.queue()[1].status, QueueItemStatus::Queued);
        assert_eq!(
            orchestrator.playback.started,
            vec![format!("{first_session_id}:queue_item_0001@0")]
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
        orchestrator
            .complete_search(&job_id, vec![song("song_1")])
            .unwrap();
        let queue_item_id = orchestrator
            .enqueue_song_by_id("song_1")
            .unwrap()
            .queue_item_id
            .unwrap();
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 0)
            .unwrap();
        orchestrator.seek(12_345).unwrap();
        orchestrator.stop().unwrap();

        let snapshot = orchestrator.snapshot();
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.search_jobs.len(), 1);
        assert_eq!(snapshot.playback.state, PlaybackStatus::Stopped);
        assert_eq!(snapshot.queue[0].status, QueueItemStatus::Queued);
    }

    #[test]
    fn finish_current_track_removes_finished_item_and_autoplays_next() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let second_queue_item_id = orchestrator
            .enqueue_song(song("song_2"))
            .unwrap()
            .queue_item_id
            .unwrap();
        let first_session_id = current_session_id(&orchestrator);

        orchestrator
            .finish_current_track(&first_session_id, 180_000)
            .unwrap();
        let second_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&second_session_id, &second_queue_item_id, 0)
            .unwrap();

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(orchestrator.queue()[0].song.id, "song_2");
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
        assert_eq!(
            orchestrator
                .state()
                .playback()
                .current_queue_item_id
                .as_deref(),
            Some(second_queue_item_id.as_str())
        );
    }

    #[test]
    fn next_removes_current_item_before_advancing() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let second_queue_item_id = orchestrator
            .enqueue_song(song("song_2"))
            .unwrap()
            .queue_item_id
            .unwrap();
        let first_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&first_session_id, "queue_item_0001", 0)
            .unwrap();

        orchestrator.next().unwrap();
        let second_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&second_session_id, &second_queue_item_id, 0)
            .unwrap();

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(orchestrator.queue()[0].song.id, "song_2");
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
    }

    #[test]
    fn previous_restarts_current_track_without_history() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let first_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&first_session_id, "queue_item_0001", 0)
            .unwrap();
        orchestrator.seek(12_345).unwrap();

        orchestrator.previous().unwrap();
        let second_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&second_session_id, "queue_item_0001", 0)
            .unwrap();

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(orchestrator.queue()[0].song.id, "song_1");
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
        assert_eq!(orchestrator.state().playback().position_ms, 0);
    }

    #[test]
    fn moving_before_current_item_is_rejected() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        let first_id = orchestrator
            .enqueue_song(song("song_1"))
            .unwrap()
            .queue_item_id
            .unwrap();
        let second_id = orchestrator
            .enqueue_song(song("song_2"))
            .unwrap()
            .queue_item_id
            .unwrap();
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &first_id, 0)
            .unwrap();

        let error = orchestrator.move_queue_item(&second_id, 0).unwrap_err();

        match error {
            CoreError::Conflict { code, message } => {
                assert_eq!(code, "queue_move_before_current_not_allowed");
                assert_eq!(
                    message,
                    "cannot move a queued item before the currently playing item"
                );
            }
            other => panic!("unexpected error: {other}"),
        }

        assert_eq!(
            orchestrator
                .state()
                .playback()
                .current_queue_item_id
                .as_deref(),
            Some(first_id.as_str())
        );
    }

    #[test]
    fn removing_current_queue_item_is_rejected() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, "queue_item_0001", 0)
            .unwrap();

        let error = orchestrator
            .remove_queue_item("queue_item_0001")
            .unwrap_err();

        match error {
            CoreError::Conflict { code, message } => {
                assert_eq!(code, "current_queue_item_not_removable");
                assert_eq!(message, "cannot remove the currently playing queue item");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    struct FailingPlayback;

    impl PlaybackPort for FailingPlayback {
        fn start(
            &mut self,
            _item: &QueueItem,
            _playback_session_id: &str,
            _position_ms: u64,
        ) -> Result<(), PortError> {
            Err(PortError::new(
                "playback_start_failed",
                "stub start failure",
            ))
        }

        fn set_volume(&mut self, _gain: f32) -> Result<(), PortError> {
            Ok(())
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
    fn enqueue_autoplay_errors_are_mapped_to_core_errors() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            FailingPlayback,
            StubSearch::default(),
        );

        let error = orchestrator.enqueue_song(song("song_1")).unwrap_err();

        match error {
            CoreError::Port { code, message } => {
                assert_eq!(code, "playback_start_failed");
                assert_eq!(message, "stub start failure");
            }
            other => panic!("unexpected error: {other}"),
        }

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Failed);
        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Stopped
        );
        assert_eq!(orchestrator.state().playback().current_queue_item_id, None);
    }

    #[test]
    fn playback_start_is_confirmed_explicitly_after_loading() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        let queue_item_id = orchestrator
            .enqueue_song(song("song_1"))
            .unwrap()
            .queue_item_id
            .unwrap();

        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Loading
        );
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Loading);

        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Playing
        );
        assert_eq!(orchestrator.state().playback().position_ms, 321);
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Playing);
    }

    #[test]
    fn volume_is_updated_and_kept_in_snapshot() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.set_volume(65).unwrap();

        assert_eq!(orchestrator.state().audio().volume_percent, 65);
        assert_eq!(orchestrator.playback.volumes, vec![0.65]);

        let snapshot = orchestrator.snapshot();
        assert_eq!(snapshot.audio.volume_percent, 65);
    }

    #[test]
    fn playback_start_failure_marks_current_item_failed() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        let queue_item_id = orchestrator
            .enqueue_song(song("song_1"))
            .unwrap()
            .queue_item_id
            .unwrap();
        let playback_session_id = current_session_id(&orchestrator);

        orchestrator
            .report_playback_start_failed(&playback_session_id, &queue_item_id)
            .unwrap();

        assert_eq!(
            orchestrator.state().playback().state,
            PlaybackStatus::Stopped
        );
        assert_eq!(orchestrator.state().playback().current_queue_item_id, None);
        assert_eq!(orchestrator.queue()[0].status, QueueItemStatus::Failed);
    }

    #[test]
    fn seek_is_rejected_while_playback_is_loading() {
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            StubPlayback::default(),
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();

        let error = orchestrator.seek(123).unwrap_err();

        match error {
            CoreError::Conflict { code, message } => {
                assert_eq!(code, "playback_start_in_progress");
                assert_eq!(message, "cannot seek while a track is still loading");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn clear_queue_does_not_mutate_state_when_stop_fails() {
        let playback = StubPlayback {
            fail_stop: true,
            ..Default::default()
        };
        let mut orchestrator = Orchestrator::new(
            StubClock,
            StubIds::default(),
            StubEvents::default(),
            playback,
            StubSearch::default(),
        );

        orchestrator.enqueue_song(song("song_1")).unwrap();
        let error = orchestrator.clear_queue().unwrap_err();

        match error {
            CoreError::Port { code, message } => {
                assert_eq!(code, "playback_stop_failed");
                assert_eq!(message, "stub stop failure");
            }
            other => panic!("unexpected error: {other}"),
        }

        assert_eq!(orchestrator.queue().len(), 1);
        assert_eq!(orchestrator.state().playback().state, PlaybackStatus::Loading);
        assert_eq!(
            orchestrator.state().playback().current_queue_item_id.as_deref(),
            Some("queue_item_0001")
        );
    }
}
