# SPEC.md: Maggie – Technical & Functional Specification

## 1. Project Overview & Scope
**Maggie** is a native Wayland screen magnifier and utility tool written in **Rust (Edition 2024)**. It is designed to run seamlessly on minimal desktop environments and tiling window managers (such as **Niri**) without relying on legacy X11 layers or fallback architectures.

---

## 2. Core Architecture & Environment
* **Language:** Rust (Edition 2024)
* **Display Server Protocol:** Native Wayland exclusively.
* **Task Management:** Managed via a project root `Justfile` (e.g., `just build`, `just run`, `just check`, `just test`).

---

## 3. Screen Capture & Modes — *Implemented*
* **Frozen-frame model:** The screen is captured exactly **once** at startup via `zwlr_screencopy` (`capture_output` on the current output). The SHM buffer (XRGB8888/ARGB8888) is converted to RGBA with a stride-aware row copy, honoring the `y_invert` flag.
* **Capture-before-content:** The fullscreen layer-shell overlay (`Layer::Overlay`) is committed at startup but presents **no image data until the first captured frame is ready**; the initial screencopy therefore never contains the overlay, avoiding the Droste-effect self-feedback that plagues live-capture magnifiers.
* **Frozen view:** After the first frame, the overlay displays a nearest-neighbor zoomed view of the frozen frame. Zoom keys `1`–`9` re-scale the same frozen frame; the screen is never re-captured at runtime.
* **Failure handling:** A failed capture is retried up to **3 times**; if all retries fail, the overlay renders black.
* **Zoom centering:** The viewport follows the **live cursor** over the frozen frame — the view is centered on the content under the cursor and clamped at the capture edges; if the cursor was never seen, it centers on the capture. Zoom keys `1`–`9` re-scale.
* **Viewport clamping:** The viewport is clamped to the capture bounds so it always fills the screen and sticks at the edges; consequently, the pointer drifts off-center as it approaches the screen edges.
* **No live mode:** Continuous live capture is explicitly **out of scope**; the interactive live-magnifier idea (including an earlier planned `--live` flag) was dropped. Behavior modes that assumed a live view (see §6) are obsolete until redefined.

---

## 4. Configuration System — *Partial*
* **Location:** XDG standard configuration directory (`~/.config/maggie/`).
* **Format:** `.ron` (Rusty Object Notation) configuration file.
* **Storage Requirements:**
  * **Default Zoom Level** (floating point).
  * **Keybindings** for all in-app functions.
  * **Screenshot Path** (default: `~/Pictures`).
  * **Screenshot Filename Pattern:** Utilizes standard `strftime` formatting tokens (supporting dynamic variables such as `%Y` for year, `%m` for month, `%d` for day, `%H` for hour, `%M` for minute, etc., defaulting to `maggie_%Y%m%d_%H%M%S.png`).
* **Status:** Loading from disk is implemented; the listed `strftime` tokens are evaluated on save. **Write-on-change persistence is not wired up** — `save_config` exists but is unused, so runtime adjustments never reach disk.

---

## 5. Visuals & Rendering — *Implemented* (AA toggle: *Stub*)
* **Zoom & Scaling:** Controlled via numeric keyboard keys `1` through `9` to switch zoom levels; keys re-scale the frozen frame.
* **Rendering Preference:** No anti-aliasing; **nearest-neighbor rendering** (pixellated visuals) is preferred by default to keep magnification blocks crisp.
* **Anti-Aliasing Toggle:** An optional toggle bound to the `A` key, implemented only if it does not introduce development complexity. — *Stub: key is bound but not yet functional.*

---

## 6. Behavior Modes — *Revised*
* The former **Center Cursor** / **Edge Pan** / **Miniature Window** modes remain **obsolete/removed** in the frozen-frame model; they are pending redefinition or removal. Mode-switch bindings (`Ctrl+C`/`Ctrl+E`/`Ctrl+M`) still exist in code but have no rendering effect.
* **Cursor-following:** The former "Center Cursor" concept is now the **built-in default behavior** of the frozen viewport — the view follows the live cursor over the frozen frame, clamped at the capture edges. See §3.

---

## 7. OSD (On-Screen Display) Key Legend — *Implemented*
* Toggable via the `K` key (configurable via `keybindings.toggle_osd`).
* Off by default, unless configured to always show via the configuration file (`show_osd`).
* Dynamically stays or moves out of the way of the cursor position: the legend box is drawn in the quadrant opposite the cursor.
* Rendered with a built-in 5×7 bitmap font (`src/osd.rs`) drawn at **2× scale** (5×7 glyphs scaled 2×, i.e. rendered at 10×14); lists zoom level, `1`–`9` zoom, `K` OSD toggle, `F` fullscreen screenshot, `S`/`W`/`C` (pending), and `Q`/`Esc` quit.

---

## 8. Screenshot Subsystem — *Partial*
Triggered using dedicated keybindings (`S`, `W`, `F`):
* **Manual Selection (`S`):** Allows dragging a rectangular selection region. Once drawn, the side closest to the mouse cursor can be nudged using the **Arrow keys** (1 px per keypress). — *Stub: not yet implemented.*
* **Window Selection (`W`):** Displays a dynamically sized grid representing all available windows for capture. Clicking an item captures and saves that specific window. — *Stub: not yet implemented.*
* **Fullscreen Capture (`F`):** Saves the currently frozen frame as a PNG. — *Implemented.*
* **Cancellation:** Pressing **Escape** during Manual or Window selection mode cancels the operation. — *Pending together with `S`/`W`; Escape currently quits the app.*
* **Output Path Generation:** Combines the configured `screenshot_path` with the evaluated `strftime` filename pattern (`~` expansion and directory creation included). — *Implemented.*

---

## 9. Configuration Window — *Stub*
* Activated via the `C` key.
* Displays all current and future configuration options.
* **Instant Application:** Value modifications take effect immediately without requiring explicit "Apply" or "Cancel" buttons.
* **Factory Reset:** Each setting includes an adjacent reset button to revert individual values back to factory defaults.
* **Persistence:** All runtime adjustments immediately update the underlying configuration file on disk.
* **Status:** Not yet implemented; the `C` key is bound but inert.

---

## 10. CLI Arguments — *Implemented*
The application supports the following command-line interface arguments:
* `-z` or `--zoom <LEVEL>`: Specifies a preferred initial zoom level (e.g., `3`).
* `-d` or `--debug`: Enables debug mode to output verbose troubleshooting logs to `stdout`.
* `--version`: Prints the program version.
* `--help`: Displays help documentation.
