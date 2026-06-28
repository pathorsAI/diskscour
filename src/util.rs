//! Small shared helpers: human-readable sizes, colors, OS integration.

use std::path::Path;

use eframe::egui::Color32;
use eframe::egui::ecolor::Hsva;

/// Format a byte count as a short human-readable string (base 1024).
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// Deterministic, pleasant color derived from a string (FNV-1a hash → hue).
pub fn color_from_str(s: &str, is_dir: bool) -> Color32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32 / 360.0;
    let value = if is_dir { 0.80 } else { 0.62 };
    Color32::from(Hsva::new(hue, 0.55, value, 1.0))
}

/// Pick black/white text for legibility on a given background.
pub fn text_on(bg: Color32) -> Color32 {
    let lum = bg.r() as u32 + bg.g() as u32 + bg.b() as u32;
    if lum > 380 {
        Color32::from_black_alpha(230)
    } else {
        Color32::from_white_alpha(235)
    }
}

#[cfg(target_os = "macos")]
pub fn reveal_in_finder(path: &Path) {
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
}

#[cfg(target_os = "macos")]
pub fn open_path(path: &Path) {
    let _ = std::process::Command::new("open").arg(path).spawn();
}

#[cfg(not(target_os = "macos"))]
pub fn reveal_in_finder(_path: &Path) {}

#[cfg(not(target_os = "macos"))]
pub fn open_path(_path: &Path) {}

/// Native folder picker via `osascript` — no GUI dependency. Returns `None` on cancel.
#[cfg(target_os = "macos")]
pub fn pick_folder() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg("POSIX path of (choose folder with prompt \"Choose a folder to scan\")")
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // user cancelled
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(s))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pick_folder() -> Option<std::path::PathBuf> {
    None
}

/// Total and available bytes for the filesystem containing `path` (via `df`).
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("df")
        .args(["-k", "-P"])
        .arg(path)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    // -P columns: Filesystem 1024-blocks Used Available Capacity Mounted-on
    let total = cols.get(1)?.parse::<u64>().ok()? * 1024;
    let avail = cols.get(3)?.parse::<u64>().ok()? * 1024;
    Some((total, avail))
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
