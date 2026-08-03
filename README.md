<p align="center">
  <img src="assets/branding/maggie_logo/maggie_logo_512.png" alt="Maggie Logo" width="512">
</p>

# Maggie

Native Wayland screen magnifier and utility tool, written in Rust.

---

**⚠ Status:** Maggie is built for **<u>Linux only</u>**. It's at **v0.1.0** and is developed and tested primarily on my own Linux distro with the **Niri** compositor. It's **Wayland-only** — X11 is not supported, and other compositors (Sway, Hyprland, KWin, GNOME…) are untested and may not work correctly.

---

## Overview

Maggie is a **frozen-frame** screen magnifier: it captures the screen exactly once at startup via `zwlr_screencopy` and shows a fullscreen, cursor-following, pixelated view of that frame as a layer-shell overlay — no live capture, no compositor window rules required. It runs natively on Wayland compositors exposing `wlr-layer-shell` + `wlr-screencopy`, such as minimal desktop environments and tiling window managers in the spirit of Niri.

Rendering is GPU-accelerated via **EGL + OpenGL ES 2** (nearest-neighbor, crisp magnifier look), with a fully functional **CPU bilinear fallback** when EGL/GLES2 is unavailable.

## Features

Here's what Maggie can do today:

- **Keyboard zoom:** `1`–`9` switch zoom levels instantly.
- **Scroll-wheel zoom:** `levels` mode (steps through 1–9) or `factor` mode (10 % steps, clamped to 1×–32×); wheel direction is reversed by default, flippable via the `invert_scroll_zoom` config option; high-resolution `value120` deltas supported.
- **Cursor-following:** `snap` (instant, default), `ease` (smooth exponential), or `inertia` (with momentum glide), tracked at sub-pixel precision and clamped at the capture edges.
- **GPU rendering:** EGL + OpenGL ES 2 with nearest-neighbor sampling at 2× buffer scale; automatic, permanent CPU fallback (bilinear) if GPU init fails.
- **OSD key legend:** `K` toggles an on-screen legend that stays in the corner farthest from the cursor.
- **Fullscreen screenshot:** `F` saves the frozen frame as a PNG (default `~/Pictures/maggie_%Y%m%d_%H%M%S.png`).
- **Configuration:** RON file at `~/.config/maggie/config.ron`.
- **CLI:** `-z/--zoom <level>` initial zoom, `-d/--debug` verbose logging, `--help`, `--version`.
- **Quit:** `Q`, `Escape`, or right mouse button.

### Keybindings

| Key | Action |
|---|---|
| `1`–`9` | Set zoom level |
| Mouse wheel | Zoom in/out (mode + direction configurable) |
| `F` | Save fullscreen screenshot |
| `K` | Toggle OSD legend |
| `A` | Anti-aliasing toggle *(stub, inert)* |
| `C` | Configuration window *(stub, inert)* |
| `S` / `W` | Manual region / window screenshot *(stub, inert)* |
| `Q`, `Escape`, RMB | Quit |

## Usage

```bash
maggie                       # start with defaults
maggie -z 3                  # start at zoom level 3
maggie -d                    # verbose debug logging to stdout
```

## Installation

### From Source

```bash
git clone https://github.com/hced/maggie.git
cd maggie
cargo build --release
cp target/release/maggie /usr/local/bin/
```

System dependencies (Debian/Ubuntu): `libwayland-dev` and `libxkbcommon-dev`. Prefer a pre-built binary? Check the [Releases](https://github.com/hced/maggie/releases) page — tag pushes build and publish a `maggie-linux.tar.gz` automatically.

The repo includes a `justfile` with developer conveniences: `just build`, `just run`, `just tests`, `just check`, `just lint` — and a full release workflow (`just release`, `just push-release-tag`).

## Configuration

Maggie reads its configuration from `~/.config/maggie/config.ron` (Rusty Object Notation). If the file is absent, it falls back to sensible defaults. Options:

- `default_zoom` — initial zoom level (floating point).
- `cursor_follow` — `snap` (default) | `ease` | `inertia`.
- `scroll_zoom_mode` — `levels` (default) | `factor`.
- `invert_scroll_zoom` — boolean, default `false`.
- `show_osd` — boolean, default `true`.
- `keybindings` — bindings for all in-app functions.
- `screenshot_path` — default `~/Pictures`.
- `screenshot_filename_pattern` — supports `%Y %m %d %H %M %S` tokens, default `maggie_%Y%m%d_%H%M%S.png`.

Newer options carry `#[serde(default)]`, so config files written before they existed remain loadable. Configuration is currently **load-only** — edit the file manually (see Limitations).

## Architecture

Maggie is a single-binary Wayland client built on `wayland-client` + `smithay-client-toolkit`.

### Design highlights

- **Frozen-frame model** — the screen is captured exactly once at startup via `zwlr_screencopy`; the SHM buffer (XRGB8888/ARGB8888) is converted to RGBA with a stride-aware row copy honoring `y_invert`. Failed captures retry up to 3 times, then the overlay renders black.
- **Capture-before-content** — the overlay is committed at startup but presents no image data until the first frame arrives, so the initial screencopy never contains the overlay — avoiding the Droste-effect self-feedback that plagues live-capture magnifiers.
- **Fullscreen layer-shell overlay** — `Layer::Overlay` with all anchors and an **exclusive zone of −1** ("dont care"), so the compositor hands it the full physical screen instead of shrinking it around bars/docks. The surface re-asserts its size on every configure and redraws immediately. Keyboard interactivity is `on-demand`, keeping compositor-level global keybindings alive.
- **Lazy GPU init** — the `wl_egl_window` is created at the first configure, when the real output size is known, so the very first presented buffer is already fullscreen. EGL is loaded dynamically (`khronos-egl`); GLES2 bindings are generated at build time by `gl_generator`.
- **Swap interval 0** — frame redraws never block the event loop, so input works during panning animations.
- **Event-driven input** — pointer motion at sub-pixel precision, wheel deltas, and keyboard events are handled in a `blocking_dispatch` loop; panning animations are driven by `wl_surface` frame callbacks.

### Module breakdown

| Module | Responsibility |
|---|---|
| `src/main.rs` | CLI parsing (clap), `tracing` setup, entry point |
| `src/engine.rs` | Core state machine: Wayland globals, layer-shell surface, screencopy handling, input dispatch, view math (zoom/centering/clamping, ease/inertia), draw orchestration, screenshot saving |
| `src/capture.rs` | Screenshot output path generation (`~` expansion, filename tokens, directory creation) |
| `src/render.rs` | `RgbaBuffer`, CPU bilinear renderer and nearest-neighbor scaling |
| `src/gpu.rs` | EGL/GLES2 renderer: shader compilation, textured-quad draw, OSD pass, lazy init, resize |
| `src/osd.rs` | 5×7 bitmap font, OSD sprite construction, farthest-corner placement (unit-tested) |
| `src/input.rs` | Legacy keysym → `Action` dispatch layer (actual key handling lives in `engine.rs`) |
| `src/config.rs` | RON config schema, defaults, `load_config` / `save_config` |

## Roadmap

Broader compositor and distro support is a direction I'd like to take Maggie in, but **none of it is implemented yet**:

| Target | Status |
|---|---|
| Niri | Tested |
| Sway | Not implemented |
| Hyprland | Not implemented |
| GNOME (Mutter) | Not implemented |
| KDE (KWin) | Not implemented |

The frozen-frame + layer-shell design is compositor-agnostic at the protocol level, but each environment needs its own capture path (GNOME notably lacks `zwlr_screencopy`), and layering/anchoring behavior differs between compositors.

Planned or under consideration (per `SPEC.md` — not commitments):

- **Manual selection screenshot (`S`)** — drag a rectangular region; nudge its sides with the arrow keys. *Stub.*
- **Window selection screenshot (`W`)** — grid of available windows; click to capture and save one. *Stub.*
- **Configuration window (`C`)** — live config editing with instant application, per-setting reset, and persistence. *Stub.*
- **Anti-aliasing toggle (`A`)** — nearest-neighbor / bilinear switch. *Stub.*
- **Write-on-change config persistence** — `save_config` exists but is unused; runtime adjustments never reach disk.
- **Selection-mode cancellation** — Escape should cancel an in-progress `S`/`W` selection instead of quitting.
- **Legacy mode bindings** — the obsolete Center Cursor / Edge Pan / Miniature Window modes (`Ctrl+C`/`Ctrl+E`/`Ctrl+M`) are pending redefinition or removal.

### Packaging

Tag pushes (`v*`) trigger the CI workflow, which builds a `--release` binary, runs the test suite, and publishes a packaged archive with checksums to a GitHub Release.

## Limitations

What Maggie doesn't do (yet):

- **No live capture mode** — the screen is captured once at startup; the view is frozen until exit (by design).
- **Config is load-only** — runtime changes never reach disk; edit `config.ron` manually.
- **Only `F` screenshot works** — `S` and `W` are not implemented; `Escape` quits rather than cancelling.
- **`A` and `C` are inert** — bound, but log "not yet implemented".
- **Linux / Wayland-only** — other compositors untested; X11 unsupported.

## Author

H. Cederblad
