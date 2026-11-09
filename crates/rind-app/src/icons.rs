//! App 图标：把包里的 `.icns` 用 `/usr/bin/sips` 转成 64px PNG，缓存在 Rind 的数据目录里。
//! 只读 App 包、只写自己的缓存目录；任何一步失败都返回 None，界面用默认图标。

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 返回 `app` 的 PNG 图标路径；缓存比 `.icns` 新就直接用。
pub fn icon_png(cache_dir: &Path, app: &Path) -> Option<PathBuf> {
    let icns = icns_path(app)?;
    let out = cache_dir.join(cache_name(app));
    if is_fresh(&out, &icns) {
        return Some(out);
    }
    std::fs::create_dir_all(cache_dir).ok()?;
    let status = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png", "-Z", "64"])
        .arg(&icns)
        .arg("--out")
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    (status.success() && out.is_file()).then_some(out)
}

/// `Contents/Info.plist` 的 `CFBundleIconFile` 指向的 `.icns`（名字可以不带扩展名）。
fn icns_path(app: &Path) -> Option<PathBuf> {
    let info = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
    let name = info
        .as_dictionary()?
        .get("CFBundleIconFile")?
        .as_string()?
        .to_string();
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let resources = app.join("Contents/Resources");
    [
        resources.join(&name),
        resources.join(format!("{name}.icns")),
    ]
    .into_iter()
    .find(|p| p.extension().is_some_and(|e| e == "icns") && p.is_file())
}

fn is_fresh(png: &Path, icns: &Path) -> bool {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (modified(png), modified(icns)) {
        (Some(png), Some(icns)) => png >= icns,
        _ => false,
    }
}

/// 缓存文件名：App 文件名 + 路径的散列，同名 App 放在不同位置也不会撞。
fn cache_name(app: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app.hash(&mut hasher);
    let stem: String = app
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    format!("{stem}-{:016x}.png", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_names_are_stable_and_distinct() {
        let a = cache_name(Path::new("/Applications/Foo Bar.app"));
        let b = cache_name(Path::new("/Users/x/Applications/Foo Bar.app"));
        assert_eq!(a, cache_name(Path::new("/Applications/Foo Bar.app")));
        assert_ne!(a, b);
        assert!(a.starts_with("Foo_Bar-") && a.ends_with(".png"));
    }

    #[test]
    fn missing_bundle_has_no_icon() {
        let dir = std::env::temp_dir().join("rind-app-icon-test-missing");
        assert!(icon_png(&dir, Path::new("/definitely/not/here.app")).is_none());
    }
}
