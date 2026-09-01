//! Input handling for winit 0.30 with PhysicalKey API.

use winit::event::{ElementState, Modifiers as WinitModifiers};
use winit::keyboard::{KeyCode, PhysicalKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ZoomIn,
    ZoomOut,
    ZoomReset,
    PanLeft,
    PanRight,
    PanUp,
    PanDown,
    ToggleOsd,
    ToggleCursor,
    ToggleMinimap,
    ConfigWindow,
    Quit,
    ScreenshotStart,
    ScreenshotConfirm,
    ScreenshotCancel,
    AnnotationToggle,
    CaptureToggle,
    Undo,
    Redo,
    None,
}

#[derive(Debug, Default, Clone)]
pub struct PointerState {
    pub position: (f64, f64),
    pub left_pressed: bool,
    pub right_pressed: bool,
    pub middle_pressed: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ModifiersState {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl From<WinitModifiers> for ModifiersState {
    fn from(m: WinitModifiers) -> Self {
        let s = m.state();
        Self {
            shift: s.shift_key(),
            ctrl: s.control_key(),
            alt: s.alt_key(),
            logo: s.super_key(),
        }
    }
}

pub fn winit_key_to_action(key: PhysicalKey, modifiers: &ModifiersState) -> Action {
    use Action::*;
    let code = match key {
        PhysicalKey::Code(c) => c,
        _ => return None,
    };
    match code {
        KeyCode::Equal | KeyCode::NumpadAdd if modifiers.ctrl => ZoomReset,
        KeyCode::Equal | KeyCode::NumpadAdd => ZoomIn,
        KeyCode::Minus | KeyCode::NumpadSubtract => ZoomOut,
        KeyCode::Digit0 => ZoomReset,
        KeyCode::ArrowLeft => PanLeft,
        KeyCode::ArrowRight => PanRight,
        KeyCode::ArrowUp => PanUp,
        KeyCode::ArrowDown => PanDown,
        KeyCode::F1 | KeyCode::F5 => ToggleOsd,
        KeyCode::F2 => ToggleCursor,
        KeyCode::F3 => ToggleMinimap,
        KeyCode::F4 => ConfigWindow,
        KeyCode::Escape | KeyCode::KeyQ => Quit,
        KeyCode::KeyS if !modifiers.ctrl => ScreenshotStart,
        KeyCode::Enter => ScreenshotConfirm,
        KeyCode::Backspace => ScreenshotCancel,
        KeyCode::KeyA if !modifiers.ctrl => AnnotationToggle,
        KeyCode::KeyC if !modifiers.ctrl => CaptureToggle,
        KeyCode::KeyZ if modifiers.ctrl => Undo,
        KeyCode::KeyY if modifiers.ctrl => Redo,
        _ => None,
    }
}
