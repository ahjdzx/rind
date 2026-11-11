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
    AppTagline,
    DemoBadge,
    NavOverview,
    NavGroupClean,
    NavApps,
    NavOrphans,
    NavCleanup,
    NavSpace,
    NavLaunch,
    NavHistory,
    NavSettings,
    OverviewSubtitle,
    OverviewCards,
    DiskTitle,
    DiskUsed,
    DiskFree,
    DiskUnknown,
    SmartTitle,
    SmartBody,
    SmartStart,
    SmartAgain,
    SmartRunning,
    SmartRunningBody,
    SmartDoneBody,
    TaskDone,
    TaskWaiting,
    CardNotScanned,
    CardLargeFiles,
    CardSpaceHint,
    AppsSubtitle,
    SearchApps,
    AppsLoading,
    AppsEmpty,
    NoAppSelectedTitle,
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
    StartScan,
    Refresh,
    RefreshApps,
    ColPath,
    ColKind,
    ColSize,
    ColConfidence,
    ColReason,
    SelectSuggested,
    ClearSelection,
    IncludeApp,
    MoveToTrash,
    CleanSelected,
    Quit,
    ForceQuit,
    ConfirmTitle,
    ConfirmWithoutApp,
    ConfirmNote,
    EmptyTrashHint,
    Cancel,
    ResultTitle,
    PartialTitle,
    NothingMoved,
    NothingChanged,
    LeftoversEmpty,
    OrphansSubtitle,
    OrphansIdleTitle,
    OrphansIdleBody,
    OrphansNoneTitle,
    OrphansNoneBody,
    CleanupSubtitle,
    CleanupIdleTitle,
    CleanupIdleBody,
    CleanupNoneTitle,
    CleanupNoneBody,
    SpaceSubtitle,
    SpaceHome,
    SpaceDownloads,
    SpaceDocuments,
    SpaceLibrary,
    SpaceApplications,
    SpaceChoose,
    SpaceIdleTitle,
    SpaceIdleBody,
    SpaceAnalyzeHome,
    SpaceTabDirs,
    SpaceTabLarge,
    SpaceTotal,
    SpaceFiles,
    SpaceIncomplete,
    SpacePartialDir,
    SpaceEmptyDir,
    SpaceNoLargeFiles,
    SpaceNoLargeFilesBody,
    SpaceModified,
    SpaceAccessed,
    LaunchSubtitle,
    LaunchLoading,
    LaunchEmpty,
    LaunchEmptyBody,
    FilterAll,
    FilterOrphans,
    FilterUser,
    LaunchOrphan,
    LaunchNoOwner,
    LaunchNoProgram,
    LaunchNoLabel,
    LaunchEnable,
    LaunchDisable,
    LaunchRemove,
    HistorySubtitle,
    HistoryEmpty,
    HistoryEmptyBody,
    HistoryLoading,
    HistoryPending,
    Restored,
    Restore,
    SettingsSubtitle,
    SettingsGeneral,
    SettingsAppearance,
    AppearanceSystem,
    AppearanceLight,
    AppearanceDark,
    SettingsLanguage,
    LangZh,
    LangEn,
    SettingsCleanup,
    SettingsSessionOnly,
    SettingsRoots,
    SettingsAddRoot,
    SettingsRemoveRoot,
    SettingsResetRoots,
    SettingsDefault,
    SettingsNoRoots,
    SettingsAge,
    SettingsAgeHint,
    ProtectedTitle,
    ProtectedDescription,
    AboutTitle,
    AboutDescription,
    AboutVersion,
}

impl Key {
    /// 全部键，测试用。
    #[cfg(test)]
    pub const ALL: &'static [Key] = &[
        Key::AppTitle,
        Key::AppTagline,
        Key::DemoBadge,
        Key::NavOverview,
        Key::NavGroupClean,
        Key::NavApps,
        Key::NavOrphans,
        Key::NavCleanup,
        Key::NavSpace,
        Key::NavLaunch,
        Key::NavHistory,
        Key::NavSettings,
        Key::OverviewSubtitle,
        Key::OverviewCards,
        Key::DiskTitle,
        Key::DiskUsed,
        Key::DiskFree,
        Key::DiskUnknown,
        Key::SmartTitle,
        Key::SmartBody,
        Key::SmartStart,
        Key::SmartAgain,
        Key::SmartRunning,
        Key::SmartRunningBody,
        Key::SmartDoneBody,
        Key::TaskDone,
        Key::TaskWaiting,
        Key::CardNotScanned,
        Key::CardLargeFiles,
        Key::CardSpaceHint,
        Key::AppsSubtitle,
        Key::SearchApps,
        Key::AppsLoading,
        Key::AppsEmpty,
        Key::NoAppSelectedTitle,
        Key::NoAppSelected,
        Key::DetailBundleId,
        Key::DetailVersion,
        Key::DetailPath,
        Key::DetailSize,
        Key::SourceAppStore,
        Key::SourceHomebrew,
        Key::SourceOther,
        Key::Scanning,
        Key::Rescan,
        Key::StartScan,
        Key::Refresh,
        Key::RefreshApps,
        Key::ColPath,
        Key::ColKind,
        Key::ColSize,
        Key::ColConfidence,
        Key::ColReason,
        Key::SelectSuggested,
        Key::ClearSelection,
        Key::IncludeApp,
        Key::MoveToTrash,
        Key::CleanSelected,
        Key::Quit,
        Key::ForceQuit,
        Key::ConfirmTitle,
        Key::ConfirmWithoutApp,
        Key::ConfirmNote,
        Key::EmptyTrashHint,
        Key::Cancel,
        Key::ResultTitle,
        Key::PartialTitle,
        Key::NothingMoved,
        Key::NothingChanged,
        Key::LeftoversEmpty,
        Key::OrphansSubtitle,
        Key::OrphansIdleTitle,
        Key::OrphansIdleBody,
        Key::OrphansNoneTitle,
        Key::OrphansNoneBody,
        Key::CleanupSubtitle,
        Key::CleanupIdleTitle,
        Key::CleanupIdleBody,
        Key::CleanupNoneTitle,
        Key::CleanupNoneBody,
        Key::SpaceSubtitle,
        Key::SpaceHome,
        Key::SpaceDownloads,
        Key::SpaceDocuments,
        Key::SpaceLibrary,
        Key::SpaceApplications,
        Key::SpaceChoose,
        Key::SpaceIdleTitle,
        Key::SpaceIdleBody,
        Key::SpaceAnalyzeHome,
        Key::SpaceTabDirs,
        Key::SpaceTabLarge,
        Key::SpaceTotal,
        Key::SpaceFiles,
        Key::SpaceIncomplete,
        Key::SpacePartialDir,
        Key::SpaceEmptyDir,
        Key::SpaceNoLargeFiles,
        Key::SpaceNoLargeFilesBody,
        Key::SpaceModified,
        Key::SpaceAccessed,
        Key::LaunchSubtitle,
        Key::LaunchLoading,
        Key::LaunchEmpty,
        Key::LaunchEmptyBody,
        Key::FilterAll,
        Key::FilterOrphans,
        Key::FilterUser,
        Key::LaunchOrphan,
        Key::LaunchNoOwner,
        Key::LaunchNoProgram,
        Key::LaunchNoLabel,
        Key::LaunchEnable,
        Key::LaunchDisable,
        Key::LaunchRemove,
        Key::HistorySubtitle,
        Key::HistoryEmpty,
        Key::HistoryEmptyBody,
        Key::HistoryLoading,
        Key::HistoryPending,
        Key::Restored,
        Key::Restore,
        Key::SettingsSubtitle,
        Key::SettingsGeneral,
        Key::SettingsAppearance,
        Key::AppearanceSystem,
        Key::AppearanceLight,
        Key::AppearanceDark,
        Key::SettingsLanguage,
        Key::LangZh,
        Key::LangEn,
        Key::SettingsCleanup,
        Key::SettingsSessionOnly,
        Key::SettingsRoots,
        Key::SettingsAddRoot,
        Key::SettingsRemoveRoot,
        Key::SettingsResetRoots,
        Key::SettingsDefault,
        Key::SettingsNoRoots,
        Key::SettingsAge,
        Key::SettingsAgeHint,
        Key::ProtectedTitle,
        Key::ProtectedDescription,
        Key::AboutTitle,
        Key::AboutDescription,
        Key::AboutVersion,
    ];
}

/// 取固定文案。
pub fn ui(lang: Lang, key: Key) -> &'static str {
    use Key::*;
    match key {
        AppTitle => t(lang, "app.title"),
        AppTagline => pick(lang, "卸载与清理", "Uninstall & clean up"),
        AboutTitle => pick(lang, "关于", "About"),
        AboutDescription => pick(
            lang,
            "可解释、可撤销的 macOS 卸载与清理工具：每个文件都说明理由，只移到废纸篓，能从历史一键放回。",
            "An explainable, undoable macOS uninstaller and cleaner: a reason for every file, Trash only, and one-click put-back from History.",
        ),
        AboutVersion => pick(lang, "版本", "Version"),
        DemoBadge => pick(lang, "示例", "Demo"),
        NavOverview => pick(lang, "总览", "Overview"),
        NavGroupClean => pick(lang, "清理", "Clean up"),
        NavApps => pick(lang, "应用卸载", "Uninstall apps"),
        NavOrphans => pick(lang, "孤儿残留", "Orphaned files"),
        NavCleanup => pick(lang, "缓存清理", "Caches & junk"),
        NavSpace => pick(lang, "空间分析", "Space"),
        NavLaunch => pick(lang, "启动项", "Launch items"),
        NavHistory => t(lang, "nav.history"),
        NavSettings => t(lang, "nav.settings"),
        OverviewSubtitle => pick(lang, "看看空间都去哪儿了，一键找出能安全清理的东西。", "See where your space went and find what is safe to clean."),
        OverviewCards => pick(lang, "清理项目", "What to clean"),
        DiskTitle => pick(lang, "磁盘空间", "Disk space"),
        DiskUsed => pick(lang, "已用", "Used"),
        DiskFree => pick(lang, "可用", "Available"),
        DiskUnknown => pick(lang, "读不到磁盘容量。", "Disk capacity is not available."),
        SmartTitle => pick(lang, "智能扫描", "Smart scan"),
        SmartBody => pick(lang, "同时查找孤儿残留、可清理的缓存和启动项，结果会同步到各个页面，不用再扫一遍。", "Looks for orphaned files, caches and launch items at once; every page reuses the results."),
        SmartStart => pick(lang, "开始智能扫描", "Start smart scan"),
        SmartAgain => pick(lang, "重新扫描", "Scan again"),
        SmartRunning => pick(lang, "正在扫描…", "Scanning…"),
        SmartRunningBody => pick(lang, "可以随时取消，或先去别的页面看看，扫描会在后台继续。", "Cancel any time, or browse other pages while it runs in the background."),
        SmartDoneBody => pick(lang, "Rind 建议清理的孤儿残留和缓存合计。点下面的卡片查看并勾选。", "Orphaned files and caches Rind suggests cleaning. Open a card below to review."),
        TaskDone => pick(lang, "完成", "Done"),
        TaskWaiting => pick(lang, "等待中", "Waiting"),
        CardNotScanned => pick(lang, "还没扫描，点「智能扫描」", "Not scanned yet; run a smart scan"),
        CardLargeFiles => pick(lang, "大文件", "Large files"),
        CardSpaceHint => pick(lang, "选一个文件夹，看看什么最占地方", "Pick a folder to see what takes space"),
        AppsSubtitle => pick(lang, "选一个应用，看清它留下的每个文件和理由，再决定移走哪些。", "Pick an app to see every file it leaves behind, and why, before moving anything."),
        SearchApps => pick(lang, "搜索应用…", "Search apps…"),
        AppsLoading => t(lang, "state.loading_apps"),
        AppsEmpty => t(lang, "state.no_apps"),
        NoAppSelectedTitle => pick(lang, "选一个应用", "Pick an app"),
        NoAppSelected => pick(lang, "在左侧选一个应用，看看它在你的 Mac 上留下了什么。", "Pick an app on the left to see what it leaves on your Mac."),
        DetailBundleId => "Bundle ID",
        DetailVersion => t(lang, "col.version"),
        DetailPath => t(lang, "col.path"),
        DetailSize => t(lang, "col.size"),
        SourceAppStore => "App Store",
        SourceHomebrew => "Homebrew",
        SourceOther => pick(lang, "直接安装", "Downloaded"),
        Scanning => pick(lang, "正在扫描…", "Scanning…"),
        Rescan => pick(lang, "重新扫描", "Rescan"),
        StartScan => pick(lang, "开始扫描", "Scan"),
        Refresh => t(lang, "action.refresh"),
        RefreshApps => pick(lang, "重新读取应用列表", "Reload the app list"),
        ColPath => t(lang, "col.path"),
        ColKind => t(lang, "col.kind"),
        ColSize => t(lang, "col.size"),
        ColConfidence => t(lang, "col.confidence"),
        ColReason => t(lang, "col.reason"),
        SelectSuggested => t(lang, "action.select_suggested"),
        ClearSelection => t(lang, "action.clear"),
        IncludeApp => pick(lang, "包含应用本体", "Include the app"),
        MoveToTrash => t(lang, "action.trash"),
        CleanSelected => pick(lang, "清理所选", "Clean selected"),
        Quit => t(lang, "action.quit_app"),
        ForceQuit => t(lang, "action.force_quit"),
        ConfirmTitle => t(lang, "confirm.title"),
        ConfirmWithoutApp => pick(lang, "不含应用本体。", "The app itself stays."),
        ConfirmNote => pick(lang, "一切都只是移入废纸篓，历史页可以放回。", "Everything goes to the Trash; History can put it back."),
        EmptyTrashHint => pick(lang, "移到废纸篓后，要清空废纸篓才会真正腾出空间。", "Space is only freed once you empty the Trash."),
        Cancel => t(lang, "action.cancel"),
        ResultTitle => pick(lang, "完成", "Done"),
        PartialTitle => pick(lang, "History 没有记全", "History is incomplete"),
        NothingMoved => pick(lang, "什么都没有移动", "Nothing moved"),
        NothingChanged => pick(lang, "没有改动", "Nothing changed"),
        LeftoversEmpty => t(lang, "state.no_leftovers"),
        OrphansSubtitle => pick(lang, "应用已经删了，数据还留着。按推测的主人分组，每一项都说明理由。", "Apps that are gone, data that stayed. Grouped by likely owner, each with a reason."),
        OrphansIdleTitle => pick(lang, "找找已删应用的残留", "Find what deleted apps left behind"),
        OrphansIdleBody => pick(lang, "Rind 会在 ~/Library 等位置找没有任何已安装应用认领的文件，只报有依据的。", "Rind looks in ~/Library and friends for files no installed app claims, and only reports what it can explain."),
        OrphansNoneTitle => pick(lang, "没有发现孤儿残留", "No orphaned files"),
        OrphansNoneBody => pick(lang, "已删除应用没有留下 Rind 能认出来的东西。", "Deleted apps left nothing Rind can recognise."),
        CleanupSubtitle => pick(lang, "缓存、日志和开发产物，删掉后会按需重建。", "Caches, logs and build artifacts that are rebuilt on demand."),
        CleanupIdleTitle => pick(lang, "看看有多少缓存可以清", "See how much cache can go"),
        CleanupIdleBody => pick(lang, "包括应用缓存、日志、浏览器缓存、Xcode、包管理器缓存和项目里的构建产物。", "App caches, logs, browser caches, Xcode, package caches and project build artifacts."),
        CleanupNoneTitle => pick(lang, "没什么可清理的", "Nothing to clean"),
        CleanupNoneBody => pick(lang, "没有找到可以安全清理的缓存。", "No caches that are safe to clean were found."),
        SpaceSubtitle => pick(lang, "选一个目录，看什么最占地方；点目录往下看。", "Pick a folder to see what takes space; click a folder to go deeper."),
        SpaceHome => pick(lang, "家目录", "Home"),
        SpaceDownloads => pick(lang, "下载", "Downloads"),
        SpaceDocuments => pick(lang, "文稿", "Documents"),
        SpaceLibrary => "Library",
        SpaceApplications => pick(lang, "应用程序", "Applications"),
        SpaceChoose => pick(lang, "选择文件夹…", "Choose folder…"),
        SpaceIdleTitle => pick(lang, "空间都去哪儿了？", "Where did the space go?"),
        SpaceIdleBody => pick(lang, "选上面的一个位置，或者自己选一个文件夹开始分析。", "Pick a place above, or choose any folder to analyze."),
        SpaceAnalyzeHome => pick(lang, "分析家目录", "Analyze home folder"),
        SpaceTabDirs => pick(lang, "目录", "Folders"),
        SpaceTabLarge => pick(lang, "大文件", "Large files"),
        SpaceTotal => pick(lang, "总大小", "Total"),
        SpaceFiles => pick(lang, "文件数", "Files"),
        SpaceIncomplete => pick(lang, "分析没有完成（超时或被取消），大小只是下限。", "The analysis did not finish (timed out or cancelled); sizes are lower bounds."),
        SpacePartialDir => pick(lang, "这个目录里有读不了的部分，大小只是下限。", "Part of this folder could not be read; its size is a lower bound."),
        SpaceEmptyDir => pick(lang, "这里没有更多可显示的内容", "Nothing more to show here"),
        SpaceNoLargeFiles => pick(lang, "没有大文件", "No large files"),
        SpaceNoLargeFilesBody => pick(lang, "这个目录下没有超过 100 MB 的文件。", "No file in this folder is over 100 MB."),
        SpaceModified => pick(lang, "修改", "Modified"),
        SpaceAccessed => pick(lang, "访问", "Opened"),
        LaunchSubtitle => pick(lang, "开机或登录时自动运行的后台程序。主人已经不在的会标出来。", "Background programs that start at boot or login. Ones whose app is gone are flagged."),
        LaunchLoading => pick(lang, "正在读取启动项…", "Reading launch items…"),
        LaunchEmpty => pick(lang, "这里没有启动项", "No launch items here"),
        LaunchEmptyBody => pick(lang, "换一个筛选看看。", "Try another filter."),
        FilterAll => pick(lang, "全部", "All"),
        FilterOrphans => pick(lang, "孤儿", "Orphans"),
        FilterUser => pick(lang, "当前用户", "This user"),
        LaunchOrphan => pick(lang, "孤儿", "Orphan"),
        LaunchNoOwner => pick(lang, "没有找到所属应用", "No owning app found"),
        LaunchNoProgram => pick(lang, "没有写明要运行的程序", "No program specified"),
        LaunchNoLabel => pick(lang, "plist 里没有 Label，无法交给 launchctl 管理", "The plist has no Label, so launchctl cannot manage it"),
        LaunchEnable => pick(lang, "启用", "Enable"),
        LaunchDisable => pick(lang, "停用", "Disable"),
        LaunchRemove => pick(lang, "移除", "Remove"),
        HistorySubtitle => pick(lang, "Rind 移到废纸篓的每一批东西，都可以放回原处。", "Every batch Rind moved to the Trash, ready to put back."),
        HistoryEmpty => t(lang, "history.empty"),
        HistoryEmptyBody => pick(lang, "卸载或清理之后，记录会出现在这里。", "Records show up here after you uninstall or clean something."),
        HistoryLoading => pick(lang, "正在读取历史…", "Reading history…"),
        HistoryPending => pick(lang, "未完成", "Pending"),
        Restored => t(lang, "history.restored"),
        Restore => t(lang, "action.restore"),
        SettingsSubtitle => pick(lang, "外观、语言、缓存清理的偏好，以及 Rind 永远不动的地方。", "Appearance, language, cleanup preferences, and what Rind never touches."),
        SettingsGeneral => pick(lang, "通用", "General"),
        SettingsAppearance => pick(lang, "外观", "Appearance"),
        AppearanceSystem => pick(lang, "跟随系统", "System"),
        AppearanceLight => pick(lang, "浅色", "Light"),
        AppearanceDark => pick(lang, "深色", "Dark"),
        SettingsLanguage => pick(lang, "语言", "Language"),
        LangZh => t(lang, "lang.zh"),
        LangEn => t(lang, "lang.en"),
        SettingsCleanup => pick(lang, "缓存清理", "Cleanup"),
        SettingsSessionOnly => pick(lang, "这些设置只在本次运行内有效，改完后重新扫描缓存才会用上。", "These settings last for this session; rescan caches to apply them."),
        SettingsRoots => pick(lang, "到哪些目录里找开发项目", "Where to look for projects"),
        SettingsAddRoot => pick(lang, "添加目录…", "Add folder…"),
        SettingsRemoveRoot => pick(lang, "不再在这里找", "Stop looking here"),
        SettingsResetRoots => pick(lang, "恢复默认", "Reset"),
        SettingsDefault => pick(lang, "默认", "Default"),
        SettingsNoRoots => pick(lang, "没有找到默认的项目目录，可以手动添加。", "No default project folders found; add one."),
        SettingsAge => pick(lang, "多久没动过", "Untouched for"),
        SettingsAgeHint => pick(lang, "构建产物超过这么久没修改，Rind 才会建议清理。", "Rind only suggests build artifacts left untouched this long."),
        ProtectedTitle => t(lang, "protected.title"),
        ProtectedDescription => pick(lang, "无论怎么选，下面的位置和它们里面的东西都不会被移动。", "Whatever is selected, these places and everything inside them stay put."),
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

/// 中英各一句，带参数的。
fn say(lang: Lang, zh: String, en: String) -> String {
    match lang {
        Lang::ZhCn => zh,
        Lang::En => en,
    }
}

/// "已选 5 项 · 可释放 1.2 GB"
pub fn picked_line(lang: Lang, count: usize, size: &str) -> String {
    say(
        lang,
        format!("已选 {count} 项 · 可释放 {size}"),
        format!("{count} selected · {size} to free"),
    )
}

/// "已选 2 / 5"
pub fn picked_of(lang: Lang, picked: usize, total: usize) -> String {
    say(
        lang,
        format!("已选 {picked} / {total}"),
        format!("{picked} of {total} selected"),
    )
}

/// "这 3 项会移到废纸篓。"（大小无从说起时）
pub fn confirm_count(lang: Lang, count: usize) -> String {
    say(
        lang,
        format!("这 {count} 项会移到废纸篓，历史里可以随时放回来。"),
        format!("These {count} items will move to the Trash; History can put them back."),
    )
}

/// "正在看 1,234 个文件…"
pub fn files_seen(lang: Lang, done: usize) -> String {
    say(
        lang,
        format!("已经看过 {done} 个文件…"),
        format!("{done} files looked at…"),
    )
}

/// "12 个"
pub fn count(lang: Lang, n: usize) -> String {
    say(lang, format!("{n} 个"), n.to_string())
}

/// "共 12,345 个文件"
pub fn file_count(lang: Lang, n: u64) -> String {
    say(lang, format!("{n} 个文件"), format!("{n} files"))
}

/// "30 天"
pub fn days(lang: Lang, n: u32) -> String {
    say(lang, format!("{n} 天"), format!("{n} days"))
}

/// "已用 380 GB，可用 71 GB，共 494 GB"
pub fn disk_line(lang: Lang, used: &str, free: &str, total: &str) -> String {
    say(
        lang,
        format!("已用 {used}，可用 {free}，共 {total}"),
        format!("{used} used, {free} available of {total}"),
    )
}

/// "可释放约 12 GB"
pub fn reclaimable(lang: Lang, size: &str) -> String {
    say(
        lang,
        format!("可释放约 {size}"),
        format!("About {size} to free"),
    )
}

/// "共 4.1 GB"
pub fn apps_total(lang: Lang, size: &str) -> String {
    say(lang, format!("合计 {size}"), format!("{size} in total"))
}

/// "12 个应用"
pub fn app_count_line(lang: Lang, n: usize) -> String {
    say(lang, format!("{n} 个应用"), format!("{n} apps"))
}

/// "6 组 · 建议清理 3.2 GB"
pub fn orphan_card(lang: Lang, groups: usize, suggested: &str) -> String {
    say(
        lang,
        format!("{groups} 组 · 建议清理 {suggested}"),
        format!("{groups} groups · {suggested} suggested"),
    )
}

/// "6 组，18 项，共 6.5 GB"
pub fn orphan_totals(lang: Lang, groups: usize, items: usize, size: &str) -> String {
    say(
        lang,
        format!("{groups} 组，{items} 项，共 {size}"),
        format!("{groups} groups, {items} items, {size}"),
    )
}

/// 确认框里："涉及 2 个已删除的应用。"
pub fn groups_involved(lang: Lang, groups: usize) -> String {
    say(
        lang,
        format!("涉及 {groups} 个已删除的应用。"),
        format!("From {groups} deleted apps."),
    )
}

/// "共 31 项，合计 40 GB"
pub fn cleanup_card(lang: Lang, items: usize, total: &str) -> String {
    say(
        lang,
        format!("建议清理 · 共 {items} 项，合计 {total}"),
        format!("suggested · {items} items, {total} in total"),
    )
}

/// "合计 30 GB · ~"
pub fn large_files_card(lang: Lang, total: &str, root: &str) -> String {
    say(
        lang,
        format!("合计 {total} · {root}"),
        format!("{total} in total · {root}"),
    )
}

/// "孤儿 3 · 运行中 4"
pub fn launch_card(lang: Lang, orphans: usize, running: usize) -> String {
    say(
        lang,
        format!("孤儿 {orphans} · 运行中 {running}"),
        format!("{orphans} orphaned · {running} running"),
    )
}

/// "开发项目：~/work、~/Projects · 超过 30 天没动过的构建产物才建议清理"
pub fn cleanup_prefs_line(lang: Lang, roots: &[String], days: u32) -> String {
    match lang {
        Lang::ZhCn => format!(
            "开发项目目录：{} · 超过 {days} 天没动过的构建产物才建议清理（可在设置里改）",
            if roots.is_empty() { "无".to_string() } else { roots.join("、") }
        ),
        Lang::En => format!(
            "Project folders: {} · build artifacts untouched for {days}+ days are suggested (change in Settings)",
            if roots.is_empty() { "none".to_string() } else { roots.join(", ") }
        ),
    }
}

/// "微信 正在运行，退出后重新扫描才能清理"
pub fn in_use_rescan(lang: Lang, app: &str) -> String {
    say(
        lang,
        format!("{app} 正在运行，退出它之后重新扫描才能清理"),
        format!("{app} is running; quit it and rescan to clean this"),
    )
}

/// "2 项正被运行中的应用使用"
pub fn in_use_count(lang: Lang, n: usize) -> String {
    say(
        lang,
        format!("{n} 项正被运行中的应用使用，退出后重新扫描"),
        format!("{n} in use by running apps; quit them and rescan"),
    )
}

/// "空间分析 · ~/Downloads"
pub fn space_batch_title(lang: Lang, root: &str) -> String {
    say(
        lang,
        format!("空间分析 · {root}"),
        format!("Space · {root}"),
    )
}

/// "正在分析 ~/Downloads"
pub fn analyzing(lang: Lang, root: &str) -> String {
    say(
        lang,
        format!("正在分析 {root}"),
        format!("Analyzing {root}"),
    )
}

/// "其余 37 项"
pub fn other_items(lang: Lang, n: u64) -> String {
    say(lang, format!("其余 {n} 项"), format!("{n} more items"))
}

/// "有 2 个目录读不了，没有算进去。给终端或 Rind 授予「完全磁盘访问」后重新扫描。"
pub fn unreadable_dirs(lang: Lang, n: usize) -> String {
    say(
        lang,
        format!(
            "有 {n} 个目录读不了（权限不够），没有算进大小里。给终端或 Rind 授予「完全磁盘访问」后重新扫描。"
        ),
        format!(
            "{n} folders could not be read (no permission) and are not counted. Grant Full Disk Access to your terminal or to Rind, then scan again."
        ),
    )
}

/// "属于 Docker"
pub fn launch_owner(lang: Lang, app: &str) -> String {
    say(lang, format!("属于 {app}"), format!("Belongs to {app}"))
}

/// "…（已不存在）"
pub fn program_missing(lang: Lang, program: &str) -> String {
    say(
        lang,
        format!("{program}（已不存在）"),
        format!("{program} (missing)"),
    )
}

/// "发现 3 个孤儿启动项"
pub fn launch_orphans_hint(lang: Lang, n: usize) -> String {
    say(
        lang,
        format!("{n} 个启动项的程序已经不在了"),
        format!("{n} launch items point at missing programs"),
    )
}

/// "已停用 com.foo"
pub fn launch_toggled(lang: Lang, name: &str, enabled: bool) -> String {
    match (lang, enabled) {
        (Lang::ZhCn, true) => format!("已启用 {name}"),
        (Lang::ZhCn, false) => format!("已停用 {name}，随时可以再启用"),
        (Lang::En, true) => format!("Enabled {name}"),
        (Lang::En, false) => format!("Disabled {name}; you can enable it again any time"),
    }
}

/// "没能停用 com.foo：…"
pub fn launch_toggle_failed(lang: Lang, name: &str, enabled: bool, error: &str) -> String {
    match (lang, enabled) {
        (Lang::ZhCn, true) => format!("没能启用 {name}：{error}"),
        (Lang::ZhCn, false) => format!("没能停用 {name}：{error}"),
        (Lang::En, true) => format!("Could not enable {name}: {error}"),
        (Lang::En, false) => format!("Could not disable {name}: {error}"),
    }
}

/// 移除启动项前的说明。
pub fn launch_remove_note(lang: Lang, name: &str) -> String {
    say(
        lang,
        format!("先让 {name} 停下来，再把它的 plist 移到废纸篓。"),
        format!("{name} is stopped first, then its plist goes to the Trash."),
    )
}

/// "刚刚" / "5 分钟前" / "3 天前" / "2024-01-02"
pub fn relative_time(
    lang: Lang,
    now: chrono::DateTime<chrono::Utc>,
    then: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = (now - then).num_seconds().max(0);
    let (minutes, hours, days) = (secs / 60, secs / 3600, secs / 86_400);
    if minutes < 1 {
        return say(lang, "刚刚".into(), "just now".into());
    }
    if hours < 1 {
        return say(
            lang,
            format!("{minutes} 分钟前"),
            format!("{minutes} min ago"),
        );
    }
    if days < 1 {
        return say(lang, format!("{hours} 小时前"), format!("{hours} h ago"));
    }
    if days < 60 {
        return say(lang, format!("{days} 天前"), format!("{days} days ago"));
    }
    then.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_text_in_both_languages() {
        for &key in Key::ALL {
            assert!(!ui(Lang::ZhCn, key).is_empty(), "{key:?} zh");
            assert!(!ui(Lang::En, key).is_empty(), "{key:?} en");
        }
    }

    #[test]
    fn chinese_comes_first() {
        assert_eq!(ui(Lang::ZhCn, Key::NavOrphans), "孤儿残留");
        assert_eq!(ui(Lang::En, Key::NavOrphans), "Orphaned files");
    }

    #[test]
    fn relative_time_steps() {
        let now = chrono::Utc::now();
        let ago = |s: i64| now - chrono::Duration::seconds(s);
        assert_eq!(relative_time(Lang::ZhCn, now, ago(10)), "刚刚");
        assert_eq!(relative_time(Lang::ZhCn, now, ago(5 * 60)), "5 分钟前");
        assert_eq!(relative_time(Lang::En, now, ago(3 * 3600)), "3 h ago");
        assert_eq!(relative_time(Lang::ZhCn, now, ago(3 * 86_400)), "3 天前");
        assert!(relative_time(Lang::ZhCn, now, ago(400 * 86_400)).contains('-'));
        // 时钟往回拨也不出负数。
        assert_eq!(
            relative_time(Lang::En, now, now + chrono::Duration::hours(1)),
            "just now"
        );
    }

    #[test]
    fn parameterised_lines() {
        assert_eq!(
            picked_line(Lang::ZhCn, 3, "1 GB"),
            "已选 3 项 · 可释放 1 GB"
        );
        assert_eq!(launch_toggled(Lang::En, "x", true), "Enabled x");
        assert!(cleanup_prefs_line(Lang::ZhCn, &[], 30).contains("无"));
        assert!(cleanup_prefs_line(Lang::En, &["~/work".into()], 7).contains("~/work"));
    }
}
