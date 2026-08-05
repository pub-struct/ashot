# ashot

Agentic screenshot tool in Rust: a headless CLI that AI agents call to capture
and annotate screenshots, plus a CleanShot-style desktop app for humans built
with [GPUI](https://www.gpui.rs) (Zed's UI framework). Linux/Wayland first.
See `DESIGN.md` for the full decision record.

- **For agents**: `ashot capture` / `ashot annotate` — no window, no GPU,
  JSON in and JSON out. One call captures, crops, and draws rectangles,
  ellipses, arrows, numbered markers, and text onto the screenshot.
- **For humans**: `ashot ui` — freeze-frame drag-select overlay, instant
  copy-to-clipboard with a floating preview card, and an annotation editor
  sharing the exact same renderer agents use.

## Install / build

```sh
cargo build --release        # binary at target/release/ashot
```

## One-time setup (human)

On GNOME the desktop portal may require a one-time consent for silent captures:

```sh
ashot setup
```

Approve the dialog if one appears; after that, captures run unattended.

## Commands

Every command prints JSON metadata on stdout (or stderr when the PNG itself is
streamed to stdout). Errors are JSON on stderr with exit code 1:
`{"ok":false,"error":{"code":"portal_denied","message":"..."}}`.

```sh
# Capture
ashot capture                          # full desktop -> ~/Pictures/Screenshots/shot-<ts>.png
ashot capture -o out.png               # explicit path
ashot capture -o -                     # PNG bytes to stdout (metadata to stderr)
ashot capture --region 100,100,800,600 # crop, in image pixels (X,Y,W,H)
ashot capture --monitor 0              # one monitor (index from `ashot monitors`)
ashot capture --clipboard              # also copy PNG to the clipboard
ashot capture --annotate SPEC          # capture + annotate in one call

# Annotate an existing PNG (input is never modified)
ashot annotate in.png --spec SPEC -o out.png
ashot annotate in.png --spec @spec.json     # spec from file
echo "$SPEC" | ashot annotate in.png --spec -  # spec from stdin
ashot annotate in.png --spec SPEC --keep-spec  # writes out.png.shot.json sidecar

# Introspection
ashot monitors                         # monitor layout as JSON

# Desktop app (GPUI)
ashot ui                               # freeze-frame overlay: drag to select,
                                        #   then Save / Copy / Edit / Cancel
ashot ui image.png                     # open the annotation editor on a PNG
```

### Overlay shortcuts
Drag to select · **Enter** save (selection or full screen) · **C** copy ·
**E** edit selection · **Esc** cancel.

### Editor
Toolbar tools (also keys **R**/**O**/**A**/**M**/**T**): rectangle, ellipse,
arrow, numbered marker, text. Seven colors, S/M/L stroke. **Ctrl+Z** undo,
**Ctrl+C** copy, **Ctrl+S** save, **Esc** quit. Text tool: click, type,
**Enter** commits. Saving burns annotations through the same core renderer the
CLI uses, so human and agent output are pixel-identical.

## Annotation spec

JSON array (or `{"annotations": [...]}`). **All coordinates are pixels in the
target image** — the same pixel space reported by the capture metadata
(`width`, `height`, `scale_factor`).

```json
[
  {"type": "rect",    "x": 10, "y": 10, "w": 200, "h": 100,
   "color": "red", "stroke_width": 5, "fill_opacity": 0.1, "label": "main area"},
  {"type": "ellipse", "x": 300, "y": 50, "w": 120, "h": 80, "color": "#0a84ff"},
  {"type": "arrow",   "from": [500, 400], "to": [350, 250], "label": "click here"},
  {"type": "marker",  "x": 120, "y": 80},
  {"type": "marker",  "x": 240, "y": 80, "number": 7, "size": 20},
  {"type": "text",    "x": 10, "y": 300, "text": "note", "size": 28, "color": "#333"}
]
```

- `color`: `#RGB`, `#RRGGBB`, `#RRGGBBAA`, or a name
  (red, orange, yellow, green, blue, purple, pink, black, white, gray).
  Default `#ff3b30` (red).
- `stroke_width`: default 4. `fill_opacity`: 0–1, default 0 (outline only).
- `marker`: numbered badge; numbers auto-assign in spec order when omitted.
- `label` (rect/ellipse/arrow): white text pill in the shape's color; arrows
  label at the tail so the target stays visible.

## Error codes

`portal_denied` (run `ashot setup`), `portal_unavailable`, `invalid_spec`,
`invalid_color`, `monitor_not_found`, `wayland_unavailable`,
`region_out_of_bounds`, `image`, `io`, `internal`.

## Workspace

- `crates/ashot-core` — capture (xdg-desktop-portal + wl_output enumeration),
  annotation model, headless renderer (tiny-skia + cosmic-text), clipboard,
  paths. No GPU, no window.
- `crates/ashot` — the CLI.
- `crates/ashot-app` — the GPUI desktop app (overlay + editor), Claude-desktop
  visual language. GPUI is pinned to a Zed rev in the workspace manifest; all
  GPUI-specific code stays in this crate.

### Build notes (Linux)
- Toolchain: current stable via rustup (`rust-toolchain.toml`); GPUI tracks
  Zed main (pinned rev in the workspace manifest).
- No `-dev` packages needed: `crates/ashot-app/build.rs` symlinks the runtime
  `lib*.so.N` system libraries (`xkbcommon`, `xcb`, …) into the build dir, and
  `RUST_FONTCONFIG_DLOPEN=on` (set via `.cargo/config.toml`) makes fontconfig
  dlopen at runtime.
- ashpd runs the async-io flavor (not tokio) to stay compatible with GPUI's
  zbus usage inside the app process.

## License

Dual-licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
