use nocturne_core::{CoreError, SystemErrorSeverity as CoreSystemErrorSeverity};

use crate::BackendOrchestrator;

pub(crate) fn report_playback_command_error(
    orchestrator: &mut BackendOrchestrator,
    error: &CoreError,
) {
    let CoreError::Port { code, .. } = error else {
        return;
    };

    if let Err(report_error) = orchestrator.emit_system_error(
        code.clone(),
        user_message_for_playback_failure(code),
        CoreSystemErrorSeverity::Error,
    ) {
        eprintln!("failed to emit playback system error for {code}: {report_error}");
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
