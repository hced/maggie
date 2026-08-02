#![allow(dead_code)]

use crate::engine::MagnifierState;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    ToggleOsd,
    ToggleAntiAliasing,
    ModeCenterCursor,
    ModeEdgePan,
    ModeMiniature,
    ScreenshotManual,
    ScreenshotWindow,
    ScreenshotFullscreen,
    OpenConfig,
}

pub struct InputHandler;

impl InputHandler {
    pub fn new() -> Self {
        InputHandler
    }

    pub fn handle_key(_magnifier: &mut MagnifierState, keysym: u32) -> Option<Action> {
        match keysym {
            0x6B => Some(Action::ToggleOsd),
            0x73 => Some(Action::ScreenshotManual),
            0x77 => Some(Action::ScreenshotWindow),
            0x66 => Some(Action::ScreenshotFullscreen),
            0x63 => Some(Action::OpenConfig),
            _ => None,
        }
    }
}
