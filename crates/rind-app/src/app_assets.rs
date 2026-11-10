//! 界面用到的图标资源。gpui-kit 的 `Assets` 只内嵌它自己组件要用的那 101 个 Lucide 图标；
//! Rind 另外用到的要在这里登记，否则 `Icon::new(IconName::Ghost)` 这类图标画出来是空白的。
//! 新用了一个图标就加进 `ExtraIcons`——测试会扫源码，漏登记的会让测试失败。

use std::borrow::Cow;

use gpui_kit::assets::{icon_assets, Assets};
use gpui_kit::{AssetSource, Result, SharedString};

icon_assets!(
    ExtraIcons,
    [
        AppWindow,
        ArchiveRestore,
        Brush,
        Clock,
        Download,
        Ghost,
        House,
        Library,
        Lock,
        Rocket,
        ScanSearch,
        ShieldCheck,
        Sparkles,
        Trash,
        X
    ]
);

/// 先找 Rind 自己登记的图标，找不到再交给 gpui-kit 的默认资源。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    use gpui_kit::assets::IconName;

    use super::*;

    /// 源码里写到的 `IconName::X` / `LucideIcon::X`（去重）。
    fn icons_named_in_source() -> BTreeSet<String> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut names = BTreeSet::new();
        for entry in std::fs::read_dir(&src).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for prefix in ["IconName::", "LucideIcon::"] {
                for (at, _) in text.match_indices(prefix) {
                    let rest = &text[at + prefix.len()..];
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if name.starts_with(|c: char| c.is_ascii_uppercase()) && name != "ALL" {
                        names.insert(name);
                    }
                }
            }
        }
        names
    }

    #[test]
    fn every_icon_the_ui_uses_is_embedded() {
        let by_name: HashMap<String, IconName> = IconName::ALL
            .iter()
            .map(|icon| (format!("{icon:?}"), *icon))
            .collect();
        let used = icons_named_in_source();
        assert!(used.len() > 10, "the scan found too few icons: {used:?}");
        let mut blank = Vec::new();
        for name in &used {
            let icon = by_name
                .get(name)
                .unwrap_or_else(|| panic!("{name} is not a bundled icon name"));
            let path = icon.path();
            // 没内嵌时 gpui-kit 的 `Assets` 返回错误而不是 `None`，两种都算画不出来。
            if !matches!(AppAssets.load(&path), Ok(Some(_))) {
                blank.push(format!("{name} ({path})"));
            }
        }
        assert!(
            blank.is_empty(),
            "these icons would render blank; add them to ExtraIcons: {blank:?}"
        );
    }

    #[test]
    fn listing_includes_both_sources_once() {
        let listed = AppAssets.list("icons/").expect("list");
        assert!(listed.iter().any(|p| p.as_ref() == "icons/ghost.svg"));
        assert!(listed.iter().any(|p| p.as_ref() == "icons/check.svg"));
        let unique: BTreeSet<_> = listed.iter().collect();
        assert_eq!(unique.len(), listed.len());
    }
}
