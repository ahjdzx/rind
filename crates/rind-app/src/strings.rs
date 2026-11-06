//! 界面自己的文案，按稳定的意图键取。凡是 `rind-i18n` 已经有的固定文案（导航、列名、按钮、
//! 摘要）都走 `rind_i18n::t`，这里只放界面独有的句子；core 结果的描述（理由、类型、保护原因）
//! 一律来自 `rind-i18n`，不在这里重复。

use gpui_kit::{App, Global};
pub use rind_i18n::Lang;
use rind_i18n::{fill, t};

use crate::services::ExecuteFailure;

/// 当前界面语言。整个应用只有一份，改了之后每个页面都重画。
pub struct LangSetting {
    lang: Lang,
}

impl Global for LangSetting {}

impl LangSetting {
    pub fn new(lang: Lang) -> Self {
        Self { lang }
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }
}

/// 读当前语言；全局没设时按中文。
pub fn lang(cx: &App) -> Lang {
    if cx.has_global::<LangSetting>() {
        cx.global::<LangSetting>().lang()
    } else {
        Lang::ZhCn
    }
}

fn pick(lang: Lang, zh: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::ZhCn => zh,
        Lang::En => en,
    }
}

/// 固定文案的意图键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    AppTitle,
    NavApps,
    NavHistory,
    NavSettings,
    SearchApps,
    AppsLoading,
    AppsEmpty,
    NoAppSelected,
    DetailBundleId,
    DetailVersion,
    DetailPath,
    DetailSize,
    SourceAppStore,
    SourceHomebrew,
    SourceOther,
    Scanning,
    Rescan,
    ColPath,
    ColKind,
    ColSize,
    ColConfidence,
    ColReason,
    SelectSuggested,
    ClearSelection,
    IncludeApp,
    MoveToTrash,
    Quit,
    ForceQuit,
    ConfirmTitle,
    ConfirmWithoutApp,
    ConfirmNote,
    Cancel,
    ResultTitle,
    PartialTitle,
    NothingMoved,
    HistoryTitle,
    HistoryEmpty,
    HistoryLoading,
    Restored,
    Restore,
    SettingsTitle,
    SettingsLanguage,
    LangZh,
    LangEn,
    ProtectedTitle,
    ProtectedDescription,
    LeftoversEmpty,
    DemoBadge,
    RefreshApps,
}

/// 取固定文案。
pub fn ui(lang: Lang, key: Key) -> &'static str {
    use Key::*;
    match key {
        AppTitle => t(lang, "app.title"),
        NavApps => t(lang, "nav.apps"),
        NavHistory => t(lang, "nav.history"),
        NavSettings => t(lang, "nav.settings"),
        SearchApps => pick(lang, "搜索应用…", "Search apps…"),
        AppsLoading => t(lang, "state.loading_apps"),
        AppsEmpty => t(lang, "state.no_apps"),
        NoAppSelected => pick(
            lang,
            "在左侧选一个应用，看看它在你的 Mac 上留下了什么。",
            "Pick an app on the left to see what it leaves on your Mac.",
        ),
        DetailBundleId => "Bundle ID",
        DetailVersion => t(lang, "col.version"),
        DetailPath => t(lang, "col.path"),
        DetailSize => t(lang, "col.size"),
        SourceAppStore => "App Store",
        SourceHomebrew => "Homebrew",
        SourceOther => pick(lang, "直接安装", "Downloaded"),
        Scanning => pick(lang, "正在扫描…", "Scanning…"),
        Rescan => pick(lang, "重新扫描", "Rescan"),
        ColPath => t(lang, "col.path"),
        ColKind => t(lang, "col.kind"),
        ColSize => t(lang, "col.size"),
        ColConfidence => t(lang, "col.confidence"),
        ColReason => t(lang, "col.reason"),
        SelectSuggested => t(lang, "action.select_suggested"),
        ClearSelection => t(lang, "action.clear"),
        IncludeApp => pick(lang, "包含应用本体", "Include the app"),
        MoveToTrash => t(lang, "action.trash"),
        Quit => t(lang, "action.quit_app"),
        ForceQuit => t(lang, "action.force_quit"),
        ConfirmTitle => t(lang, "confirm.title"),
        ConfirmWithoutApp => pick(lang, "不含应用本体。", "The app itself stays."),
        ConfirmNote => pick(
            lang,
            "一切都只是移入废纸篓，历史页可以放回。",
            "Everything goes to the Trash; History can put it back.",
        ),
        Cancel => t(lang, "action.cancel"),
        ResultTitle => pick(lang, "完成", "Done"),
        PartialTitle => pick(lang, "History 没有记全", "History is incomplete"),
        NothingMoved => pick(lang, "什么都没有移动", "Nothing moved"),
        HistoryTitle => t(lang, "nav.history"),
        HistoryEmpty => t(lang, "history.empty"),
        HistoryLoading => pick(lang, "正在读取历史…", "Reading history…"),
        Restored => t(lang, "history.restored"),
        Restore => t(lang, "action.restore"),
        SettingsTitle => t(lang, "nav.settings"),
        SettingsLanguage => pick(lang, "语言", "Language"),
        LangZh => t(lang, "lang.zh"),
        LangEn => t(lang, "lang.en"),
        ProtectedTitle => t(lang, "protected.title"),
        ProtectedDescription => pick(
            lang,
            "无论怎么选，下面的位置和它们里面的东西都不会被移动。",
            "Whatever is selected, these places and everything inside them stay put.",
        ),
        LeftoversEmpty => t(lang, "state.no_leftovers"),
        DemoBadge => pick(lang, "示例数据", "Demo data"),
        RefreshApps => t(lang, "action.refresh"),
    }
}

/// "正在扫描 ~/Library/Caches…"
pub fn scanning_location(lang: Lang, location: &str) -> String {
    fill(t(lang, "state.scanning"), &[("location", location)])
}

/// "正在计算大小：3 / 12"
pub fn measuring(lang: Lang, done: usize, total: usize) -> String {
    fill(
        t(lang, "state.measuring"),
        &[("done", &done.to_string()), ("total", &total.to_string())],
    )
}

/// "已选 5 项，共 1.2 GB"
pub fn selected_summary(lang: Lang, count: usize, freed: &str) -> String {
    fill(
        t(lang, "summary.selected"),
        &[("n", &count.to_string()), ("size", freed)],
    )
}

/// "Obsidian 正在运行，先退出它才能移动。"
pub fn app_running(lang: Lang, name: &str) -> String {
    match lang {
        Lang::ZhCn => format!("{name} 正在运行，先退出它才能移动。"),
        Lang::En => format!("{name} is running; quit it before moving anything."),
    }
}

/// "应用本体不能移动：…"
pub fn app_held(lang: Lang, reason: &str) -> String {
    match lang {
        Lang::ZhCn => format!("应用本体不能移动：{reason}"),
        Lang::En => format!("The app itself cannot be moved: {reason}"),
    }
}

/// "打不开 History，Rind 不会移动任何东西：…"
pub fn ledger_unavailable(lang: Lang, error: &str) -> String {
    match lang {
        Lang::ZhCn => format!("打不开 History，所以 Rind 不会移动任何东西（没有记录的东西不进废纸篓）：{error}"),
        Lang::En => format!("History cannot be opened, so Rind will not move anything (nothing goes to the Trash unrecorded): {error}"),
    }
}

/// "12 个项目，共 1.2 GB"
pub fn confirm_items(lang: Lang, count: usize, size: &str) -> String {
    fill(
        t(lang, "confirm.body"),
        &[("n", &count.to_string()), ("size", size)],
    )
}

/// "包含应用本体 Obsidian。"
pub fn confirm_with_app(lang: Lang, name: &str) -> String {
    match lang {
        Lang::ZhCn => format!("包含应用本体 {name}。"),
        Lang::En => format!("Including the app {name}."),
    }
}

/// "3 项大小未知，实际释放可能更多。"
pub fn confirm_unknown(lang: Lang, count: usize) -> String {
    match lang {
        Lang::ZhCn => format!("{count} 项大小未知，实际释放可能更多。"),
        Lang::En => format!("{count} items have an unknown size; more may be freed."),
    }
}

/// "已移到废纸篓 8 项，已释放 1.2 GB；1 项 Rind 拒绝移动，0 项移动失败"
pub fn result_summary(
    lang: Lang,
    moved: usize,
    freed: &str,
    refused: usize,
    failed: usize,
) -> String {
    let parts = [
        fill(t(lang, "summary.moved"), &[("n", &moved.to_string())]),
        fill(t(lang, "summary.freed"), &[("size", freed)]),
        fill(t(lang, "summary.refused"), &[("n", &refused.to_string())]),
        fill(t(lang, "summary.failed"), &[("n", &failed.to_string())]),
    ];
    match lang {
        Lang::ZhCn => format!("{}，{}；{}，{}。", parts[0], parts[1], parts[2], parts[3]),
        Lang::En => format!("{}, {}; {}, {}.", parts[0], parts[1], parts[2], parts[3]),
    }
}

/// History 没记全时的说明：这些项已经在废纸篓里了。
pub fn partial_note(lang: Lang, error: &str) -> String {
    match lang {
        Lang::ZhCn => format!("上面这些项已经在废纸篓里，但 History 没能记下来，放回要手动从废纸篓拖回：{error}"),
        Lang::En => format!("These items are already in the Trash, but History could not record them; put them back by hand from the Trash: {error}"),
    }
}

/// "放回了 3 项，1 项没能放回"
pub fn restore_summary(lang: Lang, restored: usize, skipped: usize) -> String {
    let done = fill(
        t(lang, "history.restore_done"),
        &[("n", &restored.to_string())],
    );
    if skipped == 0 {
        return done;
    }
    let skipped = fill(
        t(lang, "history.restore_skipped"),
        &[("n", &skipped.to_string())],
    );
    match lang {
        Lang::ZhCn => format!("{done}，{skipped}"),
        Lang::En => format!("{done}, {skipped}"),
    }
}

/// "12 项"
pub fn item_count(lang: Lang, count: usize) -> String {
    match lang {
        Lang::ZhCn => format!("{count} 项"),
        Lang::En => format!("{count} items"),
    }
}

/// "也被 Foo、Bar 使用"
pub fn shared_with(lang: Lang, names: &[String]) -> String {
    match lang {
        Lang::ZhCn => format!("也被 {} 使用", names.join("、")),
        Lang::En => format!("also used by {}", names.join(", ")),
    }
}

/// "出错了：…"
pub fn error(lang: Lang, message: &str) -> String {
    match lang {
        Lang::ZhCn => format!("出错了：{message}"),
        Lang::En => format!("Something went wrong: {message}"),
    }
}

/// core 执行失败时给人看的话。`Partial` 另有 `partial_note`。
pub fn describe_failure(lang: Lang, failure: &ExecuteFailure) -> String {
    match failure {
        ExecuteFailure::AppRunning => t(lang, "state.running").to_string(),
        ExecuteFailure::NothingSelected => t(lang, "state.nothing_selected").to_string(),
        ExecuteFailure::LedgerUnavailable(err) => ledger_unavailable(lang, err),
        ExecuteFailure::Partial { error, .. } => partial_note(lang, error),
        ExecuteFailure::AppMoveFailed(err) => match lang {
            Lang::ZhCn => format!("应用本体没能移动，残留一个都没动：{err}"),
            Lang::En => {
                format!("The app could not be moved, so none of its leftovers were touched: {err}")
            }
        },
        ExecuteFailure::Other(err) => error(lang, err),
    }
}
