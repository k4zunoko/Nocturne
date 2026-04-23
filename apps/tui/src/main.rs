use nocturne_core::NocturneCore;

fn main() {
    let core = NocturneCore::new();
    let playback = core.initial_playback_state();
    println!(
        "Nocturne TUI scaffold ready (profile {}, initial state {:?})",
        core.workspace_profile(),
        playback.state
    );
}
