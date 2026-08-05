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
        // Resolve `$HOME` only when the path actually needs `~` expansion — an
        // absolute or relative path must keep working even if home cannot be
        // determined.
        let expanded_path = if self.screenshot_path.starts_with('~') {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
            expand_screenshot_dir(&self.screenshot_path, &home)
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

/// Expand a configured screenshot directory (which may start with `~`) into
/// an absolute path rooted at `home`.
///
/// `~/Pictures` becomes `home/Pictures`. The remainder after `~` starts with
/// `/`, and `Path::join` treats a leading `/` as an *absolute* path that
/// replaces the base — joining would yield `/Pictures` at the filesystem root
/// (permission denied on `create_dir_all`), so the separator is stripped.
fn expand_screenshot_dir(path: &str, home: &std::path::Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => {
            let rest = rest.trim_start_matches('/');
            if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn expand_screenshot_dir_handles_tilde() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_screenshot_dir("~/Pictures", home),
            PathBuf::from("/home/tester/Pictures")
        );
        // Bare `~` maps to the home dir itself.
        assert_eq!(
            expand_screenshot_dir("~", home),
            PathBuf::from("/home/tester")
        );
        // Trailing/extra separators are tolerated.
        assert_eq!(
            expand_screenshot_dir("~/~/shots", home),
            PathBuf::from("/home/tester/~/shots")
        );
        // Non-tilde paths pass through untouched.
        assert_eq!(
            expand_screenshot_dir("/abs/path", home),
            PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_screenshot_dir("relative/dir", home),
            PathBuf::from("relative/dir")
        );
    }
}
