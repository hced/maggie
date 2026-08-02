# SPEC.md: Maggie – Technical & Functional Specification

## 1. Project Overview & Scope
**Maggie** is a native Wayland screen magnifier and utility tool written in **Rust (Edition 2024)**. It is designed to run seamlessly on minimal desktop environments and tiling window managers (such as **Niri**) without relying on legacy X11 layers or fallback architectures.

---

## 2. Core Architecture & Environment
* **Language:** Rust (Edition 2024)
* **Display Server Protocol:** Native Wayland exclusively.
* **Task Management:** Managed via a project root `Justfile` (e.g., `just build`, `just run`, `just check`, `just test`).

---

## 3. Configuration System
* **Location:** XDG standard configuration directory (`~/.config/maggie/`).
* **Format:** `.ron` (Rusty Object Notation) configuration file.
* **Storage Requirements:**
  * **Default Zoom Level** (floating point).
  * **Keybindings** for all in-app functions.
  * **Screenshot Path** (default: `~/Pictures`).
  * **Screenshot Filename Pattern:** Utilizes standard `strftime` formatting tokens (supporting dynamic variables such as `%Y` for year, `%m` for month, `%d` for day, `%H` for hour, `%M` for minute, etc., defaulting to `maggie_%Y%m%d_%H%M%S.png`).

---

## 4. Visuals & Rendering
* **Zoom & Scaling:** Controlled via numeric keyboard keys `1` through `9` to switch zoom levels.
* **Rendering Preference:** No anti-aliasing; **nearest-neighbor rendering** (pixellated visuals) is preferred by default to keep magnification blocks crisp. 
* **Anti-Aliasing Toggle:** An optional toggle bound to the `A` key, implemented only if it does not introduce development complexity.

---

## 5. Behavior Modes
The application supports three distinct behavior modes configurable via the settings:
1. **Center Cursor (Default):** Keeps the mouse cursor locked at the screen center while moving around.
2. **Edge Pan:** The screen stays still until the mouse reaches a set distance threshold from any screen edge.
3. **Miniature Window:** Displays a miniature window with rounded corners representing the zoomed portion of the screen.

---

## 6. OSD (On-Screen Display) Key Legend
* Toggable via the `K` key.
* Off by default, unless configured to always show via the configuration file.
* Dynamically stays or moves out of the way of the cursor position.

---

## 7. Screenshot Subsystem
Triggered using dedicated keybindings (`S`, `W`, `F`):
* **Manual Selection (`S`):** Allows dragging a rectangular selection region. Once drawn, the side closest to the mouse cursor can be nudged using the **Arrow keys** (1 px per keypress).
* **Window Selection (`W`):** Displays a dynamically sized grid representing all available windows for capture. Clicking an item captures and saves that specific window.
* **Fullscreen Capture (`F`):** Captures the entire active workspace or output.
* **Cancellation:** Pressing **Escape** during Manual or Window selection mode cancels the operation.
* **Output Path Generation:** Combines the configured `screenshot_path` with the evaluated `strftime` filename pattern.

---

## 8. Configuration Window
* Activated via the `C` key.
* Displays all current and future configuration options.
* **Instant Application:** Value modifications take effect immediately without requiring explicit "Apply" or "Cancel" buttons.
* **Factory Reset:** Each setting includes an adjacent reset button to revert individual values back to factory defaults.
* **Persistence:** All runtime adjustments immediately update the underlying configuration file on disk.

---

## 9. CLI Arguments
The application supports the following command-line interface arguments:
* `-z` or `--zoom <LEVEL>`: Specifies a preferred initial zoom level (e.g., `3`).
* `-d` or `--debug`: Enables debug mode to output verbose troubleshooting logs to `stdout`.
* `--version`: Prints the program version.
* `--help`: Displays help documentation.
