#![allow(dead_code)]

use anyhow::Result;
use chrono::{Datelike, Local, Timelike};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ScreenshotRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub struct CaptureManager {
    screenshot_path: String,
    screenshot_filename_pattern: String,
}

impl CaptureManager {
    pub fn new(screenshot_path: String, screenshot_filename_pattern: String) -> Self {
        CaptureManager {
            screenshot_path,
            screenshot_filename_pattern,
        }
    }

    pub fn generate_screenshot_path(&self) -> Result<PathBuf> {
        let expanded_path = if self.screenshot_path.starts_with('~') {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
            home.join(&self.screenshot_path[1..])
        } else {
            PathBuf::from(&self.screenshot_path)
        };

        let filename = self.format_filename(&self.screenshot_filename_pattern)?;
        std::fs::create_dir_all(&expanded_path)?;
        Ok(expanded_path.join(filename))
    }

    fn format_filename(&self, pattern: &str) -> Result<String> {
        let now = Local::now();
        let formatted = pattern
            .replace("%Y", &format!("{:04}", now.year()))
            .replace("%m", &format!("{:02}", now.month()))
            .replace("%d", &format!("{:02}", now.day()))
            .replace("%H", &format!("{:02}", now.hour()))
            .replace("%M", &format!("{:02}", now.minute()))
            .replace("%S", &format!("{:02}", now.second()));

        Ok(formatted)
    }
}
