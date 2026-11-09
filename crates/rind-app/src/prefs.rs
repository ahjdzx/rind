//! 界面偏好落盘：外观和语言。两个文件都放在 `<data-dir>`（`~/Library/Application Support/Rind`）：
//! `appearance` 一行 `system` / `light` / `dark`，`language` 一行 `zh` / `en`。
//! 示例模式轮到调用方把关（不调用这里）；文件不在、读不了、内容认不出来，都按没存过处理。

use std::path::Path;

use rind_i18n::Lang;

use crate::settings_page::Appearance;

/// 存放外观的文件名，位置在 `<data-dir>`（`~/Library/Application Support/Rind`）。
pub const APPEARANCE_FILE: &str = "appearance";

/// 存放语言的文件名，和外观放在一起。
pub const LANGUAGE_FILE: &str = "language";

/// 读一次外观偏好。文件不在、读不了、内容认不出来，都返回 `None`（调用方落回「跟随系统」）。
pub fn load(data_dir: &Path) -> Option<Appearance> {
    let text = std::fs::read_to_string(data_dir.join(APPEARANCE_FILE)).ok()?;
    Appearance::parse(text.trim())
}

/// 写一次外观偏好。失败的原因交给调用方（只记日志，不打断界面）。
pub fn save(data_dir: &Path, appearance: Appearance) -> std::io::Result<()> {
    write_atomic(data_dir, APPEARANCE_FILE, appearance.as_str())
}

/// 读一次语言偏好。文件不在、读不了、内容认不出来，都返回 `None`。
pub fn load_lang(data_dir: &Path) -> Option<Lang> {
    let text = std::fs::read_to_string(data_dir.join(LANGUAGE_FILE)).ok()?;
    Lang::from_code(&text)
}

/// 写一次语言偏好。失败的原因交给调用方（只记日志，不打断界面）。
pub fn save_lang(data_dir: &Path, lang: Lang) -> std::io::Result<()> {
    write_atomic(data_dir, LANGUAGE_FILE, lang.code())
}

/// 先写临时文件再改名：中断在任何时刻，目标文件要么是旧值、要么是新值，不会是半截。
fn write_atomic(data_dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let tmp = data_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, format!("{contents}\n"))?;
    std::fs::rename(tmp, data_dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rind-app-prefs-test-{name}"));
        let _ = std::fs::remove_file(dir.join(APPEARANCE_FILE));
        let _ = std::fs::remove_file(dir.join(LANGUAGE_FILE));
        dir
    }

    #[test]
    fn round_trips_every_variant() {
        let dir = temp_dir("round-trip");
        for appearance in [Appearance::System, Appearance::Light, Appearance::Dark] {
            save(&dir, appearance).unwrap();
            assert_eq!(load(&dir), Some(appearance));
        }
        // 改名后不留临时文件。
        assert!(!dir.join("appearance.tmp").exists());
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

    #[test]
    fn language_round_trips_every_variant() {
        let dir = temp_dir("lang-round-trip");
        for lang in [Lang::ZhCn, Lang::En] {
            save_lang(&dir, lang).unwrap();
            assert_eq!(load_lang(&dir), Some(lang));
        }
        // 改名后不留临时文件。
        assert!(!dir.join("language.tmp").exists());
        let _ = std::fs::remove_file(dir.join(LANGUAGE_FILE));
    }

    #[test]
    fn missing_or_unknown_language_means_no_preference() {
        let dir = temp_dir("lang-missing");
        assert_eq!(load_lang(&dir), None);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LANGUAGE_FILE), "fr\n").unwrap();
        assert_eq!(load_lang(&dir), None);
        let _ = std::fs::remove_file(dir.join(LANGUAGE_FILE));
    }
}
