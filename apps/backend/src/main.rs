use nocturne_api::ApiVersion;
use nocturne_core::{NocturneCore, Orchestrator};
use nocturne_infrastructure::{
    InfrastructureProfile, LocalClock, LocalEventLog, LocalEventPublisher, LocalIdGenerator,
    LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime, SharedPlaybackState,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let core = NocturneCore::new();
    let event_log = LocalEventLog::default();
    let playback_state = SharedPlaybackState::default();
    let search_runtime = LocalSearchRuntime::default();

    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        LocalEventPublisher::new(event_log.clone()),
        LocalPlaybackAdapter::new(playback_state.clone()),
        LocalSearchAdapter::new(search_runtime.clone()),
    );
    orchestrator.set_backend_version(Some(env!("CARGO_PKG_VERSION")));

    let accepted = orchestrator.submit_search("utada traveling")?;
    for pending in search_runtime.drain_pending() {
        match search_runtime.resolve(&pending.query) {
            Ok(results) => orchestrator.complete_search(&pending.job_id, results)?,
            Err(failure) => orchestrator.fail_search(&pending.job_id, failure.code, failure.message)?,
        }
    }

    if let Some(job_id) = accepted.job_id.as_deref() {
        if let Some(results) = orchestrator.search_results(job_id) {
            println!(
                "Nocturne backend smoke scaffold prepared search results for '{job_id}' ({})",
                results.results.len()
            );
        }
    }

    let snapshot = orchestrator.snapshot();
    println!(
        "Nocturne backend scaffold ready (api {}, profile {}, infra {}, queue {}, search_jobs {}, events {}, playing {:?})",
        ApiVersion::V1,
        core.workspace_profile(),
        InfrastructureProfile::Local,
        snapshot.queue.len(),
        snapshot.search_jobs.len(),
        event_log.len(),
        playback_state.snapshot().current_item.as_ref().map(|item| &item.song.title),
    );

    Ok(())
}
