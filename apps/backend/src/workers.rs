use std::sync::Arc;
use std::time::Duration;

use nocturne_core::{CoreError, SystemErrorSeverity as CoreSystemErrorSeverity};
use nocturne_infrastructure::{
    LocalSearchRuntime, LocalYtDlpManager, PlaybackWorkerEvent, YoutubeImportResolution,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::BackendOrchestrator;

pub(crate) fn spawn_yt_dlp_update_worker(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    yt_dlp_manager: LocalYtDlpManager,
) {
    tokio::spawn(async move {
        if let Err(error) = yt_dlp_manager.check_for_updates_if_due().await {
            eprintln!("failed to check for yt-dlp updates: {error}");
            return;
        }

        match yt_dlp_manager.apply_pending_update() {
            Ok(true) => {
                let version = yt_dlp_manager.current_version();
                let mut orchestrator = orchestrator.lock().await;
                if let Err(error) = orchestrator.update_yt_dlp_version(version) {
                    eprintln!("failed to publish yt-dlp version update: {error}");
                }
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("failed to promote yt-dlp update: {error}");
            }
        }
    });
}

pub(crate) fn spawn_search_worker(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    search_runtime: LocalSearchRuntime,
) {
    tokio::spawn(async move {
        loop {
            let processed = process_pending_search_jobs(&orchestrator, &search_runtime).await
                + process_pending_youtube_import_jobs(&orchestrator, &search_runtime).await;
            let delay = if processed == 0 {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(10)
            };
            tokio::time::sleep(delay).await;
        }
    });
}

pub(crate) fn spawn_playback_event_bridge(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    mut playback_events: mpsc::UnboundedReceiver<PlaybackWorkerEvent>,
) {
    tokio::spawn(async move {
        loop {
            match playback_events.recv().await {
                Some(PlaybackWorkerEvent::Started {
                    playback_session_id,
                    queue_item_id,
                    position_ms,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    match orchestrator.confirm_playback_started(
                        &playback_session_id,
                        &queue_item_id,
                        position_ms,
                    ) {
                        Ok(true) => {}
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!(
                                "failed to confirm playback start for {}: {error}",
                                queue_item_id
                            );
                        }
                    }
                }
                Some(PlaybackWorkerEvent::StartFailed {
                    playback_session_id,
                    queue_item_id,
                    code,
                    message,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    match orchestrator
                        .report_playback_start_failed(&playback_session_id, &queue_item_id)
                    {
                        Ok(true) => {
                            if let Err(error) = orchestrator.emit_system_error(
                                code.clone(),
                                user_message_for_playback_failure(&code),
                                CoreSystemErrorSeverity::Error,
                            ) {
                                eprintln!(
                                    "failed to emit async playback start error for {}: {error}",
                                    queue_item_id
                                );
                            } else {
                                eprintln!(
                                    "playback start failed for {}: {}",
                                    queue_item_id, message
                                );
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!(
                                "failed to mark playback start failure for {}: {error}",
                                queue_item_id
                            );
                        }
                    }
                }
                Some(PlaybackWorkerEvent::Interrupted {
                    playback_session_id,
                    queue_item_id,
                    code,
                    message,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    if orchestrator
                        .state()
                        .playback()
                        .playback_session_id
                        .as_deref()
                        != Some(playback_session_id.as_str())
                    {
                        continue;
                    }

                    if let Err(error) = orchestrator.stop() {
                        eprintln!(
                            "failed to stop interrupted playback for {}: {error}",
                            queue_item_id
                        );
                    }

                    if let Err(error) = orchestrator.emit_system_error(
                        code.clone(),
                        user_message_for_playback_interruption(&code),
                        CoreSystemErrorSeverity::Error,
                    ) {
                        eprintln!(
                            "failed to emit interrupted playback error for {}: {error}",
                            queue_item_id
                        );
                    } else {
                        eprintln!("playback interrupted for {}: {}", queue_item_id, message);
                    }
                }
                Some(PlaybackWorkerEvent::Progress {
                    playback_session_id,
                    position_ms,
                    paused,
                }) => {
                    if paused {
                        continue;
                    }
                    let mut orchestrator = orchestrator.lock().await;
                    if let Err(error) =
                        orchestrator.report_playback_progress(&playback_session_id, position_ms)
                    {
                        eprintln!(
                            "failed to publish playback progress {} for session {}: {error}",
                            position_ms, playback_session_id
                        );
                    }
                }
                Some(PlaybackWorkerEvent::Ended {
                    playback_session_id,
                    queue_item_id,
                    position_ms,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    if let Err(error) =
                        orchestrator.finish_current_track(&playback_session_id, position_ms)
                    {
                        eprintln!(
                            "failed to advance playback after natural track end {}: {error}",
                            queue_item_id
                        );
                    }
                }
                None => break,
            }
        }
    });
}

pub(crate) async fn process_pending_search_jobs(
    orchestrator: &Arc<Mutex<BackendOrchestrator>>,
    search_runtime: &LocalSearchRuntime,
) -> usize {
    let pending_jobs = search_runtime.drain_pending();

    for job in &pending_jobs {
        let runtime = search_runtime.clone();
        let query = job.query.clone();
        let result = tokio::task::spawn_blocking(move || runtime.resolve(&query)).await;

        let mut orchestrator = orchestrator.lock().await;
        match result {
            Ok(Ok(results)) => {
                if let Err(error) = orchestrator.complete_search(&job.job_id, results) {
                    eprintln!("failed to complete search job {}: {error}", job.job_id);
                }
            }
            Ok(Err(failure)) => {
                let user_message = user_message_for_search_failure(&failure.code);
                let should_emit_system_error = matches!(
                    failure.code.as_str(),
                    "yt_dlp_missing" | "yt_dlp_spawn_failed" | "yt_dlp_timeout"
                );

                if let Err(error) =
                    orchestrator.fail_search(&job.job_id, &failure.code, user_message)
                {
                    eprintln!("failed to fail search job {}: {error}", job.job_id);
                }

                if should_emit_system_error
                    && let Err(error) = orchestrator.emit_system_error(
                        &failure.code,
                        user_message,
                        CoreSystemErrorSeverity::Error,
                    )
                {
                    eprintln!("failed to emit system error for {}: {error}", job.job_id);
                }
            }
            Err(error) => {
                if let Err(report_error) = orchestrator.fail_search(
                    &job.job_id,
                    "search_worker_join_failed",
                    error.to_string(),
                ) {
                    eprintln!(
                        "failed to report search worker join error for {}: {report_error}",
                        job.job_id
                    );
                }
            }
        }
    }

    pending_jobs.len()
}

pub(crate) async fn process_pending_youtube_import_jobs(
    orchestrator: &Arc<Mutex<BackendOrchestrator>>,
    search_runtime: &LocalSearchRuntime,
) -> usize {
    let pending_jobs = search_runtime.drain_pending_youtube_imports();

    for job in &pending_jobs {
        let runtime = search_runtime.clone();
        let url = job.url.clone();
        let result = tokio::task::spawn_blocking(move || runtime.resolve_youtube_url(&url)).await;

        let mut orchestrator = orchestrator.lock().await;
        match result {
            Ok(Ok(YoutubeImportResolution {
                songs,
                total_count,
                failed_count,
                ..
            })) => {
                if let Err(error) = orchestrator.complete_youtube_import(
                    &job.job_id,
                    songs,
                    total_count,
                    failed_count,
                ) {
                    let code = core_error_code(&error).to_owned();
                    let user_message = user_message_for_youtube_import_completion_error(&error);
                    let should_emit_system_error = matches!(
                        code.as_str(),
                        "yt_dlp_missing"
                            | "yt_dlp_spawn_failed"
                            | "yt_dlp_timeout"
                            | "audio_output_unavailable"
                            | "audio_source_resolve_failed"
                            | "playback_stream_open_failed"
                            | "playback_decode_failed"
                            | "playback_worker_unavailable"
                    );

                    if let Err(report_error) =
                        orchestrator.fail_youtube_import(&job.job_id, &code, user_message)
                    {
                        eprintln!(
                            "failed to convert youtube import completion error for {}: {report_error}",
                            job.job_id
                        );
                    }

                    if should_emit_system_error
                        && let Err(report_error) = orchestrator.emit_system_error(
                            &code,
                            user_message,
                            CoreSystemErrorSeverity::Error,
                        )
                    {
                        eprintln!(
                            "failed to emit system error for youtube import {}: {report_error}",
                            job.job_id
                        );
                    }
                }
            }
            Ok(Err(failure)) => {
                let user_message = user_message_for_youtube_import_failure(&failure.code);
                let should_emit_system_error = matches!(
                    failure.code.as_str(),
                    "yt_dlp_missing" | "yt_dlp_spawn_failed" | "yt_dlp_timeout"
                );

                if let Err(error) =
                    orchestrator.fail_youtube_import(&job.job_id, &failure.code, user_message)
                {
                    eprintln!("failed to fail youtube import job {}: {error}", job.job_id);
                }

                if should_emit_system_error
                    && let Err(error) = orchestrator.emit_system_error(
                        &failure.code,
                        user_message,
                        CoreSystemErrorSeverity::Error,
                    )
                {
                    eprintln!(
                        "failed to emit system error for youtube import {}: {error}",
                        job.job_id
                    );
                }
            }
            Err(error) => {
                if let Err(report_error) = orchestrator.fail_youtube_import(
                    &job.job_id,
                    "youtube_import_worker_join_failed",
                    error.to_string(),
                ) {
                    eprintln!(
                        "failed to report youtube import join error for {}: {report_error}",
                        job.job_id
                    );
                }
            }
        }
    }

    pending_jobs.len()
}

pub(crate) fn user_message_for_search_failure(code: &str) -> &'static str {
    match code {
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start the search provider.",
        "yt_dlp_timeout" => "The search provider timed out.",
        "yt_dlp_invalid_json" | "yt_dlp_invalid_response" => {
            "The search provider returned unreadable results."
        }
        "yt_dlp_failed" => "Search provider failed to return results.",
        _ => "Search failed on the backend.",
    }
}

pub(crate) fn user_message_for_youtube_import_failure(code: &str) -> &'static str {
    match code {
        "youtube_url_invalid" => "Input is not a valid YouTube URL.",
        "youtube_url_unsupported" => "This YouTube URL format is not supported yet.",
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start YouTube URL resolution.",
        "yt_dlp_timeout" => "YouTube URL resolution timed out.",
        "yt_dlp_invalid_json" | "yt_dlp_invalid_response" => {
            "The backend could not read metadata for this YouTube URL."
        }
        "yt_dlp_failed" => "The backend failed to resolve this YouTube URL.",
        "youtube_import_empty" => {
            "The backend did not find any playable videos for this YouTube URL."
        }
        _ => "YouTube URL import failed on the backend.",
    }
}

pub(crate) fn user_message_for_youtube_import_completion_error(error: &CoreError) -> &'static str {
    match error {
        CoreError::Port { code, .. } => user_message_for_playback_failure(code),
        CoreError::Validation { code, .. } | CoreError::Conflict { code, .. } => {
            user_message_for_youtube_import_failure(code)
        }
        CoreError::NotFound { .. } => "YouTube URL import failed on the backend.",
    }
}

pub(crate) fn core_error_code(error: &CoreError) -> &str {
    match error {
        CoreError::Validation { code, .. } | CoreError::Conflict { code, .. } => code,
        CoreError::Port { code, .. } => code,
        CoreError::NotFound { kind, .. } => kind,
    }
}

pub(crate) fn user_message_for_playback_failure(code: &str) -> &'static str {
    match code {
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start yt-dlp for playback.",
        "yt_dlp_timeout" => "The backend timed out while preparing audio playback.",
        "audio_output_unavailable" => "Audio output is unavailable on the backend.",
        "audio_source_resolve_failed"
        | "playback_stream_open_failed"
        | "playback_decode_failed" => "The backend could not load audio for playback.",
        "playback_seek_failed" => "The backend could not seek the current playback stream.",
        "playback_url_invalid" | "playback_runtime_failed" => {
            "The backend could not prepare playback."
        }
        "playback_worker_unavailable" => "The backend playback worker is unavailable.",
        _ => "Playback failed on the backend.",
    }
}

pub(crate) fn user_message_for_playback_interruption(code: &str) -> &'static str {
    match code {
        "audio_output_changed" => {
            "Audio output changed. Playback stopped, and the next play will use the current default device."
        }
        "audio_output_stream_lost" => {
            "Playback stopped because the backend lost its audio output stream. Try playing again."
        }
        _ => "Playback stopped because audio output became unavailable on the backend.",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use nocturne_core::{
        CoreEvent, SearchJobStatus as CoreSearchJobStatus, SystemErrorSeverity as CoreSeverity,
    };
    use nocturne_domain::{AudioSettings, PlaybackStatus, Song};
    use nocturne_infrastructure::{
        BroadcastEventPublisher, LocalClock, LocalEventLog, LocalIdGenerator, LocalSearchAdapter,
        LocalSearchRuntime, PlaybackWorkerEvent,
    };
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    use crate::BackendOrchestrator;
    use crate::report_playback_command_error;
    use crate::test_support::{
        current_session_id, test_playback_adapter, test_search_runtime_with_fixture,
    };
    use crate::workers::{
        process_pending_search_jobs, process_pending_youtube_import_jobs,
        spawn_playback_event_bridge,
    };

    #[tokio::test]
    async fn pending_search_jobs_are_completed_by_worker_helper() {
        let (event_publisher, search_runtime, orchestrator) =
            test_search_runtime_with_fixture("worker fixture");

        let job_id = {
            let mut locked = orchestrator.lock().await;
            locked
                .submit_search("worker fixture")
                .unwrap()
                .job_id
                .unwrap()
        };

        let processed = process_pending_search_jobs(&orchestrator, &search_runtime).await;
        assert_eq!(processed, 1);

        let locked = orchestrator.lock().await;
        let results = locked.search_results(&job_id).unwrap();
        assert_eq!(results.job.status, CoreSearchJobStatus::Completed);
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Worker Fixture Song");
        drop((locked, event_publisher));
    }

    #[tokio::test]
    async fn pending_youtube_import_jobs_are_completed_by_worker_helper() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let search_runtime = LocalSearchRuntime::default();
        search_runtime.set_fixture(
            "https://www.youtube.com/watch?v=tuyZ9f6mHZk",
            vec![Song {
                id: String::from("youtube:tuyZ9f6mHZk"),
                title: String::from("traveling"),
                channel_name: String::from("Hikaru Utada"),
                duration_ms: 295_000,
                source_url: String::from("https://www.youtube.com/watch?v=tuyZ9f6mHZk"),
            }],
        );

        let orchestrator = Arc::new(Mutex::new(BackendOrchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        {
            let mut locked = orchestrator.lock().await;
            locked
                .submit_youtube_import("https://www.youtube.com/watch?v=tuyZ9f6mHZk")
                .unwrap();
        }

        let processed = process_pending_youtube_import_jobs(&orchestrator, &search_runtime).await;
        assert_eq!(processed, 1);

        let locked = orchestrator.lock().await;
        assert_eq!(locked.queue().len(), 1);
        assert_eq!(locked.queue()[0].song.id, "youtube:tuyZ9f6mHZk");
        drop(locked);

        let snapshot = event_log.snapshot();
        assert!(
            snapshot
                .iter()
                .any(|event| event.event == "youtube.import.completed")
        );
    }

    #[tokio::test]
    async fn worker_emits_system_error_for_missing_yt_dlp() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let search_runtime = LocalSearchRuntime::default();
        search_runtime.set_failure(
            "missing provider",
            "yt_dlp_missing",
            "hidden internal detail",
        );
        let orchestrator = Arc::new(Mutex::new(BackendOrchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher.clone(),
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        {
            let mut locked = orchestrator.lock().await;
            locked.submit_search("missing provider").unwrap();
        }
        process_pending_search_jobs(&orchestrator, &search_runtime).await;

        let snapshot = event_log.snapshot();
        let has_system_error = snapshot
            .iter()
            .any(|event| matches!(event.data, CoreEvent::SystemError(_)));
        assert!(has_system_error);
    }

    #[test]
    fn playback_port_errors_emit_user_visible_system_errors() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = BackendOrchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );

        report_playback_command_error(
            &mut orchestrator,
            &nocturne_core::CoreError::Port {
                code: String::from("audio_output_unavailable"),
                message: String::from("hidden detail"),
            },
        );

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_unavailable");
        assert_eq!(
            system_error.message,
            "Audio output is unavailable on the backend."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_playback_event_stops_playback_and_emits_system_error() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = BackendOrchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-song"),
                title: String::from("Interrupted Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_changed"),
                message: String::from("recovery failed after device loss"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let locked = orchestrator.lock().await;
        assert_eq!(locked.state().playback().state, PlaybackStatus::Stopped);
        assert!(locked.state().playback().current_queue_item_id.is_none());
        drop(locked);

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_changed");
        assert_eq!(
            system_error.message,
            "Audio output changed. Playback stopped, and the next play will use the current default device."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_stream_error_emits_generic_audio_output_message() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = BackendOrchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-stream-song"),
                title: String::from("Interrupted Stream Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-stream-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_stream_lost"),
                message: String::from("stream error: device disappeared"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_stream_lost");
        assert_eq!(
            system_error.message,
            "Playback stopped because the backend lost its audio output stream. Try playing again."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }
}
