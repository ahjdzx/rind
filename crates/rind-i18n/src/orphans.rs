//! 孤儿残留相关的文案。

use rind_core::OrphanSignal;

use crate::{pick, Lang};

/// 凭什么认定主人已经不在了。
pub fn describe_orphan_signal(lang: Lang, signal: OrphanSignal) -> &'static str {
    let (zh, en) = match signal {
        OrphanSignal::BundleIdGone => (
            "名字是一个 bundle id，盘上没有任何 App 用它",
            "Named after a bundle id that no app on this Mac uses",
        ),
        OrphanSignal::ProgramMissing => (
            "它要启动的程序已经不在了",
            "The program it launches is gone",
        ),
        OrphanSignal::KnownAppGone => (
            "规则库或 Homebrew 点名属于某个 App，但那个 App 没装",
            "Named as a known app's data (Rind's rules or Homebrew), but that app is not installed",
        ),
        OrphanSignal::NameGone => (
            "名字像某个 App 的数据，找不到那个 App",
            "Looks like an app's data, but no such app is installed",
        ),
    };
    pick(lang, zh, en)
}

/// `OrphanReport::degraded` 时的提示：没问成 Spotlight，所以一项都不预先勾选。
pub fn describe_orphans_degraded(lang: Lang) -> &'static str {
    pick(
        lang,
        "没能向 Spotlight 查询别处装着的 App（可能已关闭或超时），这些结果都只是\"可能\"，没有预先勾选。",
        "Couldn't ask Spotlight for apps installed elsewhere (it may be off or timed out), so every result is only \"possible\" and nothing is preselected.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_signal_in_both_languages() {
        for s in [
            OrphanSignal::BundleIdGone,
            OrphanSignal::ProgramMissing,
            OrphanSignal::KnownAppGone,
            OrphanSignal::NameGone,
        ] {
            assert_ne!(
                describe_orphan_signal(Lang::ZhCn, s),
                describe_orphan_signal(Lang::En, s)
            );
        }
        assert_ne!(
            describe_orphans_degraded(Lang::ZhCn),
            describe_orphans_degraded(Lang::En)
        );
    }
}
