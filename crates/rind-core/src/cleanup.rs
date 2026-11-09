//! 缓存、日志、开发产物清理。思路见 docs/ARCHITECTURE.md「清理」一节。
//!
//! 契约（GUI / CLI 依赖它，改动要同步）：
//! - 每一项都属于一个 `CleanupCategory`；同一个路径只出现一次。
//! - `suggested` 由 core 决定：安全的缓存类、大小已知、无 `hold_back` 才建议；开发产物还要求
//!   超过 `artifact_min_age_days` 天没动过；归档、设备支持文件等需要人判断的从不建议。
//! - 所属程序在运行 → `HoldBack::InUse`；保护区规则与卸载一致。进程列表取不到（空）时，
//!   应用缓存、浏览器缓存、Xcode 一律不建议。移动前调用方要再跑一次 `recheck_in_use`。
//! - 扫描根和它到家目录的每一级都必须是真实目录（不是符号链接），否则整个根跳过；符号链接子项不列、
//!   不进入。输出的 `path` 是盘上的真实路径（真实大小写）。
//! - 只进废纸篓：移动走 `remove::trash_items`，记录类型是 `RecordKind::Cleanup`。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};
use walkdir::WalkDir;

use crate::remove::Removable;
use crate::{protect, Env, HoldBack, InstalledApp, ScanProgress, Size};

/// 清理类别。界面按它分组，文案在 `rind_i18n::describe_cleanup_category`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupCategory {
    /// `~/Library/Caches` 下各 App 的缓存。
    AppCaches,
    /// `~/Library/Logs`、诊断报告。
    Logs,
    /// 浏览器的缓存目录（不是 profile 本身）。
    BrowserCaches,
    /// Xcode DerivedData、模拟器缓存等可重建的东西。
    Xcode,
    /// npm / yarn / pnpm / pip / cargo / gradle / go / Homebrew 等包管理器的下载缓存。
    PackageCaches,
    /// 项目里的 `node_modules`、`target`、`.venv`、`build` 等可重建的产物。
    DevArtifacts,
    /// Xcode Archives、iOS DeviceSupport、旧模拟器：能删但要人判断，从不建议。
    NeedsReview,
}

impl CleanupCategory {
    pub const ALL: [CleanupCategory; 7] = [
        CleanupCategory::AppCaches,
        CleanupCategory::Logs,
        CleanupCategory::BrowserCaches,
        CleanupCategory::Xcode,
        CleanupCategory::PackageCaches,
        CleanupCategory::DevArtifacts,
        CleanupCategory::NeedsReview,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupItem {
    pub path: PathBuf,
    pub category: CleanupCategory,
    /// 给人看的名字：App 名、工具名（"npm 缓存"）、项目名（"rind / target"）。
    pub label: String,
    pub size: Size,
    /// 最后修改时间（目录取里面最新的，按时间预算估计）。
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    pub hold_back: Option<HoldBack>,
    /// Rind 会不会替用户勾上。
    pub suggested: bool,
}

impl CleanupItem {
    /// 用户能不能勾上它。
    pub fn selectable(&self) -> bool {
        self.hold_back.as_ref().is_none_or(HoldBack::selectable)
    }
}

#[derive(Clone, Debug)]
pub struct CleanupOptions {
    pub measure: bool,
    pub size_budget: Duration,
    pub cancel: Option<Arc<AtomicBool>>,
    /// 到哪些目录下找开发产物。空 = 用 `default_project_roots`。
    pub project_roots: Vec<PathBuf>,
    /// 开发产物多少天没动过才建议清理。
    pub artifact_min_age_days: u32,
    /// 只扫这些类别；空 = 全部。
    pub categories: Vec<CleanupCategory>,
    /// 正在运行的进程的可执行文件路径。None = 扫描时向系统要一次；测试注入假的。
    /// 空表示取不到（真实系统里至少有 Rind 自己），按"说不清谁在运行"处理。
    pub running_executables: Option<Vec<PathBuf>>,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        CleanupOptions {
            measure: true,
            size_budget: Duration::from_secs(20),
            cancel: None,
            project_roots: Vec::new(),
            artifact_min_age_days: 30,
            categories: Vec::new(),
            running_executables: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub items: Vec<CleanupItem>,
}

impl CleanupReport {
    /// 某一类的项，按大小从大到小。
    pub fn in_category(&self, category: CleanupCategory) -> Vec<&CleanupItem> {
        self.items
            .iter()
            .filter(|i| i.category == category)
            .collect()
    }

    /// 某一类的已知大小之和。
    pub fn category_size(&self, category: CleanupCategory) -> u64 {
        self.in_category(category)
            .iter()
            .filter_map(|i| i.size.known())
            .sum()
    }

    pub fn removables(&self) -> Vec<Removable> {
        self.items
            .iter()
            .map(|i| Removable {
                path: i.path.clone(),
                size: i.size,
                hold_back: i.hold_back.clone(),
            })
            .collect()
    }

    pub fn suggested(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| i.suggested)
            .map(|i| i.path.clone())
            .collect()
    }
}

/// 默认到这些地方找项目：`~/work`、`~/Projects`、`~/Developer`、`~/code`、`~/src` 等存在的。
/// 只要真实目录（不是符号链接），给出盘上的真实大小写；不分大小写的卷上 `work` 和 `Work` 只留一个。
pub fn default_project_roots(env: &Env) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in [
        "work",
        "Work",
        "Projects",
        "projects",
        "Developer",
        "code",
        "Code",
        "src",
        "dev",
        "repos",
        "git",
        "GitHub",
    ]
    .iter()
    .filter_map(|d| real_root(env, &env.home.join(d)))
    {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// 认"在运行"用的程序身份：已安装 App 的 bundle id；它不在 `all_apps` 里时，退一步看有没有进程
/// 跑在叫 `bundle` 的 `.app` 包里。
struct Owner {
    name: &'static str,
    bundle_id: &'static str,
    bundle: &'static str,
}

const XCODE: Owner = Owner {
    name: "Xcode",
    bundle_id: "com.apple.dt.Xcode",
    bundle: "Xcode.app",
};

/// 逐条核过的固定位置（相对家目录）：整个移走后会自动重建或按需重新下载。
/// 名字是 `com.apple.*` 也照列（Xcode 缓存），因为这里每一条都是人看过的。
/// 重新下载代价高、或者权限只读可能移不动的（Go 模块、Maven / Gradle 依赖、模型）放 `NeedsReview`。
/// `Xcode` 类的所属程序是 Xcode；包管理器缓存不看在不在运行。
const FIXED: &[(CleanupCategory, &str, &str)] = &[
    (
        CleanupCategory::Xcode,
        "Xcode",
        "Library/Caches/com.apple.dt.Xcode",
    ),
    (
        CleanupCategory::Xcode,
        "CoreSimulator",
        "Library/Developer/CoreSimulator/Caches",
    ),
    (CleanupCategory::PackageCaches, "npm", ".npm/_cacache"),
    (
        CleanupCategory::PackageCaches,
        "Yarn",
        "Library/Caches/Yarn",
    ),
    (CleanupCategory::PackageCaches, "Yarn", ".yarn/berry/cache"),
    (CleanupCategory::PackageCaches, "pnpm", "Library/pnpm/store"),
    (
        CleanupCategory::PackageCaches,
        "pnpm",
        ".local/share/pnpm/store",
    ),
    (
        CleanupCategory::PackageCaches,
        "pnpm",
        "Library/Caches/pnpm",
    ),
    (CleanupCategory::PackageCaches, "Bun", ".bun/install/cache"),
    (CleanupCategory::PackageCaches, "pip", "Library/Caches/pip"),
    (CleanupCategory::PackageCaches, "pip", ".cache/pip"),
    (CleanupCategory::PackageCaches, "uv", ".cache/uv"),
    (
        CleanupCategory::PackageCaches,
        "Poetry",
        "Library/Caches/pypoetry",
    ),
    (
        CleanupCategory::PackageCaches,
        "Cargo",
        ".cargo/registry/cache",
    ),
    (CleanupCategory::NeedsReview, "Gradle", ".gradle/caches"),
    (
        CleanupCategory::PackageCaches,
        "Go",
        "Library/Caches/go-build",
    ),
    (
        CleanupCategory::PackageCaches,
        "Homebrew",
        "Library/Caches/Homebrew",
    ),
    (
        CleanupCategory::PackageCaches,
        "CocoaPods",
        "Library/Caches/CocoaPods",
    ),
    (
        CleanupCategory::PackageCaches,
        "Composer",
        "Library/Caches/composer",
    ),
    (
        CleanupCategory::PackageCaches,
        "SwiftPM",
        "Library/Caches/org.swift.swiftpm",
    ),
    (
        CleanupCategory::PackageCaches,
        "Deno",
        "Library/Caches/deno",
    ),
    (CleanupCategory::NeedsReview, "Go modules", "go/pkg/mod"),
    (CleanupCategory::NeedsReview, "Maven", ".m2/repository"),
    (
        CleanupCategory::NeedsReview,
        "Hugging Face",
        ".cache/huggingface",
    ),
];

/// 这些目录的每个子项单独列进 `NeedsReview`（一个归档、一个系统版本一项），让人逐个判断。
const REVIEW_FOLDERS: &[&str] = &[
    "Library/Developer/Xcode/Archives",
    "Library/Developer/Xcode/iOS DeviceSupport",
    "Library/Developer/Xcode/watchOS DeviceSupport",
    "Library/Developer/Xcode/tvOS DeviceSupport",
    "Library/Developer/Xcode/visionOS DeviceSupport",
];

struct Browser {
    owner: Owner,
    /// 缓存目录直接在这些根下（`ShaderCache`），或在根下的 profile 里（`Default/Code Cache`）。
    roots: &'static [&'static str],
    /// 只认这些名字。profile 本身、Cookies、Local Storage、扩展之类一概不碰。
    caches: &'static [&'static str],
}

const CHROMIUM_CACHES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "GPUPersistentCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "ShaderCache",
    "component_crx_cache",
];

const FIREFOX_CACHES: &[&str] = &["cache2", "startupCache"];

/// Safari 不在这里：它的东西在保护区里。
const BROWSERS: &[Browser] = &[
    Browser {
        owner: Owner {
            name: "Chrome",
            bundle_id: "com.google.Chrome",
            bundle: "Google Chrome.app",
        },
        roots: &[
            "Library/Caches/Google/Chrome",
            "Library/Application Support/Google/Chrome",
        ],
        caches: CHROMIUM_CACHES,
    },
    Browser {
        owner: Owner {
            name: "Edge",
            bundle_id: "com.microsoft.edgemac",
            bundle: "Microsoft Edge.app",
        },
        roots: &[
            "Library/Caches/Microsoft Edge",
            "Library/Application Support/Microsoft Edge",
        ],
        caches: CHROMIUM_CACHES,
    },
    Browser {
        owner: Owner {
            name: "Brave",
            bundle_id: "com.brave.Browser",
            bundle: "Brave Browser.app",
        },
        roots: &[
            "Library/Caches/BraveSoftware/Brave-Browser",
            "Library/Application Support/BraveSoftware/Brave-Browser",
        ],
        caches: CHROMIUM_CACHES,
    },
    Browser {
        owner: Owner {
            name: "Arc",
            bundle_id: "company.thebrowser.Browser",
            bundle: "Arc.app",
        },
        roots: &[
            "Library/Caches/Arc",
            "Library/Caches/Arc/User Data",
            "Library/Application Support/Arc/User Data",
        ],
        caches: CHROMIUM_CACHES,
    },
    Browser {
        owner: Owner {
            name: "Firefox",
            bundle_id: "org.mozilla.firefox",
            bundle: "Firefox.app",
        },
        roots: &[
            "Library/Caches/Firefox/Profiles",
            "Library/Application Support/Firefox/Profiles",
        ],
        caches: FIREFOX_CACHES,
    },
];

/// `~/Library/Caches` 下重建或重新下载代价高的（IDE 索引、语言服务器、工具链、虚拟机镜像），按名字
/// （不分大小写，结尾 `*` 表示前缀）归 `NeedsReview` 而不是 App 缓存：删了能用，但下次要花很久索引或下载，该由人决定。
/// 放 `NeedsReview` 而不是留在 App 缓存里只取消勾选：那样 App 缓存这一类就不再是"都可以放心勾"，
/// 用户也看不出为什么这几项没勾；`NeedsReview` 的文案正好说明了这一点，`.m2`、Go 模块也在那里。
const EXPENSIVE_APP_CACHES: &[&str] = &[
    "JetBrains",       // IDE 索引、共享索引
    "jdtls",           // Java 语言服务器的工作区索引
    "dotslash",        // 按需下载的工具二进制
    "vscode-cpptools", // C/C++ 扩展的 IntelliSense 数据库
    "clangd",          // clangd 的后台索引
    "Coursier",        // Scala / Metals 下载的依赖与语言服务器
    "lima",            // Lima / Colima 的虚拟机镜像
    "colima",
    "ms-playwright*", // Playwright 下载的浏览器和驱动，动辄上 GB
];

/// `~/Library/Logs` 下属于 macOS、名字却不是 `com.apple.*` 的，按前缀（不分大小写）跳过。
const APPLE_LOGS: &[&str] = &[
    "Baseband",
    "PrivacyPreservingMeasurement",
    "SiriTTSService",
    "PhotosSearch",
    "CrashReporter",
    "DiskUtility",
    "fsck_",
];

/// 开发产物的父目录路径里有这些名字（不分大小写）时，多半是工具装插件、扩展的地方（coc 扩展、
/// Raycast 扩展、dotfiles 里的配置），不是用户的项目：列出来，但不建议。
const TOOL_DIRS: &[&str] = &["extensions", "plugins", "addons", "dotfiles"];

/// 有锁文件或 `.git` 的目录才像一个项目。
const NODE_LOCKS: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
];

const GRADLE_FILES: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];

/// 产物目录自己的特征：有其中之一才算（结尾 `/` 表示子目录，否则是文件）。
const ARTIFACT_MARKS: &[(&str, &[&str])] = &[
    (
        "target",
        &["CACHEDIR.TAG", ".rustc_info.json", "debug/", "release/"],
    ),
    (
        "node_modules",
        &[
            ".package-lock.json",
            ".modules.yaml",
            ".yarn-integrity",
            ".yarn-state.yml",
            ".pnpm/",
        ],
    ),
    ("Pods", &["Manifest.lock"]),
    (".next", &["BUILD_ID", "cache/"]),
    (
        "build",
        &[
            "tmp/",
            "intermediates/",
            "classes/",
            "generated/",
            "kotlin/",
        ],
    ),
    (
        ".gradle",
        &[
            "buildOutputCleanup/",
            "vcs-1/",
            "checksums/",
            "file-system.probe",
        ],
    ),
    (".venv", &["pyvenv.cfg"]),
    ("venv", &["pyvenv.cfg"]),
];

/// 虚拟环境旁边有这些（或 `requirements*.txt`）才建议：删了能照着装回来。
const PYTHON_MANIFESTS: &[&str] = &[
    "pyproject.toml",
    "Pipfile",
    "uv.lock",
    "poetry.lock",
    "setup.py",
];

/// 从项目根往下最多几层找产物。
const PROJECT_DEPTH: usize = 6;
/// 算项目活跃度最多看多少项；看不完就说不清，不替用户勾。
const PROJECT_ENTRIES: usize = 20_000;
/// 估计一般目录的修改时间：看几层、多少项。
const CHANGE_DEPTH: usize = 3;
const CHANGE_ENTRIES: usize = 2_000;

/// 扫描时找到、还没判断和度量的一项。
struct Found {
    path: PathBuf,
    category: CleanupCategory,
    label: String,
    /// 来自 `FIXED`：逐条核过，不按名字当作 macOS 的东西。
    vetted: bool,
    /// 开发产物所在的项目：用项目的活跃度代替产物自己的修改时间。
    project: Option<PathBuf>,
    /// 列出来，但不替用户勾（不像独立项目的开发产物）。
    doubtful: bool,
}

impl Found {
    fn new(path: PathBuf, category: CleanupCategory, label: String) -> Found {
        Found {
            path,
            category,
            label,
            vetted: false,
            project: None,
            doubtful: false,
        }
    }
}

/// 扫描可清理的东西。`all_apps` 用来给缓存起名、判断所属程序在不在运行。
/// 取消后返回空，也不报 `Finished`（与 `leftovers::scan` 一致）。
pub fn scan(
    env: &Env,
    all_apps: &[InstalledApp],
    opts: &CleanupOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> CleanupReport {
    use CleanupCategory::*;
    let cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    };
    let wanted = |c: CleanupCategory| opts.categories.is_empty() || opts.categories.contains(&c);

    // 进程列表只取一次，而且只在有类别要看"在不在运行"时才取。
    let exes = match &opts.running_executables {
        Some(exes) => exes.clone(),
        None if [AppCaches, BrowserCaches, Xcode].into_iter().any(wanted) => process_executables(),
        None => Vec::new(),
    };
    let running = Running::new(all_apps, exes);

    let mut found: Vec<Found> = Vec::new();
    for category in CleanupCategory::ALL {
        if !wanted(category) {
            continue;
        }
        if cancelled() {
            return CleanupReport::default();
        }
        match category {
            AppCaches => app_caches(env, all_apps, false, on_progress, &mut found),
            Logs => logs(env, all_apps, on_progress, &mut found),
            BrowserCaches => browser_caches(env, on_progress, &mut found),
            Xcode => {
                fixed(env, Xcode, on_progress, &mut found);
                derived_data(env, on_progress, &mut found);
            }
            PackageCaches => fixed(env, PackageCaches, on_progress, &mut found),
            DevArtifacts => dev_artifacts(env, opts, &cancelled, on_progress, &mut found),
            NeedsReview => {
                fixed(env, NeedsReview, on_progress, &mut found);
                app_caches(env, all_apps, true, on_progress, &mut found);
                review_folders(env, on_progress, &mut found);
            }
        }
    }
    if cancelled() {
        return CleanupReport::default();
    }
    let found = dedup(found);

    // SAFETY: geteuid 没有参数，总是成功。
    let euid = unsafe { libc::geteuid() };
    let judged: Vec<(Option<HoldBack>, Option<DateTime<Utc>>)> = found
        .par_iter()
        .map(|f| {
            if cancelled() {
                return (None, None);
            }
            let modified = match &f.project {
                Some(project) => project_activity(project),
                None => latest_change(&f.path),
            };
            let in_use = in_use_of(env, &running, f.category, &f.path);
            (
                hold_back_for(env, &f.path, f.vetted, in_use, euid),
                modified,
            )
        })
        .collect();
    if cancelled() {
        return CleanupReport::default();
    }

    let sizes = if opts.measure && !found.is_empty() {
        let paths: Vec<PathBuf> = found.iter().map(|f| f.path.clone()).collect();
        crate::size::measure_all_with(
            &paths,
            opts.size_budget,
            opts.cancel.as_deref(),
            on_progress,
        )
    } else {
        vec![Size::Unknown; found.len()]
    };
    if cancelled() {
        return CleanupReport::default();
    }

    let now = Utc::now();
    let min_age = chrono::Duration::days(i64::from(opts.artifact_min_age_days));
    let mut items: Vec<CleanupItem> = found
        .into_iter()
        .zip(judged)
        .zip(sizes)
        // 量出来是 0 的（空目录）没什么可清的，不列，免得一屏的 0 B。
        .filter(|(_, size)| *size != Size::Known(0))
        .map(|((f, (hold_back, modified)), size)| {
            // 说不清项目多久没动（`modified` 是 None）就不算够老。
            let old_enough =
                f.category != DevArtifacts || modified.is_some_and(|m| now - m >= min_age);
            let suggested = f.category != NeedsReview
                && !f.doubtful
                && !running.unsure_about(f.category)
                && hold_back.is_none()
                && size.known().is_some_and(|n| n > 0)
                && old_enough;
            CleanupItem {
                path: f.path,
                category: f.category,
                label: f.label,
                size,
                modified,
                hold_back,
                suggested,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| b.size.known().cmp(&a.size.known()))
            .then_with(|| a.path.cmp(&b.path))
    });
    on_progress(ScanProgress::Finished);
    CleanupReport { items }
}

/// `~/Library/Caches` 一层。`com.apple.*` 不列；归到别的类别的（浏览器、包管理器、Xcode）也不列，
/// 不管这次扫不扫那些类别，免得只扫 App 缓存时把 Homebrew 整个当成一个 App 的缓存。
/// `expensive` 为真时只列 `EXPENSIVE_APP_CACHES`（归 `NeedsReview`），否则只列其余的（归 App 缓存），
/// 这样按类别过滤时两边各管各的。
fn app_caches(
    env: &Env,
    apps: &[InstalledApp],
    expensive: bool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
    out: &mut Vec<Found>,
) {
    let Some(caches) = real_root(env, &env.user_library.join("Caches")) else {
        return;
    };
    on_progress(ScanProgress::Location(env.display(&caches)));
    let claimed: std::collections::HashSet<String> = FIXED
        .iter()
        .map(|(_, _, path)| *path)
        .chain(BROWSERS.iter().flat_map(|b| b.roots.iter().copied()))
        .filter_map(|path| path.strip_prefix("Library/Caches/"))
        .filter_map(|rest| rest.split('/').next())
        .map(str::to_lowercase)
        .collect();
    for (name, path) in children(&caches) {
        if is_apple(&name) || claimed.contains(&name.to_lowercase()) {
            continue;
        }
        let lower = name.to_lowercase();
        let costly = EXPENSIVE_APP_CACHES.iter().any(|e| {
            let e = e.to_lowercase();
            match e.strip_suffix('*') {
                Some(prefix) => lower.starts_with(prefix),
                None => lower == e,
            }
        });
        if costly != expensive {
            continue;
        }
        let category = if costly {
            CleanupCategory::NeedsReview
        } else {
            CleanupCategory::AppCaches
        };
        // 更新暂存目录（`*.ShipIt`、`*updater*`）：App 正在更新时移走会把更新弄坏，所以要能确认
        // 所属 App 没在运行才建议；`com.foo.ShipIt` 按子 id 就认得出主人，`foo-updater` 去掉尾巴再认一次，
        // 还认不出就说不清它的主人在不在运行，列出但不建议。
        let staging = is_staging(&name);
        let owner = cache_owner(&name, apps);
        let label = owner.map_or_else(|| name.clone(), |i| apps[i].name.clone());
        let mut item = Found::new(path, category, label);
        item.doubtful = staging && owner.is_none();
        out.push(item);
    }
}

/// `~/Library/Logs` 一层，加上诊断报告。`DiagnosticReports` 目录本身是 macOS 的（`is_system_owned`），
/// 所以列的是里面的每份报告（只列文件）。
fn logs(
    env: &Env,
    apps: &[InstalledApp],
    on_progress: &(dyn Fn(ScanProgress) + Sync),
    out: &mut Vec<Found>,
) {
    let Some(logs) = real_root(env, &env.user_library.join("Logs")) else {
        return;
    };
    on_progress(ScanProgress::Location(env.display(&logs)));
    for (name, path) in children(&logs) {
        if is_apple(&name) || name.eq_ignore_ascii_case("DiagnosticReports") || is_apple_log(&name)
        {
            continue;
        }
        let label = owner_of(&name, apps).map_or(name, |i| apps[i].name.clone());
        out.push(Found::new(path, CleanupCategory::Logs, label));
    }
    let Some(reports) = real_root(env, &logs.join("DiagnosticReports")) else {
        return;
    };
    on_progress(ScanProgress::Location(env.display(&reports)));
    for (name, path) in children(&reports) {
        // 报告都是文件；`Retired` 之类的子目录是 macOS 自己管的。
        if is_apple(&name) || !is_real_file(&path) {
            continue;
        }
        let label = format!("DiagnosticReports / {name}");
        out.push(Found::new(path, CleanupCategory::Logs, label));
    }
}

fn browser_caches(env: &Env, on_progress: &(dyn Fn(ScanProgress) + Sync), out: &mut Vec<Found>) {
    for browser in BROWSERS {
        for root in browser.roots {
            let Some(root) = real_root(env, &env.home.join(root)) else {
                continue;
            };
            on_progress(ScanProgress::Location(env.display(&root)));
            let mut places = vec![(browser.owner.name.to_string(), root.clone())];
            for (profile, dir) in children(&root) {
                // 缓存目录自己不是 profile（`GPUPersistentCache/GPUCache` 只算外面那一项）。
                if is_real_dir(&dir) && !browser.caches.contains(&profile.as_str()) {
                    places.push((format!("{} / {profile}", browser.owner.name), dir));
                }
            }
            for (prefix, dir) in places {
                // 按盘上的名字认（区分大小写），输出的就是真实路径。列不了目录（隐私保护挡住）时
                // 退回按名字 lstat：这样的项会是"读不了"，不建议，移动出口还会再核真实路径。
                let entries = if std::fs::read_dir(&dir).is_ok() {
                    children(&dir)
                } else {
                    browser
                        .caches
                        .iter()
                        .map(|c| (c.to_string(), dir.join(c)))
                        .collect()
                };
                for (cache, path) in entries {
                    if !browser.caches.contains(&cache.as_str()) || !is_real_dir(&path) {
                        continue;
                    }
                    let label = format!("{prefix} / {cache}");
                    out.push(Found::new(path, CleanupCategory::BrowserCaches, label));
                }
            }
        }
    }
}

fn fixed(
    env: &Env,
    category: CleanupCategory,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
    out: &mut Vec<Found>,
) {
    for (_, label, path) in FIXED.iter().filter(|(c, _, _)| *c == category) {
        let Some(path) = real_root(env, &env.home.join(path)) else {
            continue;
        };
        on_progress(ScanProgress::Location(env.display(&path)));
        let mut item = Found::new(path, category, label.to_string());
        item.vetted = true;
        out.push(item);
    }
}

/// DerivedData 里每个项目一项。
fn derived_data(env: &Env, on_progress: &(dyn Fn(ScanProgress) + Sync), out: &mut Vec<Found>) {
    let Some(derived) = real_root(env, &env.user_library.join("Developer/Xcode/DerivedData"))
    else {
        return;
    };
    on_progress(ScanProgress::Location(env.display(&derived)));
    for (name, path) in children(&derived) {
        let label = format!("DerivedData / {}", derived_data_project(&name));
        out.push(Found::new(path, CleanupCategory::Xcode, label));
    }
}

fn review_folders(env: &Env, on_progress: &(dyn Fn(ScanProgress) + Sync), out: &mut Vec<Found>) {
    for folder in REVIEW_FOLDERS {
        let Some(folder) = real_root(env, &env.home.join(folder)) else {
            continue;
        };
        on_progress(ScanProgress::Location(env.display(&folder)));
        let prefix = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (name, path) in children(&folder) {
            let label = format!("{prefix} / {name}");
            out.push(Found::new(path, CleanupCategory::NeedsReview, label));
        }
    }
}

fn dev_artifacts(
    env: &Env,
    opts: &CleanupOptions,
    cancelled: &dyn Fn() -> bool,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
    out: &mut Vec<Found>,
) {
    let roots = if opts.project_roots.is_empty() {
        default_project_roots(env)
    } else {
        opts.project_roots.clone()
    };
    // APFS 不分大小写：`~/work` 和 `~/Work` 可能是同一处，按盘上的真实路径去重。
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in roots {
        // 家目录下的根：它和到家目录的每一级都不能是符号链接。家目录外（用户明确给的）：
        // 根自己不能是符号链接，往下用解析后的真实路径。
        let root = if root.starts_with(&env.home) {
            real_root(env, &root)
        } else if is_real_dir(&root) {
            std::fs::canonicalize(&root).ok()
        } else {
            None
        };
        let Some(root) = root else {
            continue;
        };
        let real = real_path(&root);
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        if cancelled() {
            return;
        }
        on_progress(ScanProgress::Location(env.display(&root)));
        find_artifacts(env, &root, cancelled, out);
    }
}

/// 从 `root` 往下最多 `PROJECT_DEPTH` 层找有旁证的构建产物。不跟符号链接，不进隐藏目录、
/// `~/Library` 和 `node_modules`（里面每个包都有 `package.json`，进去会把依赖的依赖当成项目）；
/// 找到的产物也不再往里走。
fn find_artifacts(env: &Env, root: &Path, cancelled: &dyn Fn() -> bool, out: &mut Vec<Found>) {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if cancelled() {
            return;
        }
        for (name, child) in children_with_hidden(&dir) {
            if !is_real_dir(&child) {
                continue;
            }
            if is_artifact(&dir, &name, &child) {
                let project = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                let doubtful = is_doubtful(&dir, &name, &child);
                let mut item = Found::new(
                    child,
                    CleanupCategory::DevArtifacts,
                    format!("{project} / {name}"),
                );
                item.doubtful = doubtful;
                item.project = Some(dir.clone());
                out.push(item);
                continue;
            }
            if depth + 1 >= PROJECT_DEPTH
                || name.starts_with('.')
                || name == "node_modules"
                || child == env.user_library
            {
                continue;
            }
            stack.push((child, depth + 1));
        }
    }
}

/// `dir` 下名叫 `name` 的子目录 `child` 是不是有旁证的构建产物。只看名字不够：
/// 叫 `target`、`build` 的目录也可能是用户自己的东西，所以父目录要有项目文件，产物自己也要有
/// 构建工具留下的特征（`ARTIFACT_MARKS`）。`node_modules` 只有 `.bin` 也算（bun、老 npm 不留别的），
/// 但不建议（`is_doubtful`）。
fn is_artifact(dir: &Path, name: &str, child: &Path) -> bool {
    let has = |file: &str| is_real_file(&dir.join(file));
    let project = match name {
        // 依赖装在哪都有 package.json；还要有锁文件或 `.git`，才像用户自己的项目。
        "node_modules" => {
            has("package.json") && (NODE_LOCKS.iter().any(|f| has(f)) || has_git(dir))
        }
        ".next" => has("package.json"),
        "target" => has("Cargo.toml"),
        ".venv" | "venv" => true,
        "build" | ".gradle" => GRADLE_FILES.iter().any(|f| has(f)),
        "Pods" => has("Podfile"),
        _ => false,
    };
    project
        && (has_marks(name, child) || (name == "node_modules" && is_real_dir(&child.join(".bin"))))
}

/// 产物目录自己有没有构建工具留下的特征。
fn has_marks(name: &str, child: &Path) -> bool {
    ARTIFACT_MARKS
        .iter()
        .filter(|(n, _)| *n == name)
        .flat_map(|(_, marks)| marks.iter())
        .any(|mark| match mark.strip_suffix('/') {
            Some(dir) => is_real_dir(&child.join(dir)),
            None => is_real_file(&child.join(mark)),
        })
}

fn has_git(dir: &Path) -> bool {
    // `.git` 也可能是文件（worktree、submodule）。
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

/// 列出来但不建议：路径里有工具目录（`TOOL_DIRS`）；`target` 旁边既没有 `Cargo.lock` 也没有 `.git`
/// （不像一个独立的项目，可能是别人的东西）；虚拟环境旁边没有依赖清单（删了不知道怎么装回来）；
/// `node_modules` 只有 `.bin`、没有包管理器的记录。
fn is_doubtful(dir: &Path, name: &str, child: &Path) -> bool {
    let in_tool_dir = dir.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        TOOL_DIRS.iter().any(|t| t.eq_ignore_ascii_case(&c))
    });
    in_tool_dir
        || (name == "target" && !is_real_file(&dir.join("Cargo.lock")) && !has_git(dir))
        || (matches!(name, ".venv" | "venv") && !has_python_manifest(dir))
        || (name == "node_modules" && !has_marks(name, child))
}

/// 项目里有没有 Python 依赖清单：`requirements*.txt`、`pyproject.toml` 等。
fn has_python_manifest(dir: &Path) -> bool {
    PYTHON_MANIFESTS.iter().any(|f| is_real_file(&dir.join(f)))
        || children(dir).iter().any(|(name, path)| {
            let lower = name.to_lowercase();
            lower.starts_with("requirements") && lower.ends_with(".txt") && is_real_file(path)
        })
}

/// 同一路径只留一条（先找到的）；一条在另一条下面时留里面那条、去掉外层：里面那条的类别、
/// 名字、在不在用都更准，外层里还混着别的东西，整个移走会把别处归了类的、被挡住的一起带走。
fn dedup(mut found: Vec<Found>) -> Vec<Found> {
    // 稳定排序：同一路径，先找到的仍在前面。按路径分量比较，子孙紧跟在祖先后面。
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.dedup_by(|later, earlier| later.path == earlier.path);
    let mut kept = Vec::with_capacity(found.len());
    let mut iter = found.into_iter().peekable();
    while let Some(item) = iter.next() {
        if iter
            .peek()
            .is_some_and(|next| next.path.starts_with(&item.path))
        {
            continue;
        }
        kept.push(item);
    }
    kept
}

/// 规则与卸载一致：保护区 → macOS 的 → 要管理员 → 在运行 → 读不了。
/// 在运行排在读不了前面：前者不能选，后者能手动选，挡得更严的说了算。
/// 被隐私保护（TCC）挡住的不算"要管理员"：`needs_admin_as` 看到父目录不可写就会这么判，
/// 但那其实是读不了，换个有权限的进程（或给了完全磁盘访问权限）就能动。
/// `vetted`：来自 `FIXED`，不按名字当作 macOS 的东西。`in_use`：所属程序在运行时它的名字。
fn hold_back_for(
    env: &Env,
    path: &Path,
    vetted: bool,
    in_use: Option<String>,
    euid: u32,
) -> Option<HoldBack> {
    if let Some(why) = protect::check(env, path) {
        return Some(HoldBack::Protected { why });
    }
    if !vetted && protect::is_system_owned(env, path) {
        return Some(HoldBack::SystemOwned);
    }
    let privacy = privacy_blocked(path, euid);
    if !privacy && crate::leftovers::needs_admin_as(path, euid) {
        return Some(HoldBack::NeedsAdmin);
    }
    if let Some(app) = in_use {
        return Some(HoldBack::InUse { app });
    }
    if privacy || !crate::leftovers::accessible(path, libc::R_OK) {
        return Some(HoldBack::Unreadable);
    }
    None
}

/// 移动前再查一次在用：重新取进程列表（`running_executables` 为 None 时向系统要；测试注入假的），
/// 按每项的类别和路径重新认所属程序，刷新 `hold_back`（连同保护区等一起重算）和 `suggested`。
/// 只会收紧建议、不会新增：扫描时没勾的，复查后也不替用户勾。进程列表取不到（空）时，
/// 应用缓存、浏览器缓存、Xcode 一律不建议。调用方移动前只该移 `selectable()` 的项。
pub fn recheck_in_use(
    env: &Env,
    report: &mut CleanupReport,
    all_apps: &[InstalledApp],
    running_executables: Option<Vec<PathBuf>>,
) {
    let exes = running_executables.unwrap_or_else(process_executables);
    let running = Running::new(all_apps, exes);
    // SAFETY: geteuid 没有参数，总是成功。
    let euid = unsafe { libc::geteuid() };
    for item in &mut report.items {
        let vetted = FIXED.iter().any(|(c, _, p)| {
            *c == item.category
                && env
                    .home
                    .join(p)
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&item.path.to_string_lossy())
        });
        let in_use = in_use_of(env, &running, item.category, &item.path);
        item.hold_back = hold_back_for(env, &item.path, vetted, in_use, euid);
        item.suggested =
            item.suggested && item.hold_back.is_none() && !running.unsure_about(item.category);
    }
}

/// 一项的所属程序在不在运行，在就给出它的名字。扫描和执行前复查共用，按类别和路径认：
/// `~/Library/Caches` 下一层按名字认主人（已安装 App、运行中的 bundle id，或可执行文件就叫这个名字的进程，
/// 比如 `Caches/huhu` 对应运行中的 `Huhu.app`、`Caches/cc-agentd` 对应运行中的 `…/agentbin/cc-agentd`）；
/// 浏览器缓存看在哪个浏览器的根下；Xcode 类看 Xcode。日志、包管理器缓存、开发产物不看运行状态。
fn in_use_of(
    env: &Env,
    running: &Running,
    category: CleanupCategory,
    path: &Path,
) -> Option<String> {
    use CleanupCategory::*;
    match category {
        AppCaches | NeedsReview => {
            let caches = env.user_library.join("Caches");
            if path.parent() != Some(caches.as_path()) {
                return None;
            }
            let name = path.file_name()?.to_string_lossy();
            cache_owner(&name, running.apps)
                .and_then(|i| running.app(i))
                .or_else(|| running.bundle(&name))
                .or_else(|| {
                    is_staging(&name)
                        .then(|| running.bundle(update_stem(&name)))
                        .flatten()
                })
                .or_else(|| running.exe(&name))
        }
        BrowserCaches => {
            // 输出的是真实大小写，表里的未必；按分量、不分大小写比。
            let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
            let path = lower(path);
            BROWSERS
                .iter()
                .find(|b| {
                    b.roots
                        .iter()
                        .any(|r| path.starts_with(lower(&env.home.join(r))))
                })
                .and_then(|b| running.owner(&b.owner))
        }
        Xcode => running.owner(&XCODE),
        Logs | PackageCaches | DevArtifacts => None,
    }
}

/// 被 macOS 隐私保护（TCC）挡住了：父目录是自己的，列父目录或 lstat 本身却得到 `EPERM`。
/// 普通的权限不够是 `EACCES`，不算。
fn privacy_blocked(path: &Path, me: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    if !std::fs::metadata(parent).is_ok_and(|m| m.uid() == me) {
        return false;
    }
    is_eperm(std::fs::read_dir(parent).err()) || is_eperm(std::fs::symlink_metadata(path).err())
}

fn is_eperm(error: Option<std::io::Error>) -> bool {
    error.and_then(|e| e.raw_os_error()) == Some(libc::EPERM)
}

/// 里面最近一次修改。只看前几层、前 `CHANGE_ENTRIES` 项，是个估计。
fn latest_change(path: &Path) -> Option<DateTime<Utc>> {
    WalkDir::new(path)
        .follow_links(false)
        .max_depth(CHANGE_DEPTH)
        .into_iter()
        .take(CHANGE_ENTRIES)
        .filter_map(|e| e.ok()?.metadata().ok()?.modified().ok())
        .max()
        .map(DateTime::<Utc>::from)
}

/// 项目最近一次被人动过：看产物和隐藏项以外的文件和子目录（项目根自己不算——建产物也会改它）。
/// 只跳过按 `is_artifact` 确认了的产物；源码里叫 `build`、`target` 的普通目录照算。
/// 看不完（太大）、有读不了的、一项都没有，都是 None：说不清多久没动，就不替用户勾。
fn project_activity(project: &Path) -> Option<DateTime<Utc>> {
    let walker = WalkDir::new(project)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0
                || !(name.starts_with('.')
                    || (e.file_type().is_dir()
                        && e.path()
                            .parent()
                            .is_some_and(|dir| is_artifact(dir, &name, e.path()))))
        });
    let mut latest = None;
    for (seen, entry) in walker.enumerate() {
        if seen >= PROJECT_ENTRIES {
            return None;
        }
        let modified = entry.ok()?.metadata().ok()?.modified().ok()?;
        latest = latest.max(Some(modified));
    }
    latest.map(DateTime::<Utc>::from)
}

/// 正在运行的进程，以及已安装的 App 里哪些在运行。进程列表只取一次。
struct Running<'a> {
    apps: &'a [InstalledApp],
    /// 进程可执行文件在盘上的真实路径。
    exes: Vec<PathBuf>,
    /// 与 `apps` 对齐。
    app_running: Vec<bool>,
    /// 运行中进程所属 bundle 的 id（小写）→ 给人看的名字。见 `running_bundle_ids`。
    bundle_ids: HashMap<String, String>,
}

impl<'a> Running<'a> {
    fn new(apps: &'a [InstalledApp], exes: Vec<PathBuf>) -> Running<'a> {
        // 两边都按真实路径比：`/Applications/Foo.app` 是符号链接时，进程报的是它指向的地方。
        let exes: Vec<PathBuf> = exes.iter().map(|e| real_path(e)).collect();
        let bundle_ids = running_bundle_ids(&exes);
        let app_running = apps
            .iter()
            .map(|app| {
                !exes.is_empty() && {
                    let bundle = real_path(&app.path);
                    exes.iter().any(|e| e.starts_with(&bundle))
                }
            })
            .collect();
        Running {
            apps,
            exes,
            app_running,
            bundle_ids,
        }
    }

    /// 进程列表取不到，说不清这一类的主人在不在运行：应用缓存、浏览器缓存、Xcode 不建议。
    fn unsure_about(&self, category: CleanupCategory) -> bool {
        use CleanupCategory::*;
        self.exes.is_empty() && matches!(category, AppCaches | BrowserCaches | Xcode)
    }

    /// 名字等于某个运行中 bundle 的 id、或以 `id.` 开头（不分大小写），给出那个程序的名字。
    fn bundle(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        if let Some(app) = self.bundle_ids.get(&lower) {
            return Some(app.clone());
        }
        self.bundle_ids
            .iter()
            .filter(|(id, _)| lower.starts_with(&format!("{id}.")))
            .max_by_key(|(id, _)| id.len())
            .map(|(_, app)| app.clone())
    }

    /// `apps[index]` 在运行就给出它的名字。
    fn app(&self, index: usize) -> Option<String> {
        self.app_running[index].then(|| self.apps[index].name.clone())
    }

    /// 有没有在运行的进程的可执行文件就叫这个名字（`Caches/huhu` ← `Huhu.app/Contents/MacOS/Huhu`，
    /// `Caches/cc-agentd` ← `…/agentbin/cc-agentd`）。只比最后一段，不分大小写；认错了只会让 Rind 更保守，
    /// 不会把还在用的东西划成可清理。
    fn exe(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        self.exes.iter().find_map(|exe| {
            let file = exe.file_name()?.to_string_lossy().to_string();
            let stem = exe
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| file.clone());
            (file.to_lowercase() == lower || stem.to_lowercase() == lower).then_some(stem)
        })
    }

    fn owner(&self, owner: &Owner) -> Option<String> {
        let installed = self
            .apps
            .iter()
            .zip(&self.app_running)
            .find(|(app, running)| {
                **running
                    && app
                        .bundle_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(owner.bundle_id))
            });
        if let Some((app, _)) = installed {
            return Some(app.name.clone());
        }
        if self.bundle(owner.bundle_id).is_some() {
            return Some(owner.name.to_string());
        }
        self.exes
            .iter()
            .any(|exe| {
                exe.components()
                    .any(|c| c.as_os_str().to_string_lossy() == owner.bundle)
            })
            .then(|| owner.name.to_string())
    }
}

fn process_executables() -> Vec<PathBuf> {
    let mut system = System::new_with_specifics(
        RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::nothing().with_exe(UpdateKind::Always)),
    );
    system.refresh_processes(ProcessesToUpdate::All, true);
    system
        .processes()
        .values()
        .filter_map(|p| p.exe().map(Path::to_path_buf))
        .collect()
}

fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 运行中进程所属的 bundle id：从可执行文件往上找 `.app` / `.appex` / `.bundle` / `.xpc`，每一层都算；
/// 最外层 App 里直接内嵌的 App、扩展、XPC 服务也算（搜狗输入法在跑时，它包里的 `SogouTaskManager.app`
/// 的缓存也在用）。名字取最外层 bundle 的。输入法、`/Library` 下的 helper、登录项都不在已安装 App 里，靠这个认。
fn running_bundle_ids(exes: &[PathBuf]) -> HashMap<String, String> {
    // 最外层 bundle → 进程所在的各层 bundle。
    let mut chains: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for exe in exes {
        let chain: Vec<PathBuf> = exe
            .ancestors()
            .skip(1)
            .filter(|p| has_extension(p, &["app", "appex", "bundle", "xpc"]))
            .map(Path::to_path_buf)
            .collect();
        if let Some(outer) = chain.last().cloned() {
            chains.entry(outer).or_default().extend(chain);
        }
    }
    let mut ids = HashMap::new();
    for (outer, bundles) in chains {
        let info = bundle_info(&outer);
        let name = info
            .as_ref()
            .and_then(|d| {
                ["CFBundleDisplayName", "CFBundleName"]
                    .iter()
                    .find_map(|k| d.get(k)?.as_string())
                    .map(str::to_string)
            })
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| {
                outer
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let embedded = EMBEDDED_FOLDERS.iter().flat_map(|folder| {
            children(&outer.join("Contents").join(folder))
                .into_iter()
                .map(|(_, path)| path)
                .filter(|p| has_extension(p, &["app", "appex", "xpc"]) && is_real_dir(p))
        });
        for bundle in bundles.into_iter().chain(embedded) {
            let id = if bundle == outer {
                info.as_ref().and_then(bundle_id_in)
            } else {
                bundle_info(&bundle).as_ref().and_then(bundle_id_in)
            };
            if let Some(id) = id {
                ids.entry(id.to_lowercase()).or_insert_with(|| name.clone());
            }
        }
    }
    ids
}

/// 最外层 App 里放内嵌 App / 扩展 / XPC 服务的地方（相对 `Contents`；空串是 `Contents` 自己）。
const EMBEDDED_FOLDERS: &[&str] = &[
    "",
    "Helpers",
    "Library/LoginItems",
    "Library/LaunchServices",
    "XPCServices",
    "PlugIns",
    "MacOS",
    "Frameworks",
];

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .is_some_and(|e| extensions.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn bundle_id_in(info: &plist::Dictionary) -> Option<String> {
    let id = info.get("CFBundleIdentifier")?.as_string()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// bundle 的 Info.plist（`Contents/Info.plist`，或扁平布局的 `Info.plist`）。只读真实文件，超过 1 MB 不读：
/// 嵌套很深的 plist 能把栈撑爆。
fn bundle_info(bundle: &Path) -> Option<plist::Dictionary> {
    const LIMIT: u64 = 1024 * 1024;
    ["Contents/Info.plist", "Info.plist"]
        .iter()
        .find_map(|rel| {
            let path = bundle.join(rel);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > LIMIT {
                return None;
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&path)
                .ok()?
                .take(LIMIT + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > LIMIT {
                return None;
            }
            plist::Value::from_reader(std::io::Cursor::new(bytes))
                .ok()?
                .into_dictionary()
        })
}

/// 缓存目录名对应哪个已安装 App：bundle id 相等 > 是它的子 id（`com.foo.bar.ShipIt`）或内嵌组件的 id
/// > App 名相等。都不区分大小写。
fn owner_of(name: &str, apps: &[InstalledApp]) -> Option<usize> {
    let lower = name.to_lowercase();
    let mut best: Option<(u8, usize)> = None;
    for (index, app) in apps.iter().enumerate() {
        let id = app.bundle_id.as_deref().map(str::to_lowercase);
        let score = if id.as_deref() == Some(lower.as_str()) {
            3
        } else if id.is_some_and(|id| lower.starts_with(&format!("{id}.")))
            || app
                .embedded_ids
                .iter()
                .any(|e| e.eq_ignore_ascii_case(name))
        {
            2
        } else if app.name.eq_ignore_ascii_case(name) {
            1
        } else {
            0
        };
        if score > best.map_or(0, |(s, _)| s) {
            best = Some((score, index));
        }
    }
    best.map(|(_, index)| index)
}

/// `~/Library/Caches` 下一项的主人（已安装 App 的下标）。更新暂存目录认不出时去掉尾巴再认一次。
fn cache_owner(name: &str, apps: &[InstalledApp]) -> Option<usize> {
    owner_of(name, apps).or_else(|| {
        is_staging(name)
            .then(|| owner_of(update_stem(name), apps))
            .flatten()
    })
}

/// 更新暂存目录（`*.ShipIt`、`*updater*`）：App 正在更新时移走会把更新弄坏。
fn is_staging(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".shipit") || lower.contains("updater")
}

/// `Foo.ShipIt`、`foo-updater`、`FooUpdater` → `Foo` / `foo`：更新暂存目录名去掉尾巴，拿来找主人。
fn update_stem(name: &str) -> &str {
    let lower = name.to_lowercase();
    for suffix in [".shipit", "-updater", "_updater", ".updater", "updater"] {
        if lower.ends_with(suffix) && name.is_char_boundary(name.len() - suffix.len()) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

fn is_apple_log(name: &str) -> bool {
    let lower = name.to_lowercase();
    APPLE_LOGS
        .iter()
        .any(|p| lower.starts_with(&p.to_lowercase()))
}

fn is_apple(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "com.apple" || lower.starts_with("com.apple.") || lower.starts_with("group.com.apple.")
}

/// 是目录，而且不是指向目录的符号链接。
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// 是普通文件，而且不是符号链接。
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// 扫描根：要在家目录下，从家目录往下每一级（含它自己）都是真实目录、不是符号链接；
/// 是就给出盘上真实大小写的路径（仍以 `env.home` 开头，保护区按它判断）。家目录自己不查。
/// 防的是 `~/Library/Caches` 被换成指向 `~/Library/Containers` 的链接，把别的 App 的数据当缓存列出来。
fn real_root(env: &Env, path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(&env.home).ok()?;
    let mut current = env.home.clone();
    let mut depth = 0;
    for component in rest.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        current.push(name);
        if !is_real_dir(&current) {
            return None;
        }
        depth += 1;
    }
    let real = std::fs::canonicalize(path).ok()?;
    let tail = real
        .strip_prefix(std::fs::canonicalize(&env.home).ok()?)
        .ok()?;
    // 查完到解析之间被换成链接，层数或位置就对不上。
    (tail.components().count() == depth).then(|| env.home.join(tail))
}

/// 目录下的直接子项（不含隐藏的），按名字排好。读不了就是空的。符号链接不列。
fn children(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = children_with_hidden(dir);
    out.retain(|(name, _)| !name.starts_with('.'));
    out
}

fn children_with_hidden(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

/// `Rind-abcdefghijklmnopqrstuvwxyzab` → `Rind`：去掉 Xcode 加的哈希尾巴。
fn derived_data_project(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((project, hash))
            if hash.len() >= 20 && hash.chars().all(|c| c.is_ascii_lowercase()) =>
        {
            project
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::testing::make_app;
    use crate::{AppSource, ProtectReason};
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    struct Fixture {
        _dir: tempfile::TempDir,
        env: Env,
        apps: Vec<InstalledApp>,
        projects: PathBuf,
    }

    fn mk(path: &Path) {
        mk_sized(path, 8192);
    }

    fn mk_sized(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("data.bin"), vec![0u8; bytes]).unwrap();
    }

    fn file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn app(dir: &Path, name: &str, id: &str) -> InstalledApp {
        let path = make_app(dir, name, id);
        InstalledApp {
            executable: Some(path.join("Contents/MacOS").join(name)),
            path,
            name: name.into(),
            bundle_id: Some(id.into()),
            version: None,
            team_id: None,
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        }
    }

    /// 把 `dir` 里的一切（含它自己）的修改时间拨回 `days` 天前。先改里面再改外面。
    fn age(dir: &Path, days: u64) {
        let then = SystemTime::now() - Duration::from_secs(days * 86_400);
        for entry in WalkDir::new(dir).contents_first(true) {
            let entry = entry.unwrap();
            std::fs::File::open(entry.path())
                .unwrap()
                .set_modified(then)
                .unwrap();
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let apps_dir = env.app_dirs[0].clone();
        let apps = vec![
            app(&apps_dir, "Foo", "com.acme.foo"),
            app(&apps_dir, "Google Chrome", "com.google.Chrome"),
        ];
        let home = env.home.clone();
        let ul = env.user_library.clone();

        mk(&ul.join("Caches/com.acme.foo"));
        mk(&ul.join("Caches/SomeTool"));
        mk(&ul.join("Caches/com.apple.Safari"));
        mk(&ul.join("Caches/CloudKit")); // macOS 的系统缓存目录
        mk(&ul.join("Caches/Homebrew/downloads"));

        let chrome = ul.join("Application Support/Google/Chrome");
        mk(&chrome.join("Default/Code Cache"));
        mk(&chrome.join("Default/GPUCache"));
        mk(&chrome.join("Default/Local Storage"));
        mk(&chrome.join("Default/Extensions"));
        file(&chrome.join("Default/Cookies"));
        mk(&chrome.join("ShaderCache"));
        mk(&chrome.join("GPUPersistentCache/GPUCache")); // 缓存目录不是 profile
        mk(&ul.join("Caches/Google/Chrome/Default/Cache"));
        let firefox = ul.join("Application Support/Firefox/Profiles/abc.default");
        mk(&firefox.join("cache2"));
        file(&firefox.join("places.sqlite"));
        mk(&ul.join("Caches/Firefox/Profiles/abc.default/cache2"));

        mk(&ul.join("Logs/Foo"));
        mk(&ul.join("Logs/com.apple.xpc"));
        file(&ul.join("Logs/DiagnosticReports/Foo-2024-01-01-000000.ips"));

        mk(&ul.join("Developer/Xcode/DerivedData/Rind-abcdefghijklmnopqrstuvwxyz"));
        mk(&ul.join("Developer/CoreSimulator/Caches/dyld"));
        mk(&ul.join("Caches/com.apple.dt.Xcode"));
        mk(&ul.join("Developer/Xcode/Archives/2024-01-01/Rind.xcarchive"));
        mk(&ul.join("Developer/Xcode/iOS DeviceSupport/17.0 (21A329)"));

        mk_sized(&home.join(".npm/_cacache"), 64 * 1024);
        mk(&home.join(".cargo/registry/cache"));
        mk(&home.join("go/pkg/mod"));
        mk(&home.join(".m2/repository"));

        let projects = home.join("work");
        // 很久没动的 Rust 项目。
        file(&projects.join("old/Cargo.toml"));
        file(&projects.join("old/Cargo.lock"));
        file(&projects.join("old/src/main.rs"));
        mk(&projects.join("old/target/debug"));
        file(&projects.join("old/target/CACHEDIR.TAG"));
        age(&projects.join("old"), 90);
        // 没有 Cargo.toml 的 target：是用户自己的东西。
        mk(&projects.join("loose/target"));
        // 刚动过的 Node 项目；依赖里的 node_modules 不是项目。
        file(&projects.join("web/package.json"));
        file(&projects.join("web/package-lock.json"));
        file(&projects.join("web/index.js"));
        file(&projects.join("web/node_modules/.package-lock.json"));
        file(&projects.join("web/node_modules/left-pad/package.json"));
        mk(&projects.join("web/node_modules/left-pad/node_modules/inner"));
        // Python 虚拟环境要有 pyvenv.cfg 才算。
        file(&projects.join("py/main.py"));
        file(&projects.join("py/.venv/pyvenv.cfg"));
        mk(&projects.join("py/venv/lib")); // 没有 pyvenv.cfg

        Fixture {
            _dir: dir,
            env,
            apps,
            projects,
        }
    }

    /// 一个不属于任何 bundle 的进程：进程列表取到了，但没有相关的程序在运行。
    fn idle(f: &Fixture) -> PathBuf {
        f._dir.path().join("bin/idle")
    }

    fn opts(f: &Fixture) -> CleanupOptions {
        CleanupOptions {
            project_roots: vec![f.projects.clone()],
            running_executables: Some(vec![idle(f)]),
            ..CleanupOptions::default()
        }
    }

    fn run(f: &Fixture, opts: &CleanupOptions) -> CleanupReport {
        scan(&f.env, &f.apps, opts, &|_| {})
    }

    fn item<'a>(report: &'a CleanupReport, path: &Path) -> &'a CleanupItem {
        report
            .items
            .iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| panic!("{} is listed", path.display()))
    }

    fn listed(report: &CleanupReport, path: &Path) -> bool {
        report.items.iter().any(|i| i.path == path)
    }

    #[test]
    fn every_category_finds_its_places() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let ul = &f.env.user_library;
        let home = &f.env.home;
        let expect = [
            (ul.join("Caches/com.acme.foo"), CleanupCategory::AppCaches),
            (ul.join("Logs/Foo"), CleanupCategory::Logs),
            (
                ul.join("Application Support/Google/Chrome/Default/Code Cache"),
                CleanupCategory::BrowserCaches,
            ),
            (
                ul.join("Developer/Xcode/DerivedData/Rind-abcdefghijklmnopqrstuvwxyz"),
                CleanupCategory::Xcode,
            ),
            (home.join(".npm/_cacache"), CleanupCategory::PackageCaches),
            (f.projects.join("old/target"), CleanupCategory::DevArtifacts),
            (home.join("go/pkg/mod"), CleanupCategory::NeedsReview),
        ];
        for (path, category) in expect {
            assert_eq!(item(&report, &path).category, category, "{path:?}");
        }
        for category in CleanupCategory::ALL {
            assert!(
                !report.in_category(category).is_empty(),
                "{category:?} has something"
            );
        }
    }

    #[test]
    fn app_caches_are_named_and_skip_apple_and_other_categories() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let caches = f.env.user_library.join("Caches");

        let foo = item(&report, &caches.join("com.acme.foo"));
        assert_eq!(foo.label, "Foo", "named after the installed app");
        assert!(foo.suggested);
        assert_eq!(item(&report, &caches.join("SomeTool")).label, "SomeTool");

        assert!(!listed(&report, &caches.join("com.apple.Safari")));
        // 归到别的类别的整个父目录不当成 App 缓存。
        for name in ["Google", "Firefox"] {
            assert!(!listed(&report, &caches.join(name)), "{name}");
        }
        assert_eq!(
            item(&report, &caches.join("Homebrew")).category,
            CleanupCategory::PackageCaches
        );

        let cloudkit = item(&report, &caches.join("CloudKit"));
        assert_eq!(cloudkit.hold_back, Some(HoldBack::SystemOwned));
        assert!(!cloudkit.suggested && !cloudkit.selectable());
    }

    #[test]
    fn logs_list_each_report_not_the_report_folder() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let logs = f.env.user_library.join("Logs");
        assert!(!listed(&report, &logs.join("DiagnosticReports")));
        assert!(!listed(&report, &logs.join("com.apple.xpc")));
        let crash = item(
            &report,
            &logs.join("DiagnosticReports/Foo-2024-01-01-000000.ips"),
        );
        assert_eq!(crash.category, CleanupCategory::Logs);
        assert!(crash.suggested);
        assert_eq!(item(&report, &logs.join("Foo")).label, "Foo");
    }

    #[test]
    fn browser_profile_data_is_never_listed() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let ul = &f.env.user_library;
        let chrome = ul.join("Application Support/Google/Chrome");
        let firefox = ul.join("Application Support/Firefox/Profiles/abc.default");
        for kept in [
            chrome.join("Default"),
            chrome.join("Default/Local Storage"),
            chrome.join("Default/Extensions"),
            chrome.join("Default/Cookies"),
            firefox.clone(),
            firefox.join("places.sqlite"),
        ] {
            assert!(!listed(&report, &kept), "{kept:?} is profile data");
        }
        let browser = report.in_category(CleanupCategory::BrowserCaches);
        for i in &browser {
            let name = i.path.file_name().unwrap().to_string_lossy();
            assert!(
                CHROMIUM_CACHES.contains(&name.as_ref()) || FIREFOX_CACHES.contains(&name.as_ref()),
                "{name} is a cache folder"
            );
        }
        for path in [
            chrome.join("Default/Code Cache"),
            chrome.join("Default/GPUCache"),
            chrome.join("ShaderCache"),
            chrome.join("GPUPersistentCache"),
            ul.join("Caches/Google/Chrome/Default/Cache"),
            firefox.join("cache2"),
            ul.join("Caches/Firefox/Profiles/abc.default/cache2"),
        ] {
            assert!(item(&report, &path).suggested, "{path:?}");
        }
        assert!(!listed(
            &report,
            &chrome.join("GPUPersistentCache/GPUCache")
        ));
        assert_eq!(
            item(&report, &chrome.join("Default/Code Cache")).label,
            "Chrome / Default / Code Cache"
        );
    }

    #[test]
    fn the_xcode_cache_counts_even_with_an_apple_name() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let ul = &f.env.user_library;
        let xcode = item(&report, &ul.join("Caches/com.apple.dt.Xcode"));
        assert_eq!(xcode.category, CleanupCategory::Xcode);
        assert_eq!(xcode.hold_back, None);
        assert!(xcode.suggested);
        let derived = item(
            &report,
            &ul.join("Developer/Xcode/DerivedData/Rind-abcdefghijklmnopqrstuvwxyz"),
        );
        assert_eq!(derived.label, "DerivedData / Rind");
        assert!(item(&report, &ul.join("Developer/CoreSimulator/Caches")).suggested);
    }

    #[test]
    fn review_items_are_never_suggested() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let ul = &f.env.user_library;
        for path in [
            f.env.home.join("go/pkg/mod"),
            f.env.home.join(".m2/repository"),
            ul.join("Developer/Xcode/Archives/2024-01-01"),
            ul.join("Developer/Xcode/iOS DeviceSupport/17.0 (21A329)"),
        ] {
            let i = item(&report, &path);
            assert_eq!(i.category, CleanupCategory::NeedsReview, "{path:?}");
            assert!(!i.suggested && i.selectable(), "{path:?}");
        }
        assert!(item(&report, &f.env.home.join(".cargo/registry/cache")).suggested);
    }

    #[test]
    fn dev_artifacts_need_evidence_and_age() {
        let f = fixture();
        let report = run(&f, &opts(&f));
        let p = &f.projects;

        let old = item(&report, &p.join("old/target"));
        assert_eq!(old.label, "old / target");
        assert!(old.suggested, "untouched for 90 days");
        let age = Utc::now() - old.modified.expect("project activity is known");
        assert!(age.num_days() >= 89, "uses the project's files, not target");

        assert!(!listed(&report, &p.join("loose/target")), "no Cargo.toml");
        assert!(!listed(&report, &p.join("py/venv")), "no pyvenv.cfg");
        assert!(!listed(
            &report,
            &p.join("web/node_modules/left-pad/node_modules")
        ));

        let web = item(&report, &p.join("web/node_modules"));
        assert_eq!(web.category, CleanupCategory::DevArtifacts);
        assert!(!web.suggested, "touched today");
        assert!(web.selectable());
        assert!(listed(&report, &p.join("py/.venv")));

        // 门槛放到 0 天，新项目也建议。
        let eager = CleanupOptions {
            artifact_min_age_days: 0,
            ..opts(&f)
        };
        assert!(item(&run(&f, &eager), &p.join("web/node_modules")).suggested);
    }

    #[test]
    fn artifacts_must_look_like_a_project_to_be_suggested() {
        let f = fixture();
        let p = &f.projects;
        // 只有 package.json：依赖装在哪都有，不算。
        file(&p.join("bare/package.json"));
        mk(&p.join("bare/node_modules/x"));
        // 有 .git 也算项目。
        file(&p.join("repo/package.json"));
        mk(&p.join("repo/.git"));
        mk(&p.join("repo/node_modules/.pnpm/x"));
        // 没有 Cargo.lock 也没有 .git 的 target：列出来，但不建议。
        file(&p.join("nolock/Cargo.toml"));
        mk(&p.join("nolock/target/debug"));
        // 工具装扩展的地方：有锁文件也不建议。
        let coc = p.join("dotfiles/config/coc/extensions");
        file(&coc.join("package.json"));
        file(&coc.join("package-lock.json"));
        file(&coc.join("node_modules/.package-lock.json"));
        mk(&coc.join("node_modules/coc-json"));
        for dir in ["repo", "nolock", "dotfiles"] {
            age(&p.join(dir), 90);
        }
        let report = run(&f, &opts(&f));

        assert!(!listed(&report, &p.join("bare/node_modules")));
        assert!(item(&report, &p.join("repo/node_modules")).suggested);
        let nolock = item(&report, &p.join("nolock/target"));
        assert!(!nolock.suggested && nolock.selectable());
        let coc = item(&report, &coc.join("node_modules"));
        assert_eq!(coc.category, CleanupCategory::DevArtifacts);
        assert!(!coc.suggested && coc.selectable());
    }

    #[test]
    fn costly_app_caches_need_review() {
        let f = fixture();
        let caches = f.env.user_library.join("Caches");
        mk(&caches.join("JetBrains/IntelliJIdea2024.1"));
        mk(&caches.join("jdtls"));
        let report = run(&f, &opts(&f));
        for name in ["JetBrains", "jdtls"] {
            let i = item(&report, &caches.join(name));
            assert_eq!(i.category, CleanupCategory::NeedsReview, "{name}");
            assert!(!i.suggested && i.selectable(), "{name}");
        }

        // 按类别过滤时各归各的。
        let only = |c| CleanupOptions {
            categories: vec![c],
            ..opts(&f)
        };
        let apps = run(&f, &only(CleanupCategory::AppCaches));
        assert!(!listed(&apps, &caches.join("JetBrains")));
        assert!(listed(&apps, &caches.join("com.acme.foo")));
        let review = run(&f, &only(CleanupCategory::NeedsReview));
        assert!(listed(&review, &caches.join("JetBrains")));
        assert!(!listed(&review, &caches.join("com.acme.foo")));
    }

    #[test]
    fn playwright_downloads_need_review() {
        let f = fixture();
        let caches = f.env.user_library.join("Caches");
        for name in ["ms-playwright", "ms-playwright-go", "ms-playwright-mcp"] {
            mk(&caches.join(name));
        }
        let report = run(&f, &opts(&f));
        for name in ["ms-playwright", "ms-playwright-go", "ms-playwright-mcp"] {
            let i = item(&report, &caches.join(name));
            assert_eq!(i.category, CleanupCategory::NeedsReview, "{name}");
            assert!(!i.suggested, "{name}");
        }
    }

    #[test]
    fn update_staging_is_suggested_only_when_its_app_is_quit() {
        let f = fixture();
        let caches = f.env.user_library.join("Caches");
        let shipit = caches.join("com.acme.foo.ShipIt");
        let updater = caches.join("foo-updater");
        let stranger = caches.join("mystery-updater");
        for dir in [&shipit, &updater, &stranger] {
            mk(dir);
        }

        let quit = run(&f, &opts(&f));
        for dir in [&shipit, &updater] {
            let i = item(&quit, dir);
            assert_eq!(i.category, CleanupCategory::AppCaches);
            assert_eq!(i.label, "Foo");
            assert!(i.suggested, "{dir:?}: Foo is not running");
        }
        // 认不出主人：说不清在不在更新，不建议。
        let i = item(&quit, &stranger);
        assert_eq!(i.category, CleanupCategory::AppCaches);
        assert!(!i.suggested && i.selectable());

        let foo = f.apps[0].executable.clone().unwrap();
        let running = CleanupOptions {
            running_executables: Some(vec![foo]),
            ..opts(&f)
        };
        let report = run(&f, &running);
        for dir in [&shipit, &updater] {
            let i = item(&report, dir);
            assert_eq!(i.hold_back, Some(HoldBack::InUse { app: "Foo".into() }));
            assert!(!i.suggested && !i.selectable());
        }
        assert_eq!(update_stem("FooUpdater"), "Foo");
        assert_eq!(update_stem("com.foo.bar.ShipIt"), "com.foo.bar");
        assert_eq!(update_stem("plain"), "plain");
    }

    #[test]
    fn apple_logs_without_an_apple_name_are_skipped() {
        let f = fixture();
        let logs = f.env.user_library.join("Logs");
        mk(&logs.join("Baseband"));
        mk(&logs.join("SiriTTSService"));
        file(&logs.join("PhotosSearch.aapbz"));
        file(&logs.join("fsck_apfs.log"));
        let report = run(&f, &opts(&f));
        for name in [
            "Baseband",
            "SiriTTSService",
            "PhotosSearch.aapbz",
            "fsck_apfs.log",
        ] {
            assert!(!listed(&report, &logs.join(name)), "{name}");
        }
        assert!(listed(&report, &logs.join("Foo")));
    }

    #[test]
    fn empty_items_are_not_listed() {
        let f = fixture();
        let empty = f.env.user_library.join("Logs/empty.log");
        std::fs::write(&empty, b"").unwrap();
        let report = run(&f, &opts(&f));
        assert!(!listed(&report, &empty), "0 B is noise");
        // 没量就是未知，不是 0：照列。
        let unmeasured = CleanupOptions {
            measure: false,
            ..opts(&f)
        };
        assert!(listed(&run(&f, &unmeasured), &empty));
    }

    #[test]
    fn only_eperm_counts_as_a_privacy_block() {
        use std::io::Error;
        assert!(is_eperm(Some(Error::from_raw_os_error(libc::EPERM))));
        assert!(!is_eperm(Some(Error::from_raw_os_error(libc::EACCES))));
        assert!(!is_eperm(None));

        // SAFETY: geteuid 总是成功。
        let me = unsafe { libc::geteuid() };
        if me == 0 {
            return; // root 什么都读得了
        }
        // 普通的权限不够（EACCES）不是隐私保护。
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        mk(&locked.join("cache"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = privacy_blocked(&locked.join("cache"), me);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!blocked);
        assert!(!privacy_blocked(&dir.path().join("plain"), me));
    }

    #[test]
    fn caches_in_a_wallet_profile_are_protected() {
        let f = fixture();
        let profile = f
            .env
            .user_library
            .join("Application Support/Google/Chrome/Default");
        mk(&profile.join("Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn"));
        let report = run(&f, &opts(&f));
        let cache = item(&report, &profile.join("Code Cache"));
        assert_eq!(
            cache.hold_back,
            Some(HoldBack::Protected {
                why: ProtectReason::Wallet
            })
        );
        assert!(!cache.suggested && !cache.selectable());
        assert!(!listed(&report, &profile));
    }

    #[test]
    fn unreadable_caches_are_held_back() {
        // SAFETY: geteuid 总是成功。
        if unsafe { libc::geteuid() } == 0 {
            return; // root 什么都读得了
        }
        let f = fixture();
        let locked = f.env.user_library.join("Caches/SomeTool");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let report = run(&f, &opts(&f));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let i = item(&report, &locked);
        assert_eq!(i.hold_back, Some(HoldBack::Unreadable));
        assert_eq!(i.size, Size::Unknown);
        assert!(!i.suggested && i.selectable());
    }

    #[test]
    fn running_owners_hold_their_caches_back() {
        let f = fixture();
        let exes = f
            .apps
            .iter()
            .map(|a| a.executable.clone().unwrap())
            .collect();
        let opts = CleanupOptions {
            running_executables: Some(exes),
            ..opts(&f)
        };
        let report = run(&f, &opts);
        let ul = &f.env.user_library;
        let in_use = |app: &str| Some(HoldBack::InUse { app: app.into() });

        let chrome = item(
            &report,
            &ul.join("Application Support/Google/Chrome/Default/Code Cache"),
        );
        assert_eq!(chrome.hold_back, in_use("Google Chrome"));
        assert!(!chrome.suggested && !chrome.selectable());
        assert_eq!(
            item(&report, &ul.join("Caches/com.acme.foo")).hold_back,
            in_use("Foo")
        );
        // Firefox 没在跑；包管理器缓存不看运行状态。
        assert!(
            item(
                &report,
                &ul.join("Caches/Firefox/Profiles/abc.default/cache2")
            )
            .suggested
        );
        assert!(item(&report, &f.env.home.join(".npm/_cacache")).suggested);
    }

    #[test]
    fn a_browser_found_only_by_its_bundle_name_still_counts() {
        let f = fixture();
        let elsewhere = f
            ._dir
            .path()
            .join("Elsewhere/Firefox.app/Contents/MacOS/firefox");
        file(&elsewhere);
        let opts = CleanupOptions {
            running_executables: Some(vec![elsewhere]),
            ..opts(&f)
        };
        let report = run(&f, &opts);
        let cache = item(
            &report,
            &f.env
                .user_library
                .join("Caches/Firefox/Profiles/abc.default/cache2"),
        );
        assert_eq!(
            cache.hold_back,
            Some(HoldBack::InUse {
                app: "Firefox".into()
            })
        );
    }

    #[test]
    fn each_path_appears_once() {
        let f = fixture();
        // 重叠的项目根：同一个 target 只出现一次。
        let opts = CleanupOptions {
            project_roots: vec![
                f.projects.clone(),
                f.projects.join("old"),
                f.projects.clone(),
            ],
            ..opts(&f)
        };
        let report = run(&f, &opts);
        let mut paths: Vec<&PathBuf> = report.items.iter().map(|i| &i.path).collect();
        let total = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), total, "no path twice");
        assert_eq!(
            report
                .items
                .iter()
                .filter(|i| i.path == f.projects.join("old/target"))
                .count(),
            1
        );

        // 父子都命中：留里面那条。
        let found = |p: &str, c| Found::new(PathBuf::from(p), c, p.to_string());
        let kept = dedup(vec![
            found("/a/b", CleanupCategory::AppCaches),
            found("/a/b/c", CleanupCategory::BrowserCaches),
            found("/a/bc", CleanupCategory::AppCaches),
            found("/a/b/c", CleanupCategory::Xcode),
        ]);
        let kept: Vec<(&str, CleanupCategory)> = kept
            .iter()
            .map(|f| (f.path.to_str().unwrap(), f.category))
            .collect();
        assert_eq!(
            kept,
            vec![
                ("/a/b/c", CleanupCategory::BrowserCaches),
                ("/a/bc", CleanupCategory::AppCaches),
            ]
        );
    }

    #[test]
    fn a_cancelled_scan_returns_nothing() {
        let f = fixture();
        let opts = CleanupOptions {
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..opts(&f)
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let report = scan(&f.env, &f.apps, &opts, &|p| seen.lock().unwrap().push(p));
        assert!(report.items.is_empty());
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|p| matches!(p, ScanProgress::Finished)));
    }

    #[test]
    fn only_the_asked_categories_are_scanned() {
        let f = fixture();
        let opts = CleanupOptions {
            categories: vec![CleanupCategory::PackageCaches],
            ..opts(&f)
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let report = scan(&f.env, &f.apps, &opts, &|p| seen.lock().unwrap().push(p));
        assert!(!report.items.is_empty());
        assert!(report
            .items
            .iter()
            .all(|i| i.category == CleanupCategory::PackageCaches));
        assert!(listed(&report, &f.env.user_library.join("Caches/Homebrew")));
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|p| matches!(p, ScanProgress::Location(l) if l == "~/Library/Caches")));
    }

    #[test]
    fn progress_and_order() {
        let f = fixture();
        let seen = std::sync::Mutex::new(Vec::new());
        let report = scan(&f.env, &f.apps, &opts(&f), &|p| {
            seen.lock().unwrap().push(p)
        });
        let seen = seen.lock().unwrap();
        assert!(seen
            .iter()
            .any(|p| matches!(p, ScanProgress::Location(l) if l == "~/Library/Caches")));
        assert!(seen
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { .. })));
        assert_eq!(seen.last(), Some(&ScanProgress::Finished));

        for pair in report.items.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(a.category <= b.category, "grouped by category");
            if a.category == b.category {
                assert!(a.size.known() >= b.size.known(), "biggest first");
            }
        }
        let packages = report.in_category(CleanupCategory::PackageCaches);
        assert_eq!(packages[0].path, f.env.home.join(".npm/_cacache"));
    }

    #[test]
    fn unmeasured_items_are_never_suggested() {
        let f = fixture();
        let opts = CleanupOptions {
            measure: false,
            ..opts(&f)
        };
        let report = run(&f, &opts);
        assert!(!report.items.is_empty());
        assert!(report.items.iter().all(|i| i.size == Size::Unknown));
        assert!(report.suggested().is_empty());
    }

    fn link(target: &Path, at: &Path) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    /// 把 `at` 挪开，换成指向 `target` 的符号链接。
    fn swap_for_link(target: &Path, at: &Path) {
        if at.exists() {
            let mut aside = at.as_os_str().to_owned();
            aside.push(".bak");
            std::fs::rename(at, aside).unwrap();
        }
        link(target, at);
    }

    #[test]
    fn caches_swapped_for_a_link_to_containers_lists_nothing() {
        let f = fixture();
        let ul = &f.env.user_library;
        let wechat = ul.join("Containers/com.tencent.xinWeChat");
        mk(&wechat.join("Data/Documents"));
        mk(&ul.join("Containers/com.acme.foo"));
        swap_for_link(&ul.join("Containers"), &ul.join("Caches"));
        let report = run(&f, &opts(&f));
        for i in &report.items {
            assert!(
                !i.path.starts_with(ul.join("Caches"))
                    && !i.path.starts_with(ul.join("Containers")),
                "{:?} is behind a link",
                i.path
            );
        }
        // 别的根照常。
        assert!(listed(&report, &ul.join("Logs/Foo")));
    }

    #[test]
    fn logs_swapped_for_a_link_to_documents_lists_nothing() {
        let f = fixture();
        let docs = f.env.home.join("Documents");
        mk(&docs.join("Thesis"));
        file(&docs.join("DiagnosticReports/secret.ips"));
        swap_for_link(&docs, &f.env.user_library.join("Logs"));
        let report = run(&f, &opts(&f));
        assert!(report.in_category(CleanupCategory::Logs).is_empty());
        assert!(!report.items.iter().any(|i| i.path.starts_with(&docs)));
    }

    #[test]
    fn linked_derived_data_browser_roots_and_children_are_skipped() {
        let f = fixture();
        let ul = &f.env.user_library;
        let elsewhere = f.env.home.join("Elsewhere");
        mk(&elsewhere.join("Proj-abcdefghijklmnopqrstuvwxyz"));
        mk(&elsewhere.join("Default/Code Cache"));
        mk(&elsewhere.join("Code Cache"));
        swap_for_link(&elsewhere, &ul.join("Developer/Xcode/DerivedData"));
        swap_for_link(&elsewhere, &ul.join("Application Support/Google/Chrome"));
        // 根下的子项是链接：不列、不进入。
        link(&elsewhere, &ul.join("Caches/com.acme.linked"));
        link(
            &elsewhere.join("Default"),
            &ul.join("Application Support/Firefox/Profiles/linked"),
        );
        link(&elsewhere, &ul.join("Caches/Google/Chrome/Profile 9"));
        // 祖先是链接：整个根跳过。
        let review = f.env.home.join("Review");
        mk(&review.join("Xcode/Archives/old"));
        swap_for_link(&review, &ul.join("Developer"));
        let report = run(&f, &opts(&f));

        for i in &report.items {
            assert!(!i.path.starts_with(&elsewhere) && !i.path.starts_with(&review));
            assert!(
                !i.path.starts_with(ul.join("Developer")),
                "{:?} is under a linked ancestor",
                i.path
            );
            assert!(
                !i.path
                    .starts_with(ul.join("Application Support/Google/Chrome")),
                "{:?} is under a linked browser root",
                i.path
            );
            assert!(
                !i.path.ends_with("linked")
                    && !i
                        .path
                        .starts_with(ul.join("Caches/Google/Chrome/Profile 9"))
            );
        }
        assert!(report
            .in_category(CleanupCategory::Xcode)
            .iter()
            .all(|i| i.path.starts_with(ul.join("Caches/com.apple.dt.Xcode"))));
        // 没被换掉的照常。
        assert!(listed(
            &report,
            &ul.join("Caches/Google/Chrome/Default/Cache")
        ));
    }

    #[test]
    fn linked_fixed_places_and_project_roots_are_skipped() {
        let f = fixture();
        let home = &f.env.home;
        let secret = home.join("Secret");
        mk(&secret.join("payload"));
        mk(&secret.join("registry/cache"));
        std::fs::remove_dir_all(home.join(".npm/_cacache")).unwrap();
        link(&secret, &home.join(".npm/_cacache"));
        // `.cargo` 这一级是链接。
        swap_for_link(&secret, &home.join(".cargo"));
        file(&secret.join("proj/Cargo.toml"));
        file(&secret.join("proj/Cargo.lock"));
        mk(&secret.join("proj/target/debug"));
        link(&secret, &home.join("linked-projects"));
        link(&secret.join("proj"), &f.projects.join("linked-proj"));
        let opts = CleanupOptions {
            project_roots: vec![f.projects.clone(), home.join("linked-projects")],
            artifact_min_age_days: 0,
            ..opts(&f)
        };
        let report = run(&f, &opts);
        assert!(!listed(&report, &home.join(".npm/_cacache")));
        assert!(!listed(&report, &home.join(".cargo/registry/cache")));
        assert!(!report.items.iter().any(|i| i.path.starts_with(&secret)
            || i.path.starts_with(home.join("linked-projects"))
            || i.path.starts_with(f.projects.join("linked-proj"))));
    }

    #[test]
    fn paths_use_the_real_case_on_disk() {
        let f = fixture();
        let upper = f.env.home.join("WORK");
        if !upper.exists() {
            return; // 区分大小写的卷：没有这个问题
        }
        let opts = CleanupOptions {
            project_roots: vec![upper],
            ..opts(&f)
        };
        let report = run(&f, &opts);
        assert!(listed(&report, &f.projects.join("old/target")));
        assert!(!report
            .items
            .iter()
            .any(|i| i.path.to_string_lossy().contains("/WORK/")));
        assert!(default_project_roots(&f.env).contains(&f.projects));
    }

    #[test]
    fn artifacts_need_their_own_marks() {
        let f = fixture();
        let p = &f.projects;
        // 父目录有 Cargo.toml，但 target 是用户自己的东西。
        file(&p.join("rs/Cargo.toml"));
        file(&p.join("rs/Cargo.lock"));
        mk(&p.join("rs/target/notes"));
        // node_modules 只有 `.bin`（bun、老 npm）：列出不建议；什么都没有：不列。
        file(&p.join("bun/package.json"));
        file(&p.join("bun/bun.lock"));
        mk(&p.join("bun/node_modules/.bin"));
        file(&p.join("npm/package.json"));
        file(&p.join("npm/package-lock.json"));
        mk(&p.join("npm/node_modules/mine"));
        // Pods 要有 Manifest.lock，.next 要有 BUILD_ID 或 cache/，gradle 的 build 要有 tmp/ 之类。
        file(&p.join("ios/Podfile"));
        mk(&p.join("ios/Pods/Alamofire"));
        file(&p.join("ios2/Podfile"));
        file(&p.join("ios2/Pods/Manifest.lock"));
        file(&p.join("next/package.json"));
        file(&p.join("next/.next/BUILD_ID"));
        file(&p.join("next2/package.json"));
        mk(&p.join("next2/.next/stuff"));
        file(&p.join("gr/build.gradle"));
        mk(&p.join("gr/build/tmp"));
        file(&p.join("gr2/build.gradle"));
        mk(&p.join("gr2/build/docs"));
        for dir in [
            "rs", "bun", "npm", "ios", "ios2", "next", "next2", "gr", "gr2",
        ] {
            age(&p.join(dir), 90);
        }
        let report = run(&f, &opts(&f));

        for gone in [
            "rs/target",
            "npm/node_modules",
            "ios/Pods",
            "next2/.next",
            "gr2/build",
        ] {
            assert!(!listed(&report, &p.join(gone)), "{gone}");
        }
        let bun = item(&report, &p.join("bun/node_modules"));
        assert!(!bun.suggested && bun.selectable());
        for ok in ["ios2/Pods", "next/.next", "gr/build"] {
            assert!(item(&report, &p.join(ok)).suggested, "{ok}");
        }
    }

    #[test]
    fn unconfirmed_build_folders_count_as_project_activity() {
        let f = fixture();
        let old = f.projects.join("old");
        // 源码里叫 build 的普通目录（没有构建特征），刚改过：项目是活跃的。
        mk(&old.join("src/build"));
        let report = run(&f, &opts(&f));
        let target = item(&report, &old.join("target"));
        assert!(!target.suggested, "src/build was just edited");
        // 确认过的产物不算活跃度：只有 target 刚动过，项目仍然是旧的。
        let f = fixture();
        mk(&f.projects.join("old/target/debug/fresh"));
        assert!(item(&run(&f, &opts(&f)), &f.projects.join("old/target")).suggested);
    }

    #[test]
    fn a_venv_needs_a_dependency_manifest_to_be_suggested() {
        let f = fixture();
        let p = &f.projects;
        for (dir, manifest) in [
            ("req", Some("requirements-dev.txt")),
            ("pp", Some("pyproject.toml")),
            ("uvp", Some("uv.lock")),
            ("none", None),
        ] {
            file(&p.join(dir).join("main.py"));
            file(&p.join(dir).join(".venv/pyvenv.cfg"));
            if let Some(m) = manifest {
                file(&p.join(dir).join(m));
            }
            age(&p.join(dir), 90);
        }
        let report = run(&f, &opts(&f));
        for dir in ["req", "pp", "uvp"] {
            assert!(item(&report, &p.join(dir).join(".venv")).suggested, "{dir}");
        }
        let none = item(&report, &p.join("none/.venv"));
        assert!(!none.suggested && none.selectable());
    }

    /// 装在 `/Library/Input Methods` 的输入法，不在已安装 App 里；它的服务进程在跑。
    fn sogou(f: &Fixture) -> PathBuf {
        let methods = f._dir.path().join("Library/Input Methods");
        let app = make_app(&methods, "SogouInput", "com.sogou.inputmethod.sogou");
        make_app(
            &app.join("Contents"),
            "SogouTaskManager",
            "com.sogou.SogouTaskManager",
        );
        let services = app.join("Contents/SogouServices");
        file(&services);
        services
    }

    #[test]
    fn a_cache_named_after_a_running_helper_is_left_alone() {
        let f = fixture();
        let caches = f.env.user_library.join("Caches");
        let helper_cache = caches.join("cc-agentd");
        let app_cache = caches.join("huhu");
        let unrelated = caches.join("some-quiet-tool");
        mk(&helper_cache);
        mk(&app_cache);
        mk(&unrelated);
        let bin = f._dir.path().join("bin");
        let helper = bin.join("cc-agentd");
        let app_exe = f._dir.path().join("Huhu.app/Contents/MacOS/Huhu");
        let opts = CleanupOptions {
            running_executables: Some(vec![idle(&f), helper, app_exe]),
            ..opts(&f)
        };
        let report = run(&f, &opts);
        let in_use = |app: &str| Some(HoldBack::InUse { app: app.into() });
        for (path, app) in [(&helper_cache, "cc-agentd"), (&app_cache, "Huhu")] {
            let i = item(&report, path);
            assert_eq!(i.hold_back, in_use(app), "{path:?}");
            assert!(!i.suggested && !i.selectable(), "{path:?}");
        }
        assert!(item(&report, &unrelated).suggested);
    }

    #[test]
    fn running_bundles_outside_the_installed_apps_hold_caches_back() {
        let f = fixture();
        let caches = f.env.user_library.join("Caches");
        let task = caches.join("com.sogou.SogouTaskManager");
        let sub = caches.join("com.sogou.inputmethod.sogou.cloud");
        mk(&task);
        mk(&sub);
        mk(&caches.join("com.sogou.other"));
        let running = CleanupOptions {
            running_executables: Some(vec![idle(&f), sogou(&f)]),
            ..opts(&f)
        };
        let report = run(&f, &running);
        for dir in [&task, &sub] {
            let i = item(&report, dir);
            assert_eq!(
                i.hold_back,
                Some(HoldBack::InUse {
                    app: "SogouInput".into()
                }),
                "{dir:?}"
            );
            assert!(!i.suggested && !i.selectable());
        }
        assert!(item(&report, &caches.join("com.sogou.other")).suggested);
        // 输入法没在跑：照常建议。
        let quit = run(&f, &opts(&f));
        assert!(item(&quit, &task).suggested);
    }

    #[test]
    fn huge_info_plists_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let app = make_app(dir.path(), "Big", "com.acme.big");
        let plist = app.join("Contents/Info.plist");
        let mut text = std::fs::read_to_string(&plist).unwrap();
        text = text.replace(
            "<dict>",
            &format!("<dict><!--{}-->", "x".repeat(1024 * 1024)),
        );
        std::fs::write(&plist, text).unwrap();
        let exe = app.join("Contents/MacOS/Big");
        assert!(running_bundle_ids(&[exe]).is_empty());
        let small = make_app(dir.path(), "Small", "com.acme.small");
        let ids = running_bundle_ids(&[small.join("Contents/MacOS/Small")]);
        assert_eq!(ids.get("com.acme.small").map(String::as_str), Some("Small"));
    }

    #[test]
    fn an_unknown_process_list_fails_closed() {
        let f = fixture();
        let opts = CleanupOptions {
            running_executables: Some(Vec::new()),
            ..opts(&f)
        };
        let report = run(&f, &opts);
        for category in [
            CleanupCategory::AppCaches,
            CleanupCategory::BrowserCaches,
            CleanupCategory::Xcode,
        ] {
            let items = report.in_category(category);
            assert!(!items.is_empty(), "{category:?} still listed");
            assert!(items.iter().all(|i| !i.suggested), "{category:?}");
        }
        // 不看运行状态的类别不受影响。
        assert!(item(&report, &f.env.home.join(".npm/_cacache")).suggested);
        assert!(item(&report, &f.projects.join("old/target")).suggested);
    }

    #[test]
    fn recheck_refreshes_in_use_before_moving() {
        let f = fixture();
        let ul = &f.env.user_library;
        let foo = ul.join("Caches/com.acme.foo");
        let chrome = ul.join("Application Support/Google/Chrome/Default/Code Cache");
        let task = ul.join("Caches/com.sogou.SogouTaskManager");
        mk(&task);
        let mut report = run(&f, &opts(&f));
        for path in [&foo, &chrome, &task] {
            assert!(item(&report, path).suggested, "{path:?}");
        }

        // 扫完之后 Foo、Chrome、输入法都开了。
        let exes = vec![
            idle(&f),
            f.apps[0].executable.clone().unwrap(),
            f.apps[1].executable.clone().unwrap(),
            sogou(&f),
        ];
        recheck_in_use(&f.env, &mut report, &f.apps, Some(exes));
        let in_use = |app: &str| Some(HoldBack::InUse { app: app.into() });
        assert_eq!(item(&report, &foo).hold_back, in_use("Foo"));
        assert_eq!(item(&report, &chrome).hold_back, in_use("Google Chrome"));
        assert_eq!(item(&report, &task).hold_back, in_use("SogouInput"));
        for path in [&foo, &chrome, &task] {
            let i = item(&report, path);
            assert!(!i.suggested && !i.selectable(), "{path:?}");
        }
        assert!(item(&report, &f.env.home.join(".npm/_cacache")).suggested);

        // 又都退出了：能选了，但不会替用户重新勾上。
        recheck_in_use(&f.env, &mut report, &f.apps, Some(vec![idle(&f)]));
        let i = item(&report, &foo);
        assert_eq!(i.hold_back, None);
        assert!(i.selectable() && !i.suggested);

        // 进程列表取不到：这几类一律不建议。
        let mut fresh = run(&f, &opts(&f));
        recheck_in_use(&f.env, &mut fresh, &f.apps, Some(Vec::new()));
        assert!(!item(&fresh, &ul.join("Caches/com.apple.dt.Xcode")).suggested);
        assert!(!item(&fresh, &ul.join("Caches/Google/Chrome/Default/Cache")).suggested);
        assert!(item(&fresh, &f.env.home.join(".npm/_cacache")).suggested);
    }

    #[test]
    fn retired_diagnostic_reports_are_left_to_macos() {
        let f = fixture();
        let reports = f.env.user_library.join("Logs/DiagnosticReports");
        file(&reports.join("Retired/Old-2023.ips"));
        mk(&reports.join("ProxiedDevice-1234"));
        let report = run(&f, &opts(&f));
        assert!(!report
            .items
            .iter()
            .any(|i| i.path.starts_with(reports.join("Retired"))
                || i.path.starts_with(reports.join("ProxiedDevice-1234"))));
        assert!(listed(&report, &reports.join("Foo-2024-01-01-000000.ips")));
    }

    #[test]
    fn a_scan_cancelled_while_measuring_returns_nothing() {
        let f = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let opts = CleanupOptions {
            cancel: Some(cancel.clone()),
            ..opts(&f)
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let report = scan(&f.env, &f.apps, &opts, &|p| {
            if matches!(p, ScanProgress::Measuring { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
            seen.lock().unwrap().push(p);
        });
        assert!(report.items.is_empty());
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|p| matches!(p, ScanProgress::Location(_))),
            "got past discovery"
        );
        assert!(seen
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { .. })));
        assert!(!seen.iter().any(|p| matches!(p, ScanProgress::Finished)));
    }

    #[test]
    fn derived_data_names_lose_their_hash() {
        assert_eq!(
            derived_data_project("Rind-abcdefghijklmnopqrstuvwxyz"),
            "Rind"
        );
        assert_eq!(
            derived_data_project("ModuleCache.noindex"),
            "ModuleCache.noindex"
        );
        assert_eq!(derived_data_project("my-app"), "my-app");
    }
}
