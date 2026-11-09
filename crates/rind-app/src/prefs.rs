//! 界面偏好落盘：目前只有「外观」。文件是 `<data-dir>/appearance`，内容一行 `system` / `light` / `dark`。
//! 示例模式不读写这个文件；文件不在、读不了、内容认不出来，都按没存过处理。

use std::path::Path;

use crate::settings_page::Appearance;

/// 存放外观的文件名，位置在 `<data-dir>`（`~/Library/Application Support/Rind`）。
pub const APPEARANCE_FILE: &str = "appearance";

/// 读一次外观偏好。文件不在、读不了、内容认不出来，都返回 `None`（调用方落回「跟随系统」）。
pub fn load(data_dir: &Path) -> Option<Appearance> {
    let text = std::fs::read_to_string(data_dir.join(APPEARANCE_FILE)).ok()?;
    Appearance::parse(text.trim())
}

/// 写一次外观偏好。失败的原因交给调用方（只记日志，不打断界面）。
pub fn save(data_dir: &Path, appearance: Appearance) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(
        data_dir.join(APPEARANCE_FILE),
        format!("{}\n", appearance.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rind-app-prefs-test-{name}"));
        let _ = std::fs::remove_file(dir.join(APPEARANCE_FILE));
        dir
    }

    #[test]
    fn round_trips_every_variant() {
        let dir = temp_dir("round-trip");
        for appearance in [Appearance::System, Appearance::Light, Appearance::Dark] {
            save(&dir, appearance).unwrap();
            assert_eq!(load(&dir), Some(appearance));
        }
        let _ = std::fs::remove_file(dir.join(APPEARANCE_FILE));
    }

    #[test]
    fn missing_file_means_no_preference() {
        let dir = temp_dir("missing");
        assert_eq!(load(&dir), None);
    }

    #[test]
    fn unknown_content_means_no_preference() {
        let dir = temp_dir("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(APPEARANCE_FILE), "sepia\n").unwrap();
        assert_eq!(load(&dir), None);
        let _ = std::fs::remove_file(dir.join(APPEARANCE_FILE));
    }
}
