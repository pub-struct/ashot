# agent-screenshot — Design Decisions

An agentic screenshot tool: a headless CLI that AI agents call to capture and
annotate screenshots, plus a CleanShot-X-style desktop app for humans, built in
Rust with GPUI (Zed's UI framework).

## Decisions (settled 2026-08-05)

### Platform
- **Linux first** (dev machine = GNOME Wayland), macOS later, Windows not planned for v1.
- Capture layer lives behind a trait from day one so ScreenCaptureKit (macOS) can slot in.
- Linux capture: **xdg-desktop-portal Screenshot API** (works on GNOME/KDE/wlroots),
  X11 grab as fallback for X11 sessions. Note: `grim` is wlroots-only — not usable on GNOME.

### Architecture
- **One binary (`ashot`), three modes**:
  - `ashot capture` / `ashot annotate` — fully headless, no window, no GPU required.
  - `ashot` (no args) or `ashot ui` — launches the GPUI desktop app.
- Cargo workspace: `core` (capture trait, annotation model, raster rendering),
  `cli`, `app` (GPUI, isolated so GPUI churn never touches core).
- GPUI is a **git dependency pinned to a known-good rev**; accepted trade-off
  (API churn, sparse docs) for best-in-class GPU-native UI.

### Agent interface
- Annotation spec is **JSON**, passed via flag, stdin, or `@file`.
- Every command prints **JSON metadata to stdout**: `{path, width, height, scale_factor, region}`.
- Errors are structured JSON too (e.g. missing portal permission → actionable message).

### Wayland consent
- **`ashot setup`**: one-time interactive portal grant run by the human;
  permission persists in the xdg permission store per app-id. After that,
  agent captures are silent and unattended.
- `ashot capture` without permission fails fast with a JSON error telling the
  agent to ask the human to run `ashot setup`.
- App-id must stay stable (renaming invalidates the grant): `io.github.miguer.ashot` (tbd final).

### Capture modes (v1)
- Agent: full screen (default), `--monitor N`, `--region x,y,w,h`.
- Human: GPUI fullscreen **drag-to-select overlay** → region capture.
- Window-by-title capture: **deferred** (blocked by Wayland security model;
  portal interactive window-pick may serve humans in v2).

### Annotation model
- Shape vocabulary: **rect, ellipse, arrow, marker-badge (numbered ①②③), text**.
  - "Point" = marker badge; arrows cover directional pointing.
  - All shapes: hex color, stroke width, optional fill opacity; text labels attachable.
- Coordinates: **image-pixel space of the captured PNG** — no logical/normalized coords.
  The metadata echo gives agents the true dimensions + scale factor.
- Rendering (headless): **tiny-skia** for raster + **cosmic-text** for text shaping.
  No GPU in the agent path.

### Persistence
- Output is always a **flat PNG**; input files are never mutated (`-o` writes new).
- `--keep-spec` writes `out.png.shot.json` sidecar → the UI editor (M3) can
  reload original + spec for non-destructive editing.
- PNG only in v1. `-o -` streams PNG to stdout for pipes.

### Output defaults
- Explicit `-o PATH`; default `~/Pictures/Screenshots/shot-YYYYMMDD-HHMMSS.png`.
- `--clipboard` flag (wl-clipboard-rs, handles stay-alive-until-paste).
- Resulting path always reported in stdout JSON.

### Naming
- Project: **agent-screenshot** (this folder). Binary: **`ashot`**.
- Standalone-repo potential acknowledged; can `git init` here and move out later.

## Milestones
- **M1 — Agent path** ✅ (2026-08-05): core crate + `ashot capture` /
  `ashot annotate` / `ashot setup` / `ashot monitors`, portal capture,
  tiny-skia rendering, JSON contract, renderer tests.
- **M2 — Overlay** ✅ (2026-08-05): freeze-frame approach (capture first, then
  fullscreen GPUI window over the frozen image — sidesteps Wayland transparency
  and layer-shell limits). Drag select, scrim, size chip, Save/Copy/Edit bar.
- **M3 — Editor** ✅ (2026-08-05): GPUI editor, Claude-desktop visual language
  (warm dark #262624, terracotta #D97757 accent). Five tools, 7 colors, S/M/L
  stroke, undo, text typing; burning goes through the core renderer.

## App-phase decisions (added during M2/M3)
- **UI design language**: Claude desktop — warm dark neutrals, terracotta
  accent, rounded surfaces, hairline borders (user request: "your tone").
- **GPUI pin**: Zed rev `6153542` (2026-08-05 main HEAD) on Rust stable
  (1.97.1 via rustup, `rust-toolchain.toml` tracks stable). Current GPUI split
  platform backends into a `gpui_platform` crate: construct with
  `Application::with_platform(gpui_platform::current_platform(false))` and
  enable its `wayland`/`x11` features explicitly. Bump deliberately.
- **fontconfig**: `RUST_FONTCONFIG_DLOPEN=on` (set in `.cargo/config.toml`
  `[env]`) so fontconfig-sys dlopens the runtime lib instead of requiring
  headers via pkg-config.
- **ashpd async flavor**: async-io (async-std feature), NOT tokio — the tokio
  feature flips zbus workspace-wide and panics GPUI's own zbus usage in-app.
- **Linker shims**: `crates/ashot-app/build.rs` auto-symlinks runtime
  `lib*.so.N` (`libxkbcommon`, `libxcb`, …) into OUT_DIR instead of requiring
  `-dev` apt packages — portable across machines, nothing hardcoded.
- **Editor live preview**: drafts (drag shapes, typing) are GPU-composited
  GPUI elements — zero rasterization per mouse-move. The core renderer burns
  once on commit (mouse-up / Enter), keeping agent and human output identical.
  Rationale + measurements in `crates/ashot-core/examples/pipeline_bench.rs`
  (per-event burn was 9–115 ms; the backlog caused freeze-then-snap dragging).

## Recording (added 2026-08-05)
- **Path**: ScreenCast portal (ashpd) → PipeWire node + fd → `gst-launch-1.0 -e`
  subprocess. No GStreamer linkage; `-e` maps SIGINT → EOS → finalized MP4.
- **GPU pipeline**: `pipewiresrc ! queue ! [videocrop] ! vapostproc !
  NV12/VAMemory caps ! vah264enc ! h264parse ! mp4mux`. DMA-BUF in, hardware
  encoder out; CPU never touches pixels. Auto-fallback to x264 (CPU) if the VA
  pipeline dies at startup (~1.2 s health check, fresh portal stream).
- **Resolutions**: 720p/1080p/1440p = GPU scaling in vapostproc; bitrates
  6/10/16 Mbps; dimensions evened for NV12. Never upscales.
- **Unattended**: PersistMode::ExplicitlyRevoked restore token persisted at
  `~/.config/ashot/screencast.token` — first run shows the system picker,
  every later run starts silently (agents included).
- **Portal session lifetime**: held by a keeper thread for the whole
  recording; `Recording` Drop SIGINTs the encoder as an orphan guard, and the
  recorder window's ✕ routes through Stop.
- **UI**: `ashot ui` = floating pill toolbar (user-sketched): mode toggle,
  Full/Crop selector, resolution + mic picker in record mode, one red action
  button. Transparent window → true pill shape. Region select reuses the
  freeze-frame overlay (Purpose::Record); recording shows a small movable
  status window (elapsed, GPU/CPU badge, Stop).
- **Mic audio**: optional pulsesrc → avenc_aac 128k branch into the same
  mp4mux. Device list from `pactl --format=json list sources` (monitors
  filtered out); default is no mic. Note: mp4mux silently omits a track that
  never produced samples (e.g. a sleeping bluetooth source) — picking an
  explicit device avoids it.

## Known risks / later
- GPUI git-dep breakage on rev bumps (mitigated by pinning + crate isolation).
- Global hotkeys on GNOME Wayland need the GlobalShortcuts portal (M2 concern).
- Capture can't run in CI — integration tests need a nested compositor or mocks;
  renderer is fully testable via golden images.
- v2 candidates: window pick (human), blur/redact + highlight shapes, MCP server
  mode (`ashot mcp` wrapping core), macOS backend, JPEG/WebP export, config file.
