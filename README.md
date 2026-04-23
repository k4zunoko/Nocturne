# Nocturne Workspace

This repository is organized as a Cargo workspace from day one so the v1
client/server split stays real instead of being retrofitted later.

## Layout

- `apps/backend` - headless playback backend process
- `apps/tui` - ratatui/crossterm client process
- `crates/domain` - pure domain models shared across surfaces
- `crates/api` - internal HTTP/SSE contract types shared by backend and clients
- `crates/core` - headless application boundary for orchestration and ports
- `crates/infrastructure` - adapters for `yt-dlp`, audio output, cache, and OS integration

## Why this split

- The spec requires backend and TUI to be separate processes communicating over HTTP/SSE.
- Backend state is the source of truth, so shared models and API contracts need their own crates.
- `crates/core` stays transport-agnostic so HTTP/SSE remains an adapter detail of the backend side.
- Future surfaces like a web controller can depend on `crates/api` and `crates/domain` without pulling in TUI code.
