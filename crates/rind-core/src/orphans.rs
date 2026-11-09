//! 孤儿残留：App 已经删了，它的数据还在。思路见 docs/ARCHITECTURE.md「孤儿」一节。
//!
//! 契约（GUI / CLI 依赖它，改动要同步）：
//! - `scan` 在 `leftovers::search_locations` 的同一批位置里找候选；任何已安装 App（含 Apple 自带、
//!   以及 Spotlight 能找到的任何位置的 App 包）用 `matcher::Claimant` 能拿到证据的都不算孤儿。
//! - 剩下的按推测的主人分组（`OrphanGroup`），每项是一个 `LeftoverMatch`，证据是
//!   `EvidenceKind::Orphan { signal }`，置信度按 `OrphanSignal` 和旁证定。
//! - `com.apple.*`、系统目录、保护区一律不报；`hold_back` 规则与卸载一致。
//! - 建议勾选沿用 `LeftoverMatch::suggested()`。
//! - 输出的 `path` 一律是盘上的真实写法（大小写以目录里列出的为准），同一个东西只出现一次。
//! - 搜索位置自己或它上面某一级是符号链接的，整个位置跳过，不跟随。
//! - 要问 Spotlight 却没问成（失败、超时、Spotlight 关着）时 `OrphanReport::degraded` 为 true，
//!   所有孤儿封顶为 possible，不替用户勾。
//!
//! 误报比漏报糟糕得多：用户会把"孤儿"整组移走。所以只认几种有实据的信号——名字是 bundle id 而盘上
//! 没人用它、launchd 任务要跑的程序没了、规则库 / cask 点名的 App 没装；普通名字的陌生文件夹一律不报。
//! 主人的范围也往宽里算：Apple 的 App、Spotlight 找得到的 App、输入法 / 偏好设置面板 / JDK 这类
//! 带 bundle id 的非 App 包、还活着的 launchd 任务，都算主人。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::leftovers::{self, Candidate, SearchLocation};
use crate::matcher::{candidate_stem, normalize_name, Claimant, HELPER_SUFFIXES};
use crate::remove::Removable;
use crate::rules::{CaskIndex, Rule, RuleSet};
use crate::{
    AppSource, Confidence, Env, Evidence, EvidenceKind, HoldBack, InstalledApp, LeftoverKind,
    LeftoverMatch, OrphanSignal, ScanProgress, Size,
};

#[derive(Clone, Debug)]
pub struct OrphanOptions {
    pub measure: bool,
    pub size_budget: Duration,
    /// 置为 true 就尽快停下；停下的结果不完整，调用方应丢弃。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 问不问 Spotlight 别处还有哪些 App（`~/Downloads`、外接盘、别的 App 包里……）。测试里关掉。
    pub spotlight: bool,
    /// 测试用：代替真的去问 Spotlight，`Some(None)` 是"没问成"。
    #[cfg(test)]
    pub(crate) fake_spotlight: Option<Option<Vec<PathBuf>>>,
}

impl Default for OrphanOptions {
    fn default() -> Self {
        OrphanOptions {
            measure: true,
            size_budget: Duration::from_secs(20),
            cancel: None,
            spotlight: true,
            #[cfg(test)]
            fake_spotlight: None,
        }
    }
}

/// 推测属于同一个已删除 App 的一组孤儿。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanGroup {
    /// 分组键：推测的 bundle id（如 `com.foo.bar`），没有就是归一化后的名字。
    pub key: String,
    /// 给人看的名字：规则库/cask 里的 App 名，或从 bundle id 推出来的（`bar`）。
    pub display_name: String,
    pub items: Vec<LeftoverMatch>,
}

impl OrphanGroup {
    /// 已知大小之和。
    pub fn known_size(&self) -> u64 {
        self.items.iter().filter_map(|m| m.size.known()).sum()
    }

    /// 这一组里最大的把握。
    pub fn confidence(&self) -> Option<crate::Confidence> {
        self.items.iter().map(|m| m.evidence.confidence).max()
    }
}

/// 全部孤儿，按 `known_size` 从大到小。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanReport {
    pub groups: Vec<OrphanGroup>,
    /// 要问 Spotlight 却没问成：别处的 App 可能没算进主人，所有孤儿都只到 possible。界面应该提示一句。
    #[serde(default)]
    pub degraded: bool,
}

impl OrphanReport {
    /// 给 `remove::trash_items` 的候选清单。
    pub fn removables(&self) -> Vec<Removable> {
        self.groups
            .iter()
            .flat_map(|g| g.items.iter())
            .map(|m| Removable {
                path: m.path.clone(),
                size: m.size,
                hold_back: m.hold_back.clone(),
            })
            .collect()
    }

    /// Rind 建议勾选的路径。
    pub fn suggested(&self) -> Vec<PathBuf> {
        self.groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|m| m.suggested())
            .map(|m| m.path.clone())
            .collect()
    }
}

/// 主人不是 App 包、或者谁都能用的 id（小写前缀）：一律不报。
const NEVER_ORPHANS: &[&str] = &[
    "is.workflow.",         // 快捷指令（Apple 收购前的 id，系统还在用）
    "org.cups.",            // 打印系统
    "com.vix.",             // cron
    "com.openssh.",         // 系统自带的 ssh-agent
    "org.openbsd.",         // 同上，旧名字
    "org.swift.",           // Xcode / 命令行工具链
    "org.llvm.",            // 同上
    "org.sparkle-project.", // Sparkle 更新框架，很多 App 共用
    "com.github.electron",  // Electron 开发时的默认 id，谁都可能是它
    "com.crashlytics.",     // 崩溃上报 SDK 的共享缓存
    "io.fabric.",
    "com.plausiblelabs.",
    "com.hockeyapp.",
    "io.sentry",
];

/// 厂商的更新器、守护进程、共享组件（小写前缀）：主人不是某个 App，只要这个厂商还有 App 装着就不报；
/// 厂商的 App 都没了才是孤儿。
const VENDOR_SERVICES: &[&str] = &[
    "com.google.keystone",
    "com.google.googleupdater",
    "com.google.softwareupdate",
    "com.google.googlesoftwareupdate",
    "com.microsoft.autoupdate",
    "com.microsoft.update",
    "com.microsoft.errorreporting",
    "com.microsoft.office",
    "com.microsoft.shared",
    "com.microsoft.edgeupdater",
    "com.microsoft.package.",
    "com.adobe.",
    "com.oracle.java",
    "org.mozilla.updater",
    "com.docker.",
    "us.zoom.",
    "com.logi.",
    "com.logitech.",
    "com.parallels.",
    "com.vmware.",
    "com.citrix.",
];

/// Library 下放非 App 包的地方：输入法、偏好设置面板、QuickLook 插件、JDK……它们也有 bundle id，
/// 也会留偏好设置和缓存。
const BUNDLE_DIRS: [&str; 12] = [
    "PreferencePanes",
    "Input Methods",
    "QuickLook",
    "Screen Savers",
    "Spotlight",
    "Internet Plug-Ins",
    "Services",
    "Extensions",
    "Audio/Plug-Ins/HAL",
    "Audio/Plug-Ins/Components",
    "Java/JavaVirtualMachines",
    // 安全代理插件（如 `Crypt.bundle`）：它也在 `/Library/Preferences` 下留同 id 的 plist。
    "Security/SecurityAgentPlugins",
];

/// 只有 GUI App 才会在这些地方留东西。命令行工具和 Java / 统计 SDK 也会用 bundle id 写偏好设置和
/// Application Support（`cn.tuhu.agentctl` 里就是登录凭据），所以只凭 bundle id 的一组，至少要有一项
/// 落在这些地方才算 likely。`WebData` 只算 `WebKit`：`HTTPStorages` 是任何用 CFNetwork 的带 bundle id
/// 的进程都会建的（见 `in_http_storages`）。
const APP_ONLY_KINDS: [LeftoverKind; 6] = [
    LeftoverKind::Container,
    LeftoverKind::GroupContainer,
    LeftoverKind::ApplicationScripts,
    LeftoverKind::SavedState,
    LeftoverKind::WebData,
    LeftoverKind::Cookies,
];

/// bundle id 里说明不了是哪个 App 的段，起显示名时跳过（`com.superace.updf.mac` → `updf`）。
const GENERIC_SEGMENTS: [&str; 10] = [
    "app",
    "mac",
    "macos",
    "osx",
    "desktop",
    "client",
    "gui",
    "pc",
    "main",
    "application",
];

/// 读主人包的线程数。每个包要跑一次 codesign，放在自己的小池子里，不占满全局 rayon 池。
const OWNER_READERS: usize = 4;

/// 这段时间里还被写过的，多半还有什么在用（不在 App 包里的命令行工具、没挂上的盘里的 App……），
/// 只算 possible，不替用户勾。
const STILL_WRITTEN: Duration = Duration::from_secs(7 * 24 * 3600);

/// 找孤儿。`all_apps` 是 `apps::scan_apps` 的结果（不含 Apple 自带的，函数内部自己补上）。
pub fn scan(
    env: &Env,
    all_apps: &[InstalledApp],
    rules: &RuleSet,
    opts: &OrphanOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> OrphanReport {
    let cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    };

    let Some((owners, degraded)) = collect_owners(env, all_apps, opts, &cancelled) else {
        return OrphanReport::default();
    };
    if cancelled() {
        return OrphanReport::default();
    }
    let claimants: Vec<Claimant> = owners.iter().map(|o| Claimant::new(o, rules)).collect();

    // 卸载时的那批位置。收据、Caskroom、家目录的点文件不看：名字说明不了主人，后者还常被命令行工具共用。
    // 位置自己或上面某一级是符号链接的整个跳过：不跟着链接去别处找，更不会把别处的东西当孤儿移走。
    let mut listings = Listings::new();
    let locations: Vec<SearchLocation> = leftovers::search_locations(env)
        .into_iter()
        .filter(|l| {
            !matches!(
                l.kind,
                LeftoverKind::Receipt | LeftoverKind::HomebrewCask | LeftoverKind::HomeDotfile
            )
        })
        .filter_map(|l| {
            let path = real_path(env, &l.path, false, &mut listings)?;
            Some(SearchLocation { path, ..l })
        })
        .collect();
    let mut candidates: BTreeMap<PathBuf, Candidate> = BTreeMap::new();
    for location in &locations {
        if cancelled() {
            return OrphanReport::default();
        }
        on_progress(ScanProgress::Location(env.display(&location.path)));
        for (path, is_dir) in leftovers::list_entries(&location.path, usize::MAX) {
            let name = file_name(&path);
            let candidate = Candidate::new(path.clone(), location.kind, is_dir, false);
            // 还有 App 装着的厂商目录（`Application Support/Google`）本身不报，看它里面一层。
            // 已装 App 自己的目录（`Application Support/Foo`）不往里看：里面的东西都是它的。
            let vendor_dir = location.depth >= 2
                && is_dir
                && !is_apple_name(&name)
                && !crate::protect::is_system_owned(env, &path)
                && claimants.iter().any(|c| c.is_vendor_dir(&name))
                && !claimants
                    .iter()
                    .any(|c| c.evidence(env, &candidate, rules).is_some());
            if vendor_dir {
                for (child, child_is_dir) in leftovers::list_entries(&path, 500) {
                    let candidate =
                        Candidate::new(child.clone(), location.kind, child_is_dir, true);
                    candidates.entry(child).or_insert(candidate);
                }
            } else {
                candidates.entry(path).or_insert(candidate);
            }
        }
    }

    // 规则库点名、App 却没装的路径，不管搜索位置有没有列到。
    let mut rule_hits: HashMap<PathBuf, RuleHit> = HashMap::new();
    for rule in &rules.rules {
        let installed = owners.iter().any(|o| {
            o.bundle_id
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(&rule.bundle_id))
        });
        if installed {
            continue;
        }
        let phantom = phantom_app(rule);
        for path in rules.declared_paths(env, &phantom) {
            // 只认 Library 里的：`~/.docker`、`~/.vscode` 这种点文件常被命令行工具和别的编辑器共用。
            let in_library = [&env.user_library, &env.system_library]
                .iter()
                .any(|root| path.starts_with(root) && path != **root);
            if !in_library {
                continue;
            }
            let Some(evidence) = rules.evidence(env, &phantom, &path) else {
                continue;
            };
            // 规则写的大小写可能和盘上不同（APFS 不分大小写）：按盘上的真名记，同一个目录只进来一次。
            let Some(path) = real_path(env, &path, true, &mut listings) else {
                continue;
            };
            let kind = leftovers::kind_for_path(env, &locations, &path);
            candidates
                .entry(path.clone())
                .or_insert_with(|| Candidate::new(path.clone(), kind, path.is_dir(), false));
            rule_hits.entry(path).or_insert(RuleHit {
                bundle_id: rule.bundle_id.clone(),
                name: rule.name.clone(),
                confidence: evidence.confidence.min(Confidence::Likely),
            });
        }
    }

    let mut owned = OwnedIds::default();
    let mut vendors: HashSet<String> = HashSet::new();
    // 包里别处的 App（`SogouInput.app/Contents/SogouPreference.app`）：`embedded_ids` 不看那些地方。
    let inner: Vec<String> = owners
        .par_iter()
        .filter(|o| !o.is_system)
        .flat_map_iter(|o| inner_app_ids(&o.path))
        .collect();
    for id in inner.iter().chain(
        owners
            .iter()
            .flat_map(|o| o.bundle_id.iter().chain(&o.embedded_ids)),
    ) {
        vendors.extend(vendor_of(&id.to_lowercase()));
    }
    owned.extend(&inner);
    for owner in &owners {
        owned.extend(owner.bundle_id.iter().chain(&owner.embedded_ids));
        owned.extend(
            owner
                .app_groups
                .iter()
                .map(|g| bundle_id_in(g).unwrap_or_else(|| g.clone())),
        );
    }
    // 还在跑得起来的 launchd 任务（程序还在）：它的 label 有主人。系统自带的任务也算。
    // 厂商还有任务在跑，这个厂商也算还在。
    for candidate in candidates.values() {
        if candidate.program.as_deref().is_some_and(program_exists) {
            let label = candidate_stem(&file_name(&candidate.path));
            if let Some(id) = bundle_id_in(&label) {
                vendors.extend(vendor_of(&id.to_lowercase()));
            }
            owned.extend([label]);
        }
    }
    if let Some(root) = env.system_library.parent() {
        for dir in [
            "System/Library/LaunchAgents",
            "System/Library/LaunchDaemons",
        ] {
            for (path, _) in leftovers::list_entries(&root.join(dir), usize::MAX) {
                owned.extend([candidate_stem(&file_name(&path))]);
            }
        }
    }

    let installed_tokens: HashSet<String> = match &rules.cask {
        Some(index) => owners
            .iter()
            .filter_map(|o| index.token_for(o).map(|(t, _)| t))
            .collect(),
        None => HashSet::new(),
    };
    let ctx = Context {
        env,
        rules,
        claimants,
        owner_bundles: owners
            .iter()
            .filter_map(|o| o.path.file_name())
            .map(|n| n.to_string_lossy().to_lowercase())
            .collect(),
        vendors,
        owned,
        rule_hits,
        casks: rules
            .cask
            .as_ref()
            .map(|index| CaskLookup::new(env, index, &installed_tokens)),
    };

    let candidates: Vec<Candidate> = candidates.into_values().collect();
    let mut findings: Vec<Finding> = candidates
        .par_iter()
        .filter_map(|candidate| {
            if cancelled() {
                return None;
            }
            let mut finding = judge(&ctx, candidate)?;
            // 保护区和 macOS 自己的东西不报；别的阻拦和卸载一样，列出来但不替用户选。
            finding.item.hold_back = leftovers::hold_back_for(env, &candidate.path, &[]);
            match finding.item.hold_back {
                Some(HoldBack::Protected { .. } | HoldBack::SystemOwned) => None,
                _ => Some(finding),
            }
        })
        .collect();
    if cancelled() {
        return OrphanReport::default();
    }

    // 父目录已经在列表里的，子项不再单列（规则点名的路径可能在别的候选里面）。
    findings.sort_by(|a, b| a.item.path.cmp(&b.item.path));
    let mut kept: Vec<Finding> = Vec::with_capacity(findings.len());
    for f in findings {
        if kept
            .last()
            .is_some_and(|last| f.item.path.starts_with(&last.item.path))
        {
            continue;
        }
        kept.push(f);
    }
    let mut findings = kept;

    if opts.measure && !findings.is_empty() {
        let paths: Vec<PathBuf> = findings.iter().map(|f| f.item.path.clone()).collect();
        let cancel = opts.cancel.as_deref();
        let sizes = crate::size::measure_all_with(&paths, opts.size_budget, cancel, on_progress);
        for (f, size) in findings.iter_mut().zip(sizes) {
            f.item.size = size;
        }
        if cancelled() {
            return OrphanReport::default();
        }
    }

    let mut groups = group(findings, rules, &ctx.vendors);
    if degraded {
        // 没问成 Spotlight：别处可能还装着主人，一项都不替用户勾。
        for m in groups.iter_mut().flat_map(|g| g.items.iter_mut()) {
            m.evidence.confidence = m.evidence.confidence.min(Confidence::Possible);
        }
    }
    let report = OrphanReport { groups, degraded };
    on_progress(ScanProgress::Finished);
    report
}

/// 规则库点名的一条路径。
struct RuleHit {
    bundle_id: String,
    name: String,
    confidence: Confidence,
}

/// 一个孤儿，加上分组要用的东西。
struct Finding {
    item: LeftoverMatch,
    /// 分组键，小写。
    key: String,
    /// 规则库 / cask 里的 App 名。
    app_name: Option<String>,
    /// 没有 App 名时，从 bundle id 或 label 推出来的名字。
    fallback_name: String,
    /// 它能说明"这里真装过一个 App"：在只有 GUI App 才会用的地方、launchd 程序没了、或者规则库点名。
    /// cask 只是线索，不算。
    corroborates: bool,
}

/// 判断时用的全部东西，算好一次。
struct Context<'a> {
    env: &'a Env,
    rules: &'a RuleSet,
    claimants: Vec<Claimant<'a>>,
    /// 主人的包文件名（小写，`foo.app`），认 launchd 程序原来所在的 App 是不是只是挪了地方。
    owner_bundles: HashSet<String>,
    /// 还有 App 装着的厂商（`com.foo`）。
    vendors: HashSet<String>,
    owned: OwnedIds,
    rule_hits: HashMap<PathBuf, RuleHit>,
    casks: Option<CaskLookup>,
}

/// 一个信号：凭什么、多大把握、怎么分组。
struct Signal {
    signal: OrphanSignal,
    confidence: Confidence,
    matched: String,
    key: String,
    app_name: Option<String>,
    fallback_name: String,
}

/// 这个候选是孤儿吗？是的话给出最强的信号。
fn judge(ctx: &Context, candidate: &Candidate) -> Option<Finding> {
    let name = file_name(&candidate.path);
    let lower = name.to_lowercase();
    if is_apple_name(&name) || crate::protect::is_system_owned(ctx.env, &candidate.path) {
        return None;
    }
    let id = bundle_id_in(&name);
    let id_l = id.as_deref().map(str::to_lowercase);
    // Rind 自己的东西。
    if lower.split(['.', ' ', '-', '_']).any(|w| w == "rind") {
        return None;
    }
    if let Some(id_l) = &id_l {
        if NEVER_ORPHANS.iter().any(|p| id_l.starts_with(p)) {
            return None;
        }
        let vendor_service = VENDOR_SERVICES
            .iter()
            .any(|p| id_l.starts_with(p) && vendor_of(p).is_some_and(|v| ctx.vendors.contains(&v)));
        if vendor_service || ctx.owned.owns(id_l) {
            return None;
        }
    }
    let is_job = matches!(
        candidate.kind,
        LeftoverKind::LaunchAgent | LeftoverKind::LaunchDaemon
    ) && candidate.path.extension().is_some_and(|e| e == "plist");
    if is_job && candidate.program.as_deref().is_some_and(program_exists) {
        return None;
    }
    if claimed(ctx, candidate) {
        return None;
    }

    // 按优先级列出信号，同样把握时取先出现的。
    let mut signals: Vec<Signal> = Vec::new();
    if let Some(hit) = ctx.rule_hits.get(&candidate.path) {
        signals.push(Signal {
            signal: OrphanSignal::KnownAppGone,
            confidence: hit.confidence,
            matched: hit.name.clone(),
            key: hit.bundle_id.to_lowercase(),
            app_name: Some(hit.name.clone()),
            fallback_name: hit.name.clone(),
        });
    }
    if is_job {
        if let Some(program) = &candidate.program {
            if program_gone(ctx, program) {
                let label = candidate_stem(&name);
                let key = id
                    .as_deref()
                    .map(group_id)
                    .unwrap_or_else(|| normalize_name(&label));
                signals.push(Signal {
                    signal: OrphanSignal::ProgramMissing,
                    // 程序原来在 `.app` 里也只是 likely：说明不了 App 不是换了个地方装。
                    confidence: Confidence::Likely,
                    matched: program.display().to_string(),
                    fallback_name: name_from_id(id.as_deref().unwrap_or(&label)),
                    key: key.to_lowercase(),
                    app_name: None,
                });
            }
        }
    } else if let Some(id) = &id {
        let vendor_alive = vendor_of(&id.to_lowercase()).is_some_and(|v| ctx.vendors.contains(&v));
        let key = group_id(id);
        signals.push(Signal {
            signal: OrphanSignal::BundleIdGone,
            confidence: if vendor_alive {
                Confidence::Possible
            } else {
                Confidence::Likely
            },
            matched: id.clone(),
            fallback_name: name_from_id(&key),
            key: key.to_lowercase(),
            app_name: None,
        });
    }
    if let Some(casks) = &ctx.casks {
        let hit = casks
            .sole_gone_owner(&candidate.path)
            .filter(|(token, app_name, _)| mentions(&name, app_name) || mentions(&name, token));
        if let Some((token, app_name, cask_id)) = hit {
            // cask 数据来自用户可写的缓存，只是线索；点名的东西名字里还得有这个 App
            // （`JetBrains/consentOptions` 是所有 JetBrains IDE 共用的，`~/Library/Jupyter` 是命令行的）。
            // 同一个 App 的东西要进同一组：名字里的 id，其次 cask 推出来的 id，最后才是 token。
            let key = match (&id, cask_id) {
                (Some(id), _) => group_id(id).to_lowercase(),
                (None, Some(cask_id)) => cask_id.to_string(),
                (None, None) => token.to_string(),
            };
            signals.push(Signal {
                signal: OrphanSignal::KnownAppGone,
                confidence: Confidence::Possible,
                matched: token.to_string(),
                key,
                app_name: Some(app_name.to_string()),
                fallback_name: app_name.to_string(),
            });
        }
    }

    let mut best = signals.into_iter().reduce(|best, s| {
        if s.confidence > best.confidence {
            s
        } else {
            best
        }
    })?;
    let corroborates = (APP_ONLY_KINDS.contains(&candidate.kind)
        && !in_http_storages(ctx.env, &candidate.path))
        || best.signal == OrphanSignal::ProgramMissing
        || ctx.rule_hits.contains_key(&candidate.path);
    if best.confidence > Confidence::Possible && recently_written(&candidate.path) {
        best.confidence = Confidence::Possible;
    }
    Some(Finding {
        item: LeftoverMatch {
            path: candidate.path.clone(),
            kind: candidate.kind,
            size: Size::Unknown,
            evidence: Evidence {
                kind: EvidenceKind::Orphan {
                    signal: best.signal,
                },
                confidence: best.confidence,
                matched: best.matched,
            },
            rivals: Vec::new(),
            hold_back: None,
        },
        key: best.key,
        app_name: best.app_name,
        fallback_name: best.fallback_name,
        corroborates,
    })
}

/// 有主人认领它吗：任何主人用 `Claimant` 拿到证据，或者装着的 cask 点名了它。
/// 只凭"同一厂商"拿到的 possible 不算——那正是"厂商还在、这个 App 没了"的情形，由调用方降级处理。
fn claimed(ctx: &Context, candidate: &Candidate) -> bool {
    let by_claimant = ctx.claimants.iter().any(|c| {
        c.evidence(ctx.env, candidate, ctx.rules).is_some_and(|e| {
            !(e.kind == EvidenceKind::BundleIdPrefix && e.confidence == Confidence::Possible)
        })
    });
    by_claimant
        || ctx
            .casks
            .as_ref()
            .is_some_and(|c| c.claimed_by_installed(&candidate.path))
}

/// launchd 要跑的程序确实没了：绝对路径、不在外接盘上、读得到它的上级却找不到它，
/// 它原来所在的 App 也没换个地方装着。
fn program_gone(ctx: &Context, program: &Path) -> bool {
    if !program.is_absolute() || program.starts_with("/Volumes") || program_exists(program) {
        return false;
    }
    let bundle = program
        .ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "app"));
    if let Some(bundle) = bundle {
        let moved = bundle.file_name().is_some_and(|n| {
            ctx.owner_bundles
                .contains(&n.to_string_lossy().to_lowercase())
        });
        if moved || bundle.symlink_metadata().is_ok() {
            return false;
        }
    }
    true
}

/// 读不了（没权限等）的按存在算：没有实据就不说它没了。
fn program_exists(program: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(program),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// 它自己或直接子项最近被写过。读不到时间的按被写过算。
fn recently_written(path: &Path) -> bool {
    let Some(cutoff) = SystemTime::now().checked_sub(STILL_WRITTEN) else {
        return true;
    };
    let recent = |p: &Path| {
        std::fs::symlink_metadata(p)
            .and_then(|m| m.modified())
            .map_or(true, |t| t > cutoff)
    };
    if recent(path) {
        return true;
    }
    leftovers::list_entries(path, 200)
        .iter()
        .any(|(child, _)| recent(child))
}

/// 主人们用着的 id（小写）：bundle id、内嵌组件、App Group、活着的 launchd label。
#[derive(Default)]
struct OwnedIds {
    ids: HashSet<String>,
    /// 这些 id 的上层（≥ 三段）：`com.foo.bar.helper` 有主人，`com.foo.bar` 也就有。
    parents: HashSet<String>,
}

impl OwnedIds {
    fn extend(&mut self, ids: impl IntoIterator<Item = impl AsRef<str>>) {
        for id in ids {
            let id = id.as_ref().to_lowercase();
            for parent in dotted_parents(&id) {
                self.parents.insert(parent.to_string());
            }
            self.ids.insert(id);
        }
    }

    /// `id` 本身、它的上层，或以它开头的 id 有主人。
    fn owns(&self, id_l: &str) -> bool {
        self.ids.contains(id_l)
            || self.parents.contains(id_l)
            || dotted_parents(id_l).any(|p| self.ids.contains(p))
    }
}

/// `a.b.c.d` → `a.b.c`（不含自己，至少三段）。
fn dotted_parents(id: &str) -> impl Iterator<Item = &str> + '_ {
    id.match_indices('.')
        .map(move |(i, _)| &id[..i])
        .filter(|p| p.matches('.').count() >= 2)
}

/// Homebrew cask 的 zap / uninstall 段反过来查：一条路径被哪些 cask 点名。
/// cask 有好几千个，挨个展开 glob 太慢，所以按"字面路径"和"上级目录 + 叶子 glob"建索引。
struct CaskLookup {
    tokens: Vec<String>,
    /// 每个 token 的 App 名（去掉 `.app`）；没有 `app` 段的 cask 说不清是哪个 App，是 None。
    app_names: Vec<Option<String>>,
    installed: Vec<bool>,
    /// 从 zap 段点名的文件名里推出来的 bundle id（小写，分组用）：出现最多的那个。
    bundle_ids: Vec<Option<String>>,
    exact: HashMap<String, Vec<usize>>,
    globs: HashMap<String, Vec<(glob::Pattern, usize)>>,
}

impl CaskLookup {
    fn new(env: &Env, index: &CaskIndex, installed: &HashSet<String>) -> CaskLookup {
        let mut app_names: HashMap<&str, &str> = HashMap::new();
        for (app, token) in &index.app_tokens {
            let entry = app_names.entry(token.as_str()).or_insert(app.as_str());
            if app.as_str() < *entry {
                *entry = app.as_str();
            }
        }
        let mut lookup = CaskLookup {
            tokens: Vec::new(),
            app_names: Vec::new(),
            installed: Vec::new(),
            bundle_ids: Vec::new(),
            exact: HashMap::new(),
            globs: HashMap::new(),
        };
        let mut tokens: Vec<&String> = index.zap_paths.keys().collect();
        tokens.sort();
        for token in tokens {
            let i = lookup.tokens.len();
            lookup.tokens.push(token.clone());
            lookup.app_names.push(
                app_names
                    .get(token.as_str())
                    .map(|a| a.strip_suffix(".app").unwrap_or(a).to_string()),
            );
            lookup.installed.push(installed.contains(token));
            lookup
                .bundle_ids
                .push(cask_bundle_id(&index.zap_paths[token]));
            for cp in &index.zap_paths[token] {
                // `rmdir` 只在目录空时才算，空目录不值得当孤儿报；`$APPDIR` 没有 App 就没法换。
                if cp.only_if_empty || cp.pattern.contains("$APPDIR") {
                    continue;
                }
                let pattern = cp.pattern.replace("$(TeamIdentifierPrefix)", "*.");
                let in_library = [&env.user_library, &env.system_library]
                    .iter()
                    .any(|root| Path::new(&pattern).starts_with(root));
                if !in_library {
                    continue;
                }
                let pattern = pattern.to_lowercase();
                if !crate::rules::has_glob(&pattern) {
                    lookup.exact.entry(pattern).or_default().push(i);
                    continue;
                }
                let Some((parent, leaf)) = pattern.rsplit_once('/') else {
                    continue;
                };
                // 上级目录也带通配符的很少，不值得为它们放慢所有查找。
                if crate::rules::has_glob(parent) {
                    continue;
                }
                if let Ok(leaf) = glob::Pattern::new(leaf) {
                    lookup
                        .globs
                        .entry(parent.to_string())
                        .or_default()
                        .push((leaf, i));
                }
            }
        }
        lookup
    }

    /// 点名这条路径的 cask（下标，去重）。
    fn owners_of(&self, path: &Path) -> Vec<usize> {
        let options = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let path_l = path.to_string_lossy().to_lowercase();
        let mut out: Vec<usize> = self.exact.get(&path_l).cloned().unwrap_or_default();
        if let Some((parent, leaf)) = path_l.rsplit_once('/') {
            if let Some(globs) = self.globs.get(parent) {
                out.extend(
                    globs
                        .iter()
                        .filter(|(p, _)| p.matches_with(leaf, options))
                        .map(|(_, i)| *i),
                );
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn claimed_by_installed(&self, path: &Path) -> bool {
        self.owners_of(path).iter().any(|i| self.installed[*i])
    }

    /// 只有一个（没装的）cask 点名它、而且知道那个 cask 装的是哪个 App 时：(token, App 名, bundle id)。
    /// 好几个 cask 都点名的多半是厂商共用的东西，不猜。
    fn sole_gone_owner(&self, path: &Path) -> Option<(&str, &str, Option<&str>)> {
        match self.owners_of(path).as_slice() {
            [i] if !self.installed[*i] => {
                let name = self.app_names[*i].as_deref()?;
                Some((
                    self.tokens[*i].as_str(),
                    name,
                    self.bundle_ids[*i].as_deref(),
                ))
            }
            _ => None,
        }
    }
}

/// cask 装的 App 的 bundle id：zap 段点名的文件名（`Preferences/com.foo.bar.plist`、
/// `Saved Application State/com.foo.bar.savedState`……）里出现最多的那个，一样多取短的。
/// cask 数据里没有现成的 bundle id，这只用来分组，不当证据。
fn cask_bundle_id(paths: &[crate::rules::CaskPath]) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for cp in paths {
        if crate::rules::has_glob(&cp.pattern) {
            continue;
        }
        let leaf = file_name(Path::new(&cp.pattern));
        if is_apple_name(&leaf) {
            continue;
        }
        if let Some(id) = bundle_id_in(&leaf) {
            *counts.entry(group_id(&id).to_lowercase()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|(a, na), (b, nb)| {
            na.cmp(nb)
                .then_with(|| b.len().cmp(&a.len()))
                .then_with(|| b.cmp(a))
        })
        .map(|(id, _)| id)
}

/// 所有可能的主人：传进来的 App、`env.app_dirs` 里它没认出的（iPhone / iPad App 的 Wrapper 包）、
/// Apple 自带的、Spotlight 找得到的、Library 里带 bundle id 的非 App 包。按路径去重，不含废纸篓里的。
/// 第二项是"该问 Spotlight 却没问成"。中途取消就是 None。
fn collect_owners(
    env: &Env,
    all_apps: &[InstalledApp],
    opts: &OrphanOptions,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<(Vec<InstalledApp>, bool)> {
    let mut owners: BTreeMap<PathBuf, InstalledApp> = all_apps
        .iter()
        .map(|a| (a.path.clone(), a.clone()))
        .collect();
    let mut extra: Vec<PathBuf> = Vec::new();
    for dir in &env.app_dirs {
        for (path, is_dir) in leftovers::list_entries(dir, usize::MAX) {
            if is_app_bundle(&path) {
                extra.push(path);
            } else if is_dir && !env.system_app_dirs.contains(&path) {
                // `/Applications/Adobe/...` 这样的一层子目录，和 `apps::scan_apps` 一样。
                extra.extend(
                    leftovers::list_entries(&path, usize::MAX)
                        .into_iter()
                        .map(|(p, _)| p)
                        .filter(|p| is_app_bundle(p)),
                );
            }
        }
    }
    for dir in &env.system_app_dirs {
        extra.extend(
            leftovers::list_entries(dir, usize::MAX)
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| is_app_bundle(p)),
        );
    }
    let mut degraded = false;
    if opts.spotlight {
        #[cfg(test)]
        let found = opts.fake_spotlight.clone().unwrap_or_else(spotlight_apps);
        #[cfg(not(test))]
        let found = spotlight_apps();
        match found {
            Some(paths) => extra.extend(
                paths
                    .into_iter()
                    .filter(|p| p.is_absolute() && is_app_bundle(p)),
            ),
            None => degraded = true,
        }
    }
    if cancelled() {
        return None;
    }
    extra.retain(|p| !owners.contains_key(p));
    extra.sort();
    extra.dedup();

    let caskroom = caskroom_apps(env);
    let read_one = |p: &PathBuf| {
        if cancelled() {
            None
        } else {
            read_owner(env, p, &caskroom)
        }
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(OWNER_READERS)
        .build();
    let read: Vec<InstalledApp> = match pool {
        Ok(pool) => pool.install(|| extra.par_iter().filter_map(read_one).collect()),
        Err(_) => extra.iter().filter_map(read_one).collect(),
    };
    if cancelled() {
        return None;
    }
    for app in read.into_iter().chain(plugin_bundles(env)) {
        owners.entry(app.path.clone()).or_insert(app);
    }
    // 废纸篓里的 App 已经是"要删的"了：刚把 App 拖进废纸篓再来扫孤儿是最常见的用法。
    let owners = owners
        .into_values()
        .filter(|o| !in_trash(env, &o.path))
        .collect();
    Some((owners, degraded))
}

/// Mac App 包里放内嵌组件的地方。
const MAC_NESTED: [&str; 7] = [
    "Contents/Frameworks",
    "Contents/PlugIns",
    "Contents/XPCServices",
    "Contents/Helpers",
    "Contents/Library/LoginItems",
    "Contents/Library/LaunchServices",
    "Contents/MacOS",
];

/// iPhone / iPad App 内层包（扁平布局）里放内嵌组件的地方。
const IOS_NESTED: [&str; 4] = ["PlugIns", "Frameworks", "Watch", "AppClips"];

/// 读一个主人包，只取认领要用的：名字、bundle id、内嵌组件 id、Team ID、App Group、Homebrew 来源。
/// Mac App 读 `Contents/Info.plist`；iPhone / iPad App（`Foo.app/Wrapper/X.app`）读内层包根目录的
/// `Info.plist`，签名也取内层包的。Apple 的包（`/System` 下、`com.apple.*`）不跑 codesign：
/// 它们的东西按名字就不报，签名信息用不上。
fn read_owner(env: &Env, path: &Path, caskroom: &HashMap<String, String>) -> Option<InstalledApp> {
    let (root, info, nested): (PathBuf, PathBuf, &[&str]) =
        if path.join("Contents/Info.plist").is_file() {
            (
                path.to_path_buf(),
                path.join("Contents/Info.plist"),
                &MAC_NESTED,
            )
        } else {
            let inner = wrapped_bundle(path)?;
            let info = inner.join("Info.plist");
            (inner, info, &IOS_NESTED)
        };
    let plist = crate::apps::read_plist_bounded(&info)?;
    let dict = plist.as_dictionary()?;
    let get = |key: &str| {
        dict.get(key)
            .and_then(|v| v.as_string())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let bundle_id = get("CFBundleIdentifier");
    let under_system = env
        .system_library
        .parent()
        .is_some_and(|r| path.starts_with(r.join("System")));
    let is_system = under_system
        || env.system_app_dirs.iter().any(|d| path.starts_with(d))
        || (bundle_id
            .as_deref()
            .is_some_and(|id| id.starts_with("com.apple."))
            && !path.join("Contents/_MASReceipt/receipt").exists());

    let mut embedded_ids: Vec<String> = Vec::new();
    for sub in nested {
        for (child, is_dir) in leftovers::list_entries(&root.join(sub), 500) {
            if !is_dir || child.extension().is_none() {
                continue;
            }
            let id = [
                "Contents/Info.plist",
                "Resources/Info.plist",
                "Versions/Current/Resources/Info.plist",
                "Info.plist",
            ]
            .iter()
            .find_map(|rel| plist_bundle_id(&child.join(rel)));
            if let Some(id) = id.filter(|id| Some(id) != bundle_id.as_ref()) {
                embedded_ids.push(id);
            }
        }
    }
    embedded_ids.sort();
    embedded_ids.dedup();

    let (team_id, app_groups) = if is_system {
        (None, Vec::new())
    } else {
        signing_info(&root)
    };
    let source = path
        .file_name()
        .and_then(|n| caskroom.get(&n.to_string_lossy().to_string()))
        .map_or(AppSource::Other, |t| AppSource::Homebrew(t.clone()));
    Some(InstalledApp {
        path: path.to_path_buf(),
        name: get("CFBundleDisplayName")
            .or_else(|| get("CFBundleName"))
            .unwrap_or(stem),
        bundle_id,
        version: None,
        executable: None,
        team_id,
        embedded_ids,
        app_groups,
        source,
        size: Size::Unknown,
        is_system,
    })
}

/// 一个 Info.plist 里的 CFBundleIdentifier。
fn plist_bundle_id(path: &Path) -> Option<String> {
    crate::apps::read_plist_bounded(path)?
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 一次 codesign 拿 Team ID（stderr）和 App Group（stdout 的 entitlements）。拿不到就是 None / 空。
/// 5 秒没答案就放弃：取消要等它跑完。
fn signing_info(bundle: &Path) -> (Option<String>, Vec<String>) {
    let bundle = bundle.to_string_lossy().to_string();
    let args = ["-dv", "--entitlements", "-", "--xml", bundle.as_str()];
    let Some(out) =
        crate::apps::run_with_timeout("/usr/bin/codesign", &args, Duration::from_secs(5))
    else {
        return (None, Vec::new());
    };
    let team_id = String::from_utf8_lossy(&out.stderr)
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| is_team_id(t))
        .map(str::to_string);
    // 输出可能带一小段非 XML 的前缀，从 `<?xml` 或 `bplist` 起读。
    let start = out
        .stdout
        .windows(5)
        .position(|w| w == b"<?xml" || w == b"bplis")
        .unwrap_or(0);
    let groups = plist::from_bytes::<plist::Value>(&out.stdout[start..])
        .ok()
        .and_then(|v| {
            v.as_dictionary()?
                .get("com.apple.security.application-groups")?
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.as_string().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default();
    (team_id, groups)
}

/// Homebrew Caskroom 里装着的 `.app` 文件名 → cask token。整个扫描只列一次。
fn caskroom_apps(env: &Env) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(prefix) = &env.homebrew_prefix else {
        return out;
    };
    for (token_dir, _) in leftovers::list_entries(&prefix.join("Caskroom"), usize::MAX) {
        let token = file_name(&token_dir);
        for (version_dir, _) in leftovers::list_entries(&token_dir, usize::MAX) {
            for (entry, _) in leftovers::list_entries(&version_dir, usize::MAX) {
                if entry.extension().is_some_and(|e| e == "app") {
                    out.insert(file_name(&entry), token.clone());
                }
            }
        }
    }
    out
}

/// iPhone / iPad App 的内层包：`WrappedBundle` 指向的那个（只认指向本包 `Wrapper/` 里一层的相对链接），
/// 否则 `Wrapper/` 下第一个带 `Info.plist` 的 `.app`。`Wrapper` 和内层包本身是符号链接的不认：不跟到包外去。
fn wrapped_bundle(path: &Path) -> Option<PathBuf> {
    let wrapper = path.join("Wrapper");
    if !std::fs::symlink_metadata(&wrapper).ok()?.is_dir() {
        return None;
    }
    let linked = std::fs::read_link(path.join("WrappedBundle"))
        .ok()
        .and_then(|target| {
            let mut parts = target.strip_prefix("Wrapper").ok()?.components();
            match (parts.next(), parts.next()) {
                (Some(Component::Normal(name)), None) => Some(wrapper.join(name)),
                _ => None,
            }
        });
    let mut listed: Vec<PathBuf> = leftovers::list_entries(&wrapper, 50)
        .into_iter()
        .filter(|(p, is_dir)| *is_dir && p.extension().is_some_and(|e| e == "app"))
        .map(|(p, _)| p)
        .collect();
    listed.sort();
    linked.into_iter().chain(listed).find(|inner| {
        std::fs::symlink_metadata(inner).is_ok_and(|m| m.is_dir())
            && inner.join("Info.plist").is_file()
    })
}

/// 在 `~/.Trash`，或某个卷的 `.Trashes`（`/.Trashes`、`/Volumes/*/.Trashes`）里。
fn in_trash(env: &Env, path: &Path) -> bool {
    path.starts_with(env.home.join(".Trash"))
        || path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case(".Trashes"))
}

/// Spotlight 知道的全部 App 包（还没筛）。失败、超时、一个都没有（Spotlight 关着、索引坏了）都是 None：
/// 任何一台 Mac 上 Spotlight 至少能找到系统自带的 App。
fn spotlight_apps() -> Option<Vec<PathBuf>> {
    let query = "kMDItemContentType == 'com.apple.application-bundle'";
    let out = crate::apps::run_with_timeout("/usr/bin/mdfind", &[query], Duration::from_secs(10))?;
    if !out.status.success() {
        return None;
    }
    let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// `BUNDLE_DIRS` 里带 bundle id 的包，当成只有 id 和名字的主人。
fn plugin_bundles(env: &Env) -> Vec<InstalledApp> {
    let mut out = Vec::new();
    for library in [&env.user_library, &env.system_library] {
        for sub in BUNDLE_DIRS {
            for (path, is_dir) in leftovers::list_entries(&library.join(sub), 2000) {
                if !is_dir {
                    continue;
                }
                let id = crate::apps::read_plist_bounded(&path.join("Contents/Info.plist"))
                    .and_then(|v| {
                        v.as_dictionary()?
                            .get("CFBundleIdentifier")?
                            .as_string()
                            .map(|s| s.trim().to_string())
                    })
                    .filter(|id| !id.is_empty());
                let Some(id) = id else {
                    continue;
                };
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.push(InstalledApp {
                    path,
                    name,
                    bundle_id: Some(id),
                    version: None,
                    executable: None,
                    team_id: None,
                    embedded_ids: Vec::new(),
                    app_groups: Vec::new(),
                    source: AppSource::Other,
                    size: Size::Unknown,
                    is_system: false,
                });
            }
        }
    }
    out
}

/// 规则库里没装的 App，当成一个只有 id 和名字的 App，好复用规则的路径展开。
fn phantom_app(rule: &Rule) -> InstalledApp {
    InstalledApp {
        path: PathBuf::from("/Applications").join(format!("{}.app", rule.name)),
        name: rule.name.clone(),
        bundle_id: Some(rule.bundle_id.clone()),
        version: None,
        executable: None,
        team_id: None,
        embedded_ids: Vec::new(),
        app_groups: Vec::new(),
        source: AppSource::Other,
        size: Size::Unknown,
        is_system: false,
    }
}

/// 按分组键归组，组按已知大小从大到小，组内也是。同一个 App 尽量只出一组：
/// 1. `com.foo.bar.x` 并进已有的 `com.foo.bar`；
/// 2. 厂商的 App 都没了时，同一厂商（`com.foo`）的单项碎组并起来（`com.jprofiler.v14`、`.jres`……）；
/// 3. 只有 cask token 的组，App 名或 token 和某个 bundle id 组的显示名一样（归一化后、只有一个）就并过去。
///
/// 显示名先用规则 / cask 的 App 名（键就是某条规则的 bundle id 也算），再用 bundle id 推出来的名字。
fn group(findings: Vec<Finding>, rules: &RuleSet, vendors: &HashSet<String>) -> Vec<OrphanGroup> {
    let mut by_key: BTreeMap<String, Vec<Finding>> = BTreeMap::new();
    for f in findings {
        by_key.entry(f.key.clone()).or_default().push(f);
    }
    let merge = |by_key: &mut BTreeMap<String, Vec<Finding>>, from: &str, to: &str| {
        if from != to {
            let moved = by_key.remove(from).unwrap_or_default();
            by_key.entry(to.to_string()).or_default().extend(moved);
        }
    };

    let keys: Vec<String> = by_key.keys().cloned().collect();
    for key in keys {
        let target = dotted_parents(&key)
            .find(|p| by_key.contains_key(*p))
            .map(str::to_string);
        if let Some(target) = target {
            merge(&mut by_key, &key, &target);
        }
    }

    // 有没有旁证按"同一个 bundle id"算，在按厂商、按名字合并之前：
    // 同厂商另一个产品的容器说明不了这个产品的偏好设置。
    for findings in by_key.values_mut() {
        if !findings.iter().any(|f| f.corroborates) {
            for f in findings.iter_mut() {
                f.item.evidence.confidence = Confidence::Possible;
            }
        }
    }

    let mut by_vendor: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in by_key.keys() {
        if let Some(vendor) = bundle_id_in(key).and_then(|id| vendor_of(&id.to_lowercase())) {
            if !vendors.contains(&vendor) {
                by_vendor.entry(vendor).or_default().push(key.clone());
            }
        }
    }
    for (vendor, keys) in by_vendor {
        let singles: Vec<&String> = keys.iter().filter(|k| by_key[*k].len() == 1).collect();
        if keys.len() < 2 || singles.is_empty() {
            continue;
        }
        // 有多项的组就并进最大的那个；全是单项的，并成一个以厂商为键的组。
        let target = keys
            .iter()
            .filter(|k| by_key[*k].len() > 1)
            .max_by(|a, b| {
                by_key[*a]
                    .len()
                    .cmp(&by_key[*b].len())
                    .then_with(|| b.cmp(a))
            })
            .cloned()
            .unwrap_or(vendor);
        for key in singles {
            merge(&mut by_key, key, &target);
        }
    }

    let displays: Vec<(String, String)> = by_key
        .iter()
        .filter(|(key, _)| key.contains('.'))
        .map(|(key, findings)| {
            (
                key.clone(),
                normalize_name(&display_of(key, findings, rules)),
            )
        })
        .collect();
    let token_keys: Vec<String> = by_key
        .iter()
        .filter(|(key, findings)| {
            !key.contains('.') && findings.iter().any(|f| f.app_name.is_some())
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in token_keys {
        let mut names: HashSet<String> = by_key[&key]
            .iter()
            .filter_map(|f| f.app_name.as_deref())
            .map(normalize_name)
            .collect();
        names.insert(normalize_name(&key));
        let matches: Vec<&String> = displays
            .iter()
            .filter(|(_, d)| names.contains(d))
            .map(|(k, _)| k)
            .collect();
        if let [target] = matches.as_slice() {
            merge(&mut by_key, &key, target);
        }
    }

    let mut groups: Vec<OrphanGroup> = by_key
        .into_iter()
        .map(|(key, mut findings)| {
            findings.sort_by(|a, b| a.item.path.cmp(&b.item.path));
            let display_name = display_of(&key, &findings, rules);
            let mut items: Vec<LeftoverMatch> = findings.into_iter().map(|f| f.item).collect();
            items.sort_by(|a, b| {
                size_rank(b.size)
                    .cmp(&size_rank(a.size))
                    .then_with(|| a.path.cmp(&b.path))
            });
            OrphanGroup {
                key,
                display_name,
                items,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.known_size()
            .cmp(&a.known_size())
            .then_with(|| a.key.cmp(&b.key))
    });
    groups
}

/// 一组的显示名：规则 / cask 的 App 名 → 键是某条规则的 bundle id → 键对应的那项推出来的名字 → 从键推。
fn display_of(key: &str, findings: &[Finding], rules: &RuleSet) -> String {
    findings
        .iter()
        .find_map(|f| f.app_name.clone())
        .or_else(|| {
            rules
                .rules
                .iter()
                .find(|r| r.bundle_id.eq_ignore_ascii_case(key))
                .map(|r| r.name.clone())
        })
        .or_else(|| {
            findings
                .iter()
                .find(|f| f.key == key)
                .map(|f| f.fallback_name.clone())
        })
        .unwrap_or_else(|| name_from_id(key))
}

/// 排序用：已知大小在前，未知的最后。
fn size_rank(size: Size) -> (bool, u64) {
    match size {
        Size::Known(n) => (true, n),
        Size::Unknown => (false, 0),
    }
}

/// 名字里的 bundle id（原样大小写）：去掉 `.plist` 之类的后缀、App Group 的 `group.` 和 Team 前缀。
/// 不像反向域名（至少三段、第一段是 2–6 个字母的顶级域）就是 None。
fn bundle_id_in(name: &str) -> Option<String> {
    let strip_group = |s: &str| -> String {
        for prefix in ["group.", "groups.", "systemgroup."] {
            if s.len() > prefix.len()
                && s.is_char_boundary(prefix.len())
                && s[..prefix.len()].eq_ignore_ascii_case(prefix)
            {
                return s[prefix.len()..].to_string();
            }
        }
        s.to_string()
    };
    // `group.TEAMID.x` 和 `TEAMID.group.x` 两种顺序都有。
    let mut s = strip_group(&candidate_stem(name));
    if let Some((head, rest)) = s.split_once('.') {
        if is_team_id(head) {
            s = strip_group(rest);
        }
    }
    let s = s.as_str();
    let parts: Vec<&str> = s.split('.').collect();
    let looks_like = parts.len() >= 3
        && (2..=6).contains(&parts[0].len())
        && parts[0].chars().all(|c| c.is_ascii_alphabetic())
        && !parts[1].chars().all(|c| c.is_ascii_digit())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
    looks_like.then(|| s.to_string())
}

/// Apple 发的 Team ID：10 位大写字母或数字。
fn is_team_id(s: &str) -> bool {
    s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// 分组用的 id：去掉结尾的 helper 一类后缀（`com.foo.bar.ShipIt` → `com.foo.bar`），至少留三段。
fn group_id(id: &str) -> String {
    let mut parts: Vec<&str> = id.split('.').collect();
    while parts.len() > 3
        && parts
            .last()
            .is_some_and(|p| HELPER_SUFFIXES.contains(&p.to_lowercase().as_str()))
    {
        parts.pop();
    }
    parts.join(".")
}

/// 从 bundle id 或 label 起个名字：从后往前第一段像名字的（不是 `app` / `mac` 这种、不是纯数字），
/// 顶级域那段不算。不是点分的就是它自己。
fn name_from_id(id: &str) -> String {
    let parts: Vec<&str> = id.split('.').collect();
    if parts.len() < 2 {
        return id.to_string();
    }
    parts[1..]
        .iter()
        .rev()
        .find(|p| {
            !p.is_empty()
                && !GENERIC_SEGMENTS.contains(&p.to_lowercase().as_str())
                && !p.chars().all(|c| c.is_ascii_digit())
        })
        .or(parts.last())
        .map_or_else(|| id.to_string(), |p| p.to_string())
}

/// 文件名里有没有 `name`（归一化后作为一个词，或长一点的名字被包含）。
fn mentions(file_name: &str, name: &str) -> bool {
    let name = normalize_name(name);
    if name.chars().count() < 3 {
        return false;
    }
    let stem = candidate_stem(file_name).to_lowercase();
    stem.split(['.', ' ', '-', '_'])
        .any(|w| normalize_name(w) == name)
        || (name.chars().count() >= 4 && normalize_name(&stem).contains(&name))
}

/// 包里 `Contents`、`Resources`、`SharedSupport`、`Applications` 下直接放着的 App 的 bundle id。
fn inner_app_ids(bundle: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in [
        "Contents",
        "Contents/Resources",
        "Contents/SharedSupport",
        "Contents/Applications",
    ] {
        for (path, is_dir) in leftovers::list_entries(&bundle.join(sub), 200) {
            if !is_dir || path.extension().is_none_or(|e| e != "app") {
                continue;
            }
            let id =
                crate::apps::read_plist_bounded(&path.join("Contents/Info.plist")).and_then(|v| {
                    v.as_dictionary()?
                        .get("CFBundleIdentifier")?
                        .as_string()
                        .map(str::to_string)
                });
            out.extend(id);
        }
    }
    out
}

/// `com.foo.bar` → `com.foo`；输入已是小写。
fn vendor_of(id_l: &str) -> Option<String> {
    let mut parts = id_l.split('.');
    let (a, b) = (parts.next()?, parts.next()?);
    (!a.is_empty() && !b.is_empty()).then(|| format!("{a}.{b}"))
}

/// Apple 的东西：`com.apple.*`、`group.com.apple.*`，以及带 Team 前缀的 `XXXX.groups.com.apple.*`。
fn is_apple_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("com.apple") || lower.contains(".com.apple.")
}

/// `.app`：Mac App（`Contents/Info.plist`），或 iPhone / iPad App 的 Wrapper 包。
fn is_app_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "app")
        && (path.join("Contents/Info.plist").is_file() || wrapped_bundle(path).is_some())
}

/// 在 `~/Library/HTTPStorages` 下。
fn in_http_storages(env: &Env, path: &Path) -> bool {
    path.strip_prefix(&env.user_library)
        .ok()
        .and_then(|rest| rest.components().next())
        .is_some_and(|c| c.as_os_str().eq_ignore_ascii_case("HTTPStorages"))
}

/// 列过的目录：名字和 inode。
type Listings = HashMap<PathBuf, Vec<(OsString, u64)>>;

/// `path` 在盘上的真实写法：从家目录（或系统根）往下逐级换成目录里列出的真名——APFS 不分大小写，
/// 规则写的大小写可能和盘上不同。逐级不存在、或某一级是符号链接（`leaf_may_link` 时最后一级除外）就是 None。
fn real_path(
    env: &Env,
    path: &Path,
    leaf_may_link: bool,
    listings: &mut Listings,
) -> Option<PathBuf> {
    let anchor = if path.starts_with(&env.home) {
        env.home.as_path()
    } else {
        env.system_library
            .parent()
            .filter(|r| path.starts_with(r))?
    };
    let parts: Vec<&OsStr> = path
        .strip_prefix(anchor)
        .ok()?
        .components()
        .map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let mut real = anchor.to_path_buf();
    for (i, name) in parts.iter().enumerate() {
        let meta = std::fs::symlink_metadata(real.join(name)).ok()?;
        let last = i + 1 == parts.len();
        if meta.file_type().is_symlink() && !(last && leaf_may_link) {
            return None;
        }
        let entries = listings.entry(real.clone()).or_insert_with(|| {
            std::fs::read_dir(&real)
                .map(|rd| rd.flatten().map(|e| (e.file_name(), e.ino())).collect())
                .unwrap_or_default()
        });
        // 写法一样就用它；否则找同一个 inode 的那一项；列不出来就只能照原样。
        let found = if entries.iter().any(|(n, _)| n == name) {
            None
        } else {
            entries
                .iter()
                .find(|(_, ino)| *ino == meta.ino())
                .map(|(n, _)| n.clone())
        };
        real.push(found.as_deref().unwrap_or(name));
    }
    Some(real)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::scan_apps;
    use crate::apps::testing::{make_app, make_embedded};
    use crate::rules::{CaskPath, RulePath};
    use std::sync::Mutex;

    struct Fixture {
        _dir: tempfile::TempDir,
        env: Env,
        apps: Vec<InstalledApp>,
    }

    /// 两个月前的时间：孤儿是很久没人写的东西。
    fn long_ago() -> SystemTime {
        SystemTime::now() - Duration::from_secs(60 * 24 * 3600)
    }

    fn age(path: &Path) {
        for (child, _) in leftovers::list_entries(path, usize::MAX) {
            age(&child);
        }
        std::fs::File::open(path)
            .unwrap()
            .set_modified(long_ago())
            .unwrap();
    }

    fn dir(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("data.bin"), vec![1u8; bytes]).unwrap();
    }

    fn file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn bundle(path: &Path, id: &str) {
        std::fs::create_dir_all(path.join("Contents")).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleIdentifier".into(), id.into());
        plist::Value::Dictionary(dict)
            .to_file_xml(path.join("Contents/Info.plist"))
            .unwrap();
    }

    fn launch_agent(path: &Path, program: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("Program".into(), program.display().to_string().into());
        plist::Value::Dictionary(dict).to_file_xml(path).unwrap();
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env::for_test(tmp.path());
        let apps_dir = env.app_dirs[0].clone();
        let foo = make_app(&apps_dir, "Foo", "com.acme.foo");
        make_embedded(&foo, "PlugIns", "Share.appex", "com.acme.foo.share");
        bundle(&foo.join("Contents/Resources/Inner.app"), "com.inner.tool");
        make_app(&apps_dir, "Chrome", "com.google.Chrome");
        make_app(&env.system_app_dirs[0], "Mail", "com.apple.mail");
        let ul = env.user_library.clone();
        bundle(
            &ul.join("PreferencePanes/Panel.prefPane"),
            "com.panes.panel",
        );

        // 已装 App 的东西。
        dir(&ul.join("Application Support/com.acme.foo"), 100);
        file(&ul.join("Preferences/com.acme.foo.plist"));
        dir(&ul.join("Containers/com.acme.foo.share"), 100);
        dir(&ul.join("Group Containers/group.com.acme.foo"), 100);
        dir(&ul.join("Group Containers/group.net.shared.stuff"), 100);
        file(&ul.join("Preferences/com.panes.panel.plist"));
        file(&ul.join("Preferences/com.inner.tool.plist"));
        // 已删 App 的：厂商也没了 → likely；厂商还在 → possible。
        dir(&ul.join("Caches/com.gone.thing"), 64 * 1024);
        file(&ul.join("Preferences/com.gone.thing.plist"));
        dir(&ul.join("Caches/com.gone.thing.ShipIt"), 100);
        dir(&ul.join("HTTPStorages/com.gone.thing"), 100);
        dir(&ul.join("WebKit/com.gone.thing"), 100);
        // 厂商也没了，但只有 HTTPStorages 和偏好设置：用 CFNetwork 的命令行工具也会留这些。
        dir(&ul.join("HTTPStorages/com.cfnet.tool"), 100);
        file(&ul.join("Preferences/com.cfnet.tool.plist"));
        // 只有 Application Support 的 bundle id：可能是命令行工具的（凭据都在里面）。
        dir(&ul.join("Application Support/cn.cli.tool"), 100);
        file(&ul.join("Preferences/com.acme.oldtool.plist"));
        dir(&ul.join("Application Support/Acme/com.acme.nested"), 100);
        // 已装 App 自己的目录里的东西是它的，哪怕名字像别人的 bundle id。
        dir(&ul.join("Application Support/Foo/com.zed.inner"), 100);
        // Apple 的、谁都能用的、厂商还在时的更新器。
        dir(&ul.join("Caches/com.apple.Something"), 100);
        dir(
            &ul.join("Group Containers/243LU875E5.groups.com.apple.podcasts"),
            100,
        );
        dir(
            &ul.join("Group Containers/group.is.workflow.shortcuts"),
            100,
        );
        dir(&ul.join("Caches/org.sparkle-project.Sparkle"), 100);
        file(&ul.join("Preferences/com.google.Keystone.Agent.plist"));
        // 保护区：带钱包的浏览器 profile。
        dir(
            &ul.join("Application Support/com.gone.browser/Default/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn"),
            100,
        );
        // 普通名字的陌生文件夹。
        dir(&ul.join("Application Support/Some Folder"), 100);
        dir(&ul.join("Caches/randomtool"), 100);
        // launchd：程序没了 / 程序还在。
        launch_agent(
            &ul.join("LaunchAgents/com.vanished.agent.plist"),
            &apps_dir.join("Vanished.app/Contents/MacOS/agent"),
        );
        launch_agent(
            &ul.join("LaunchAgents/homebrew.mxcl.gonedb.plist"),
            &env.home.join("opt/gonedb/bin/gonedb"),
        );
        let alive = env.home.join("bin/alive");
        file(&alive);
        launch_agent(&ul.join("LaunchAgents/com.alive.agent.plist"), &alive);
        file(&ul.join("Preferences/com.alive.agent.plist"));
        // 规则库点名、App 没装的。
        dir(&ul.join("Application Support/Ruled Data"), 100);
        dir(&env.home.join(".ruled"), 100);
        dir(&ul.join("Mobile Documents/com~ruled~app"), 100);
        // cask 点名的。
        dir(&ul.join("Application Support/GoneCask"), 100);
        dir(&ul.join("Application Support/SharedThing"), 100);
        file(&ul.join("Preferences/com.gonecask.app.plist"));
        // 只有 token 的 cask 组，按 App 名并进 bundle id 组。
        dir(&ul.join("Application Support/Widgety"), 100);
        dir(&ul.join("HTTPStorages/com.widgetmaker.widgety"), 100);
        // 厂商的 App 都没了：同一厂商的碎组并起来；厂商还在的不并。
        file(&ul.join("Preferences/com.deadvendor.one.plist"));
        file(&ul.join("Preferences/com.deadvendor.two.plist"));
        dir(&ul.join("Containers/com.deadvendor.big"), 100);
        dir(&ul.join("WebKit/com.deadvendor.big"), 100);
        file(&ul.join("Preferences/com.solovendor.a.plist"));
        file(&ul.join("Preferences/com.solovendor.b.plist"));
        file(&ul.join("Preferences/com.acme.another.plist"));
        // 废纸篓里的 App 不算主人。
        dir(&ul.join("Containers/com.trashed.app"), 100);
        dir(&ul.join("Containers/com.oldtrash.app"), 100);
        dir(&ul.join("Application Support/BrowserCaskData"), 100);
        // Rind 自己的数据。
        dir(&env.data_dir, 100);

        age(&ul);
        age(&env.home.join(".ruled"));
        let trashed = make_app(&env.home.join(".Trash"), "Trashed", "com.trashed.app");
        let root = env.system_library.parent().unwrap().to_path_buf();
        let old = make_app(
            &root.join("Volumes/Ext/.Trashes/501"),
            "Old",
            "com.oldtrash.app",
        );
        let mut apps = scan_apps(&env);
        // 当作 Spotlight 找到的。
        apps.extend(
            [trashed, old]
                .iter()
                .filter_map(|p| crate::apps::read_app(&env, p)),
        );
        // 测试里的 App 没签名，拿不到 App Group，手动给。
        for app in &mut apps {
            if app.bundle_id.as_deref() == Some("com.acme.foo") {
                app.app_groups = vec!["group.net.shared.stuff".into()];
            }
        }
        Fixture {
            _dir: tmp,
            env,
            apps,
        }
    }

    /// 一条规则（App 没装），和一个装着 / 一个没装的 cask。
    fn rules_for(env: &Env) -> RuleSet {
        let path = |rest: &str| CaskPath {
            pattern: env.user_library.join(rest).display().to_string(),
            only_if_empty: false,
        };
        let mut cask = CaskIndex::default();
        cask.app_tokens
            .insert("Gone Cask.app".into(), "gone-cask".into());
        cask.app_tokens
            .insert("Chrome.app".into(), "google-chrome".into());
        cask.app_tokens
            .insert("Widgety.app".into(), "widgety".into());
        cask.zap_paths
            .insert("widgety".into(), vec![path("Application Support/Widgety")]);
        cask.zap_paths.insert(
            "gone-cask".into(),
            vec![
                path("Application Support/GoneCask"),
                path("Application Support/SharedThing"),
                path("Preferences/com.gonecask.app.plist"),
            ],
        );
        cask.zap_paths.insert(
            "google-chrome".into(),
            vec![path("Application Support/BrowserCaskData")],
        );
        RuleSet {
            rules: vec![Rule {
                bundle_id: "com.example.ruled".into(),
                name: "Ruled App".into(),
                aliases: vec![],
                paths: vec![
                    RulePath {
                        path: "~/Library/Application Support/Ruled Data".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    },
                    RulePath {
                        path: "~/.ruled".into(),
                        confidence: Confidence::Likely,
                        note: None,
                    },
                    RulePath {
                        path: "~/Library/Mobile Documents/com~ruled~app".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    },
                ],
            }],
            cask: Some(cask),
        }
    }

    fn opts() -> OrphanOptions {
        OrphanOptions {
            spotlight: false,
            ..OrphanOptions::default()
        }
    }

    fn run(f: &Fixture) -> (OrphanReport, Vec<ScanProgress>) {
        let progress = Mutex::new(Vec::new());
        let report = scan(&f.env, &f.apps, &rules_for(&f.env), &opts(), &|p| {
            progress.lock().unwrap().push(p)
        });
        (report, progress.into_inner().unwrap())
    }

    fn by_path(env: &Env, report: &OrphanReport) -> BTreeMap<String, LeftoverMatch> {
        report
            .groups
            .iter()
            .flat_map(|g| g.items.iter())
            .map(|m| (env.display(&m.path), m.clone()))
            .collect()
    }

    fn signal(m: &LeftoverMatch) -> OrphanSignal {
        match m.evidence.kind {
            EvidenceKind::Orphan { signal } => signal,
            ref other => panic!("not an orphan: {other:?}"),
        }
    }

    #[test]
    fn reports_only_what_has_real_evidence() {
        let f = fixture();
        let (report, progress) = run(&f);
        let found = by_path(&f.env, &report);
        let paths: Vec<&str> = found.keys().map(String::as_str).collect();
        let mut expected = [
            "~/Library/Application Support/Acme/com.acme.nested",
            "~/Library/Application Support/GoneCask",
            "~/Library/Application Support/Ruled Data",
            "~/Library/Application Support/Widgety",
            "~/Library/Application Support/cn.cli.tool",
            "~/Library/Caches/com.gone.thing",
            "~/Library/Caches/com.gone.thing.ShipIt",
            "~/Library/Containers/com.deadvendor.big",
            "~/Library/Containers/com.oldtrash.app",
            "~/Library/Containers/com.trashed.app",
            "~/Library/HTTPStorages/com.cfnet.tool",
            "~/Library/HTTPStorages/com.gone.thing",
            "~/Library/HTTPStorages/com.widgetmaker.widgety",
            "~/Library/WebKit/com.deadvendor.big",
            "~/Library/WebKit/com.gone.thing",
            "~/Library/LaunchAgents/com.vanished.agent.plist",
            "~/Library/LaunchAgents/homebrew.mxcl.gonedb.plist",
            "~/Library/Preferences/com.acme.another.plist",
            "~/Library/Preferences/com.acme.oldtool.plist",
            "~/Library/Preferences/com.cfnet.tool.plist",
            "~/Library/Preferences/com.deadvendor.one.plist",
            "~/Library/Preferences/com.deadvendor.two.plist",
            "~/Library/Preferences/com.gone.thing.plist",
            "~/Library/Preferences/com.gonecask.app.plist",
            "~/Library/Preferences/com.solovendor.a.plist",
            "~/Library/Preferences/com.solovendor.b.plist",
        ];
        expected.sort();
        assert_eq!(paths, expected);
        assert!(matches!(progress.first(), Some(ScanProgress::Location(_))));
        assert!(progress
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { .. })));
        assert_eq!(progress.last(), Some(&ScanProgress::Finished));
    }

    #[test]
    fn a_gone_bundle_id_is_likely_unless_its_maker_is_still_here() {
        let f = fixture();
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);

        let gone = &found["~/Library/Caches/com.gone.thing"];
        assert_eq!(signal(gone), OrphanSignal::BundleIdGone);
        assert_eq!(gone.evidence.confidence, Confidence::Likely);
        assert!(gone.suggested(), "{gone:?}");

        for path in [
            "~/Library/Preferences/com.acme.oldtool.plist",
            "~/Library/Application Support/Acme/com.acme.nested",
            // 没有一项在只有 GUI App 才会用的地方。
            "~/Library/Application Support/cn.cli.tool",
        ] {
            let m = &found[path];
            assert_eq!(signal(m), OrphanSignal::BundleIdGone);
            assert_eq!(m.evidence.confidence, Confidence::Possible, "{path}");
            assert!(!m.suggested());
        }
    }

    #[test]
    fn launchd_job_whose_program_is_gone() {
        let f = fixture();
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);
        for path in [
            "~/Library/LaunchAgents/com.vanished.agent.plist",
            "~/Library/LaunchAgents/homebrew.mxcl.gonedb.plist",
        ] {
            let m = &found[path];
            assert_eq!(signal(m), OrphanSignal::ProgramMissing, "{path}");
            assert_eq!(m.evidence.confidence, Confidence::Likely);
        }
        let brew = report
            .groups
            .iter()
            .find(|g| g.key == "homebrew.mxcl.gonedb")
            .unwrap();
        assert_eq!(brew.display_name, "gonedb");
    }

    #[test]
    fn known_app_gone_from_rules_and_casks() {
        let f = fixture();
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);

        let ruled = &found["~/Library/Application Support/Ruled Data"];
        assert_eq!(signal(ruled), OrphanSignal::KnownAppGone);
        assert_eq!(
            ruled.evidence.confidence,
            Confidence::Likely,
            "a rule's certain is capped at likely"
        );
        let group = report
            .groups
            .iter()
            .find(|g| g.key == "com.example.ruled")
            .unwrap();
        assert_eq!(group.display_name, "Ruled App");

        let cask = &found["~/Library/Application Support/GoneCask"];
        assert_eq!(signal(cask), OrphanSignal::KnownAppGone);
        assert_eq!(cask.evidence.confidence, Confidence::Possible);
        // cask 的 zap 段里有 `com.gonecask.app.plist`：按这个 id 分组，和 bundle id 那项进同一组。
        let group = report
            .groups
            .iter()
            .find(|g| g.key == "com.gonecask.app")
            .unwrap();
        assert_eq!(group.items.len(), 2);
        assert_eq!(group.display_name, "Gone Cask");
    }

    #[test]
    fn groups_merge_helpers_and_sort_by_size() {
        let f = fixture();
        let (report, _) = run(&f);
        let first = &report.groups[0];
        assert_eq!(first.key, "com.gone.thing");
        assert_eq!(first.display_name, "thing");
        let items: Vec<String> = first.items.iter().map(|m| f.env.display(&m.path)).collect();
        assert_eq!(
            items,
            [
                "~/Library/Caches/com.gone.thing",
                "~/Library/Caches/com.gone.thing.ShipIt",
                "~/Library/HTTPStorages/com.gone.thing",
                "~/Library/Preferences/com.gone.thing.plist",
                "~/Library/WebKit/com.gone.thing",
            ]
        );
        let sizes: Vec<u64> = report.groups.iter().map(OrphanGroup::known_size).collect();
        let mut sorted = sizes.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted);
    }

    #[test]
    fn recently_written_is_only_possible() {
        let f = fixture();
        let fresh = f.env.user_library.join("Containers/com.fresh.thing");
        dir(&fresh, 100);
        let (report, _) = run(&f);
        let m = &by_path(&f.env, &report)["~/Library/Containers/com.fresh.thing"];
        assert_eq!(signal(m), OrphanSignal::BundleIdGone);
        assert_eq!(m.evidence.confidence, Confidence::Possible);
    }

    #[test]
    fn cancel_returns_nothing() {
        let f = fixture();
        let cancel = Arc::new(AtomicBool::new(true));
        let progress = Mutex::new(Vec::new());
        let report = scan(
            &f.env,
            &f.apps,
            &rules_for(&f.env),
            &OrphanOptions {
                cancel: Some(cancel),
                ..opts()
            },
            &|p| progress.lock().unwrap().push(p),
        );
        assert!(report.groups.is_empty());
        assert!(!progress.lock().unwrap().contains(&ScanProgress::Finished));
    }

    #[test]
    fn bundle_ids_in_names() {
        assert_eq!(
            bundle_id_in("com.foo.Bar.plist").as_deref(),
            Some("com.foo.Bar")
        );
        assert_eq!(
            bundle_id_in("com.foo.bar.savedState").as_deref(),
            Some("com.foo.bar")
        );
        assert_eq!(
            bundle_id_in("group.com.foo.bar").as_deref(),
            Some("com.foo.bar")
        );
        assert_eq!(
            bundle_id_in("UBF8T346G9.com.foo.bar").as_deref(),
            Some("com.foo.bar")
        );
        assert_eq!(
            bundle_id_in("W5364U7YZB.group.io.foo.bar").as_deref(),
            Some("io.foo.bar")
        );
        for plain in [
            "Foo",
            "com.foo",
            "jetbrains.iu.2025",
            "Foo Bar.1.2",
            "homebrew.mxcl.x",
            "1.2.3",
            "UBF8T346G9.Office",
        ] {
            assert_eq!(bundle_id_in(plain), None, "{plain}");
        }
        assert_eq!(group_id("com.foo.bar.ShipIt"), "com.foo.bar");
        assert_eq!(name_from_id("com.superace.updf.mac"), "updf");
        assert_eq!(name_from_id("com.install4j.1234.2"), "install4j");
        assert_eq!(name_from_id("homebrew.mxcl.gonedb"), "gonedb");
        assert!(mentions("CherryStudio", "Cherry Studio"));
        assert!(mentions("cursor-updater", "Cursor"));
        assert!(!mentions("consentOptions", "PhpStorm"));
        assert_eq!(group_id("com.foo.helper"), "com.foo.helper");
    }

    #[test]
    fn apps_in_the_trash_own_nothing() {
        let f = fixture();
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);
        for path in [
            "~/Library/Containers/com.trashed.app",
            "~/Library/Containers/com.oldtrash.app",
        ] {
            let m = &found[path];
            assert_eq!(signal(m), OrphanSignal::BundleIdGone, "{path}");
            assert_eq!(m.evidence.confidence, Confidence::Likely, "{path}");
        }
        assert!(in_trash(&f.env, &f.env.home.join(".Trash/X.app")));
        assert!(in_trash(&f.env, Path::new("/.Trashes/501/X.app")));
        assert!(in_trash(
            &f.env,
            Path::new("/Volumes/Ext/.Trashes/501/X.app")
        ));
        assert!(!in_trash(&f.env, Path::new("/Applications/X.app")));
    }

    #[test]
    fn one_app_one_group() {
        let f = fixture();
        let (report, _) = run(&f);
        let keys = |g: &OrphanGroup| -> Vec<String> {
            g.items.iter().map(|m| f.env.display(&m.path)).collect()
        };

        // 只有 token 的 cask 组按 App 名并进 bundle id 组，显示 cask 的 App 名。
        let widgety = report
            .groups
            .iter()
            .find(|g| g.key == "com.widgetmaker.widgety")
            .unwrap();
        assert_eq!(widgety.display_name, "Widgety");
        assert_eq!(
            keys(widgety),
            [
                "~/Library/Application Support/Widgety",
                "~/Library/HTTPStorages/com.widgetmaker.widgety",
            ]
        );
        assert!(report.groups.iter().all(|g| g.key != "widgety"));

        // 厂商没了：碎组并成一个以厂商为键的组。
        // 有多项的组时并进它，但并进来的单项不因为别的产品的容器而变成 likely。
        let dead = report
            .groups
            .iter()
            .find(|g| g.key == "com.deadvendor.big")
            .unwrap();
        assert_eq!(dead.items.len(), 4);
        for m in &dead.items {
            let expected = if m.kind == LeftoverKind::Preferences {
                Confidence::Possible
            } else {
                Confidence::Likely
            };
            assert_eq!(m.evidence.confidence, expected, "{:?}", m.path);
        }

        // 全是单项（`com.jprofiler.v14`、`.jres`……）：并成以厂商为键的一组。
        let solo = report
            .groups
            .iter()
            .find(|g| g.key == "com.solovendor")
            .unwrap();
        assert_eq!(solo.display_name, "solovendor");
        assert_eq!(solo.items.len(), 2);

        // 厂商还在：各是各的。
        for key in ["com.acme.another", "com.acme.oldtool", "com.acme.nested"] {
            let g = report.groups.iter().find(|g| g.key == key);
            assert_eq!(g.map(|g| g.items.len()), Some(1), "{key}");
        }
    }

    #[test]
    fn owned_ids_cover_parents_and_children() {
        let mut owned = OwnedIds::default();
        owned.extend(&["com.foo.bar.helper".to_string()]);
        assert!(owned.owns("com.foo.bar"));
        assert!(owned.owns("com.foo.bar.helper"));
        assert!(owned.owns("com.foo.bar.helper.x"));
        assert!(!owned.owns("com.foo.barx"));
        assert!(!owned.owns("com.foo.other"));
    }

    /// iPhone / iPad App 的 Wrapper 包：`Foo.app/Wrapper/Foo.app/Info.plist`，可选的 `WrappedBundle` 链接，
    /// 内层包里一个扩展。
    fn wrapper_app(dir: &Path, name: &str, id: &str, link: bool) -> PathBuf {
        let outer = dir.join(format!("{name}.app"));
        let inner = outer.join(format!("Wrapper/{name}.app"));
        let info = |path: &Path, id: &str| {
            std::fs::create_dir_all(path).unwrap();
            let mut dict = plist::Dictionary::new();
            dict.insert("CFBundleIdentifier".into(), id.into());
            plist::Value::Dictionary(dict)
                .to_file_xml(path.join("Info.plist"))
                .unwrap();
        };
        info(&inner, id);
        info(&inner.join("PlugIns/Share.appex"), &format!("{id}.share"));
        if link {
            std::os::unix::fs::symlink(format!("Wrapper/{name}.app"), outer.join("WrappedBundle"))
                .unwrap();
        }
        outer
    }

    #[test]
    fn wrapped_ios_apps_own_their_data() {
        let f = fixture();
        let ul = &f.env.user_library;
        // `/Applications` 里的（`scan_apps` 不认，`f.apps` 里没有），和只有 Spotlight 知道的。
        let reader = wrapper_app(&f.env.app_dirs[0], "Reader", "com.wrapped.reader", true);
        let other = wrapper_app(
            &f.env.home.join("Downloads"),
            "Other",
            "com.wrapped.other",
            false,
        );
        assert!(is_app_bundle(&reader) && is_app_bundle(&other));
        assert_eq!(
            wrapped_bundle(&reader),
            Some(reader.join("Wrapper/Reader.app"))
        );
        for leaf in [
            "Containers/com.wrapped.reader",
            "Containers/com.wrapped.reader.share",
            "Containers/com.wrapped.other",
        ] {
            dir(&ul.join(leaf), 100);
            age(&ul.join(leaf));
        }
        let opts = OrphanOptions {
            spotlight: true,
            fake_spotlight: Some(Some(vec![other])),
            ..opts()
        };
        let report = scan(&f.env, &f.apps, &rules_for(&f.env), &opts, &|_| {});
        assert!(!report.degraded);
        let found = by_path(&f.env, &report);
        assert!(
            found.keys().all(|p| !p.contains("com.wrapped.")),
            "{found:?}"
        );
        // 别的照常。
        assert!(found.contains_key("~/Library/Caches/com.gone.thing"));
    }

    #[test]
    fn a_wrapper_that_points_outside_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = wrapper_app(tmp.path(), "Real", "com.real.app", false);
        let fake = tmp.path().join("Fake.app");
        std::fs::create_dir_all(&fake).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("Wrapper"), fake.join("Wrapper")).unwrap();
        assert!(!is_app_bundle(&fake));
        // 链接指到 `Wrapper/` 外面的不跟，退回列 `Wrapper/`。
        let odd = wrapper_app(tmp.path(), "Odd", "com.odd.app", false);
        std::os::unix::fs::symlink("../Real.app", odd.join("WrappedBundle")).unwrap();
        assert_eq!(wrapped_bundle(&odd), Some(odd.join("Wrapper/Odd.app")));
    }

    #[test]
    fn a_security_agent_plugin_owns_its_preference_plist() {
        let f = fixture();
        let plugin = f
            .env
            .system_library
            .join("Security/SecurityAgentPlugins/Crypt.bundle");
        bundle(&plugin, "com.knightedge.encryption");
        let plist = f
            .env
            .system_library
            .join("Preferences/com.knightedge.encryption.plist");
        file(&plist);
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);
        assert!(
            !found.contains_key(&plist.display().to_string()),
            "the plugin is still on disk, so its plist is not an orphan"
        );
    }

    #[test]
    fn failed_spotlight_caps_everything_at_possible() {
        let f = fixture();
        for fake in [None, Some(Vec::new())] {
            let opts = OrphanOptions {
                spotlight: true,
                fake_spotlight: Some(fake.clone()),
                ..opts()
            };
            let report = scan(&f.env, &f.apps, &rules_for(&f.env), &opts, &|_| {});
            // 空结果当成 Spotlight 关着：`spotlight_apps` 自己会把它变成 None，这里直接给 None。
            if fake.is_some() {
                assert!(!report.degraded);
                continue;
            }
            assert!(report.degraded);
            let found = by_path(&f.env, &report);
            assert!(found.contains_key("~/Library/Caches/com.gone.thing"));
            assert!(found
                .values()
                .all(|m| m.evidence.confidence <= Confidence::Possible));
            assert!(report.suggested().is_empty());
        }
        // 旧数据里没有这个字段。
        let old: OrphanReport = serde_json::from_str(r#"{"groups":[]}"#).unwrap();
        assert!(!old.degraded);
    }

    #[test]
    fn http_storages_is_not_evidence_of_a_gui_app() {
        let f = fixture();
        let (report, _) = run(&f);
        let found = by_path(&f.env, &report);
        for path in [
            "~/Library/HTTPStorages/com.cfnet.tool",
            "~/Library/Preferences/com.cfnet.tool.plist",
        ] {
            let m = &found[path];
            assert_eq!(signal(m), OrphanSignal::BundleIdGone, "{path}");
            assert_eq!(m.evidence.confidence, Confidence::Possible, "{path}");
            assert!(!m.suggested());
        }
    }

    #[test]
    fn cancel_while_measuring_returns_nothing() {
        let f = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Mutex::new(Vec::new());
        let report = scan(
            &f.env,
            &f.apps,
            &rules_for(&f.env),
            &OrphanOptions {
                cancel: Some(cancel.clone()),
                ..opts()
            },
            &|p| {
                if matches!(p, ScanProgress::Measuring { .. }) {
                    cancel.store(true, Ordering::Relaxed);
                }
                progress.lock().unwrap().push(p);
            },
        );
        let progress = progress.into_inner().unwrap();
        assert!(progress
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { .. })));
        assert!(report.groups.is_empty());
        assert!(!progress.contains(&ScanProgress::Finished));
    }

    #[test]
    fn symlinked_locations_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env::for_test(tmp.path());
        let ul = env.user_library.clone();
        let elsewhere = tmp.path().join("elsewhere");
        dir(&elsewhere.join("Caches/com.gone.link"), 100);
        file(&elsewhere.join("Prefs/com.gone.pref.plist"));
        file(&elsewhere.join("Prefs/ByHost/com.gone.byhost.plist"));
        dir(&ul.join("Containers/com.gone.real"), 100);
        std::os::unix::fs::symlink(elsewhere.join("Caches"), ul.join("Caches")).unwrap();
        // `Preferences/ByHost` 自己不是链接，但它上一级是。
        std::os::unix::fs::symlink(elsewhere.join("Prefs"), ul.join("Preferences")).unwrap();
        age(&elsewhere);
        age(&ul);
        let rules = RuleSet {
            rules: vec![Rule {
                bundle_id: "com.example.linked".into(),
                name: "Linked".into(),
                aliases: vec![],
                paths: vec![RulePath {
                    path: "~/Library/Caches/com.gone.link".into(),
                    confidence: Confidence::Certain,
                    note: None,
                }],
            }],
            cask: None,
        };
        let report = scan(&env, &[], &rules, &opts(), &|_| {});
        let found = by_path(&env, &report);
        let paths: Vec<&str> = found.keys().map(String::as_str).collect();
        assert_eq!(paths, ["~/Library/Containers/com.gone.real"]);
        let mut listings = Listings::new();
        assert!(real_path(&env, &ul.join("Caches"), false, &mut listings).is_none());
        assert!(real_path(&env, &ul.join("Preferences/ByHost"), false, &mut listings).is_none());
        // 最后一级允许是链接（移走的是链接本身）。
        assert_eq!(
            real_path(&env, &ul.join("Caches"), true, &mut listings),
            Some(ul.join("Caches"))
        );
    }

    #[test]
    fn rule_paths_use_the_case_on_disk() {
        let f = fixture();
        let lower = f.env.user_library.join("Application Support/ruled data");
        if !lower.exists() {
            // 区分大小写的卷上没有这个问题。
            return;
        }
        let mut rules = rules_for(&f.env);
        rules.rules[0].paths[0].path = "~/Library/Application Support/ruled data".into();
        let report = scan(&f.env, &f.apps, &rules, &opts(), &|_| {});
        let found = by_path(&f.env, &report);
        let ruled: Vec<&String> = found
            .keys()
            .filter(|p| p.to_lowercase().ends_with("/ruled data"))
            .collect();
        assert_eq!(ruled, ["~/Library/Application Support/Ruled Data"]);
        assert_eq!(
            signal(&found["~/Library/Application Support/Ruled Data"]),
            OrphanSignal::KnownAppGone
        );
        let mut listings = Listings::new();
        assert_eq!(
            real_path(&f.env, &lower, true, &mut listings),
            Some(f.env.user_library.join("Application Support/Ruled Data"))
        );
    }
}
