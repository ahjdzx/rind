//! 清理类别的文案。

use rind_core::cleanup::CleanupCategory;

use crate::{pick, Lang};

/// 类别名。
pub fn describe_cleanup_category(lang: Lang, category: CleanupCategory) -> &'static str {
    let (zh, en) = match category {
        CleanupCategory::AppCaches => ("应用缓存", "App caches"),
        CleanupCategory::Logs => ("日志与诊断报告", "Logs and reports"),
        CleanupCategory::BrowserCaches => ("浏览器缓存", "Browser caches"),
        CleanupCategory::Xcode => ("Xcode 与模拟器", "Xcode and simulators"),
        CleanupCategory::PackageCaches => ("包管理器缓存", "Package manager caches"),
        CleanupCategory::DevArtifacts => ("项目构建产物", "Project build artifacts"),
        CleanupCategory::NeedsReview => ("需要你判断的", "Needs your judgement"),
    };
    pick(lang, zh, en)
}

/// 一句话说明这一类是什么、删了会怎样。
pub fn explain_cleanup_category(lang: Lang, category: CleanupCategory) -> &'static str {
    let (zh, en) = match category {
        CleanupCategory::AppCaches => (
            "App 自己生成的缓存，删掉后会按需重建，第一次打开可能慢一点。",
            "Caches apps rebuild on demand; the next launch may be a little slower.",
        ),
        CleanupCategory::Logs => (
            "运行日志和崩溃报告，排查问题时才用得上。",
            "Run logs and crash reports, only useful when troubleshooting.",
        ),
        CleanupCategory::BrowserCaches => (
            "网页缓存，不含书签、密码、历史和登录状态。",
            "Web caches; bookmarks, passwords, history and logins are untouched.",
        ),
        CleanupCategory::Xcode => (
            "DerivedData、模拟器缓存等，下次构建时重新生成。",
            "DerivedData and simulator caches, regenerated on the next build.",
        ),
        CleanupCategory::PackageCaches => (
            "npm、pip、cargo、Homebrew 等下载过的包，需要时会重新下载。",
            "Packages npm, pip, cargo, Homebrew and others downloaded; fetched again when needed.",
        ),
        CleanupCategory::DevArtifacts => (
            "node_modules、target、.venv 等，在项目里重新安装或构建即可恢复。",
            "node_modules, target, .venv and the like; reinstall or rebuild to get them back.",
        ),
        CleanupCategory::NeedsReview => (
            "Xcode 归档、iOS 设备支持文件、旧模拟器等，可能还有用，Rind 不替你勾。",
            "Xcode archives, iOS device support, old simulators: may still matter, so Rind never picks them.",
        ),
    };
    pick(lang, zh, en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_category_in_both_languages() {
        for c in CleanupCategory::ALL {
            assert_ne!(
                describe_cleanup_category(Lang::ZhCn, c),
                describe_cleanup_category(Lang::En, c)
            );
            assert_ne!(
                explain_cleanup_category(Lang::ZhCn, c),
                explain_cleanup_category(Lang::En, c)
            );
        }
    }
}
