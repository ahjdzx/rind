//! 在每个 App 会留东西的地方列出候选，交给 matcher 判断，再度量、标保护、找共用。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rayon::prelude::*;

use crate::matcher::Claimant;
use crate::rules::{pattern_matches, CaskHit, RuleSet};
use crate::{
    AppRef, AppSource, Confidence, Env, Evidence, EvidenceKind, HoldBack, InstalledApp,
    LeftoverKind, LeftoverMatch, ScanProgress, Size,
};

/// 一个会被搜索的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchLocation {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    /// 往下看几层：1 = 只看直接子项。
    pub depth: u8,
}

/// 全部搜索位置（存在的才返回）。
pub fn search_locations(env: &Env) -> Vec<SearchLocation> {
    use LeftoverKind::*;
    let ul = &env.user_library;
    let sl = &env.system_library;
    let mut all: Vec<(PathBuf, LeftoverKind, u8)> = vec![
        (ul.join("Application Support"), AppSupport, 2),
        (ul.join("Caches"), Cache, 2),
        (ul.join("Preferences"), Preferences, 1),
        (ul.join("Preferences/ByHost"), Preferences, 1),
        (ul.join("Containers"), Container, 1),
        (ul.join("Group Containers"), GroupContainer, 1),
        (ul.join("LaunchAgents"), LaunchAgent, 1),
        (ul.join("Saved Application State"), SavedState, 1),
        (ul.join("Logs"), Log, 2),
        (ul.join("HTTPStorages"), WebData, 1),
        (ul.join("WebKit"), WebData, 1),
        (ul.join("Cookies"), Cookies, 1),
        (ul.join("Application Scripts"), ApplicationScripts, 1),
        (ul.clone(), Other, 1),
        (sl.join("Application Support"), AppSupport, 2),
        (sl.join("Caches"), Cache, 1),
        (sl.join("Preferences"), Preferences, 1),
        (sl.join("LaunchAgents"), LaunchAgent, 1),
        (sl.join("LaunchDaemons"), LaunchDaemon, 1),
        (sl.join("PrivilegedHelperTools"), Other, 1),
        (env.receipts_dir.clone(), Receipt, 1),
        (env.home.clone(), HomeDotfile, 1),
    ];
    if let Some(prefix) = &env.homebrew_prefix {
        all.push((prefix.join("Caskroom"), HomebrewCask, 1));
    }
    all.into_iter()
        .filter(|(path, _, _)| path.is_dir())
        .map(|(path, kind, depth)| SearchLocation { path, kind, depth })
        .collect()
}

/// 一个还没判断归属的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    pub is_dir: bool,
    /// 是搜索位置下第二层的东西（上一层目录没认出来才看到它）。名字上的证据对它要求更严。
    pub nested: bool,
    /// launchd plist 运行的程序，列候选时读一次，之后每个 App 判断都用它。
    pub program: Option<PathBuf>,
}

impl Candidate {
    pub fn new(path: PathBuf, kind: LeftoverKind, is_dir: bool, nested: bool) -> Candidate {
        let is_job = matches!(kind, LeftoverKind::LaunchAgent | LeftoverKind::LaunchDaemon)
            && path.extension().is_some_and(|e| e == "plist");
        let program = if is_job {
            crate::matcher::launchd_program(&path)
        } else {
            None
        };
        Candidate {
            path,
            kind,
            is_dir,
            nested,
            program,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// 要不要度量大小（CLI 的 `--no-size` 会关掉）。
    pub measure: bool,
    /// 每一项的度量时间预算。
    pub size_budget: Duration,
    /// 置为 true 就尽快停下（GUI 换了 App 时）。停下的扫描结果不完整，调用方应丢弃。
    pub cancel: Option<std::sync::Arc<AtomicBool>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            measure: true,
            size_budget: Duration::from_secs(20),
            cancel: None,
        }
    }
}

/// 另一个已安装的 App：它怎么认领文件，和它的 cask 点名了什么。
struct Other<'a> {
    app_ref: AppRef,
    claimant: Claimant<'a>,
    /// 已替换变量的模式，和各自是不是 `rmdir`。
    cask_patterns: Vec<(String, bool)>,
}

/// 完整扫描：列候选 → `matcher` → 用 `all_apps` 找 rivals → `protect::check` →
/// `size::measure_all`。结果按位置、再按路径排序。取消后返回空。
pub fn scan(
    env: &Env,
    app: &InstalledApp,
    all_apps: &[InstalledApp],
    rules: &RuleSet,
    opts: &ScanOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Vec<LeftoverMatch> {
    let cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    };
    let app = verified_identity(app);
    let app: &InstalledApp = &app;
    let claimant = Claimant::new(app, rules);

    let locations = search_locations(env);
    let mut candidates: BTreeMap<PathBuf, Candidate> = BTreeMap::new();
    for location in &locations {
        if cancelled() {
            return Vec::new();
        }
        on_progress(ScanProgress::Location(env.display(&location.path)));
        for candidate in list_location(env, &claimant, rules, location) {
            candidates
                .entry(candidate.path.clone())
                .or_insert(candidate);
        }
    }

    // 规则和 cask 点名的路径，不管搜索位置有没有列到。
    for path in rules.declared_paths(env, app) {
        let kind = kind_for_path(env, &locations, &path);
        candidates
            .entry(path.clone())
            .or_insert_with(|| Candidate::new(path.clone(), kind, path.is_dir(), false));
    }
    let mut cask_hits: HashMap<PathBuf, CaskHit> = HashMap::new();
    if let Some(index) = &rules.cask {
        for hit in index.paths_for(env, app) {
            let kind = kind_for_path(env, &locations, &hit.path);
            candidates.entry(hit.path.clone()).or_insert_with(|| {
                Candidate::new(hit.path.clone(), kind, hit.path.is_dir(), false)
            });
            cask_hits.insert(hit.path.clone(), hit);
        }
    }

    let others: Vec<Other> = all_apps
        .iter()
        .filter(|other| other.path != app.path)
        .map(|other| Other {
            app_ref: other.as_ref(),
            claimant: Claimant::new(other, rules),
            cask_patterns: rules
                .cask
                .as_ref()
                .map(|index| index.patterns_for(env, other))
                .unwrap_or_default(),
        })
        .collect();

    let candidates: Vec<Candidate> = candidates.into_values().collect();
    let mut matches: Vec<LeftoverMatch> = candidates
        .par_iter()
        .filter_map(|candidate| {
            if cancelled() {
                return None;
            }
            let evidence = judge(env, &claimant, candidate, rules, &cask_hits)?;
            let mut claims = rivals_for(env, &others, candidate, rules, &evidence);
            // 另一个 App 用自己的 id 拿到 certain、我们却没那么肯定的，是那个 App 的，不列。
            if evidence.confidence < Confidence::Certain
                && claims.iter().any(|(_, c)| *c == Confidence::Certain)
            {
                return None;
            }
            // 只凭名字、厂商或 cask 沾边的目录，看看里面有没有别人的东西。
            if candidate.is_dir && is_loose(&evidence) {
                claims.extend(children_claims(env, &others, candidate, rules));
            }
            let mut rivals: Vec<AppRef> = Vec::new();
            for (app_ref, _) in claims {
                if !rivals.contains(&app_ref) {
                    rivals.push(app_ref);
                }
            }
            let hold_back = hold_back_for(env, &candidate.path, &rivals);
            Some(LeftoverMatch {
                path: candidate.path.clone(),
                kind: candidate.kind,
                size: Size::Unknown,
                evidence,
                rivals,
                hold_back,
            })
        })
        .collect();
    if cancelled() {
        return Vec::new();
    }

    // 父目录已经算进来、而且没被挡（会替用户选上）的，子项不再单列；
    // 父目录被挡或共用的，子项留着让用户单独处理。
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    let mut kept: Vec<LeftoverMatch> = Vec::with_capacity(matches.len());
    for m in matches {
        if kept
            .last()
            .is_some_and(|last| m.path.starts_with(&last.path) && last.hold_back.is_none())
        {
            continue;
        }
        kept.push(m);
    }
    let mut matches = kept;

    if opts.measure && !matches.is_empty() {
        let paths: Vec<PathBuf> = matches.iter().map(|m| m.path.clone()).collect();
        let cancel = opts.cancel.as_deref();
        let sizes = crate::size::measure_all_with(&paths, opts.size_budget, cancel, on_progress);
        for (m, size) in matches.iter_mut().zip(sizes) {
            m.size = size;
        }
        if cancelled() {
            return Vec::new();
        }
    }

    matches.sort_by(|a, b| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| a.path.cmp(&b.path))
    });
    on_progress(ScanProgress::Finished);
    matches
}

/// Team ID 和 App Group 来自 `codesign` 的显示输出，谁都能自称；只有签名链到 Apple 的才信。
/// 没签名的 App 本来就拿不到这两样，不用验。
fn verified_identity(app: &InstalledApp) -> Cow<'_, InstalledApp> {
    if app.team_id.is_none() && app.app_groups.is_empty() {
        return Cow::Borrowed(app);
    }
    if !app.path.join("Contents/_CodeSignature").exists() {
        return Cow::Borrowed(app);
    }
    if crate::apps::signature_is_trusted(&app.path) {
        Cow::Borrowed(app)
    } else {
        Cow::Owned(InstalledApp {
            team_id: None,
            app_groups: Vec::new(),
            ..app.clone()
        })
    }
}

/// 一个位置里的候选。两层的位置里，一层目录像厂商目录（`Google`、`JetBrains`）、又不是 App 自己的，再看它的子项。
fn list_location(
    env: &Env,
    claimant: &Claimant,
    rules: &RuleSet,
    location: &SearchLocation,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (path, is_dir) in list_entries(&location.path, usize::MAX) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if location.kind == LeftoverKind::HomeDotfile && !name.starts_with('.') {
            continue;
        }
        if location.kind == LeftoverKind::Other && location.path == env.user_library && is_dir {
            // `~/Library` 自己的一层目录是搜索位置，不是候选；只看直接放在这里的别的东西。
            if search_locations_names().contains(&name.as_str()) {
                continue;
            }
        }
        let candidate = Candidate::new(path.clone(), location.kind, is_dir, false);
        let owned = claimant.evidence(env, &candidate, rules).is_some();
        out.push(candidate);
        if location.depth >= 2
            && is_dir
            && !owned
            && !name.starts_with("com.apple.")
            && !crate::protect::is_system_owned(env, &path)
            && claimant.is_vendor_dir(&name)
        {
            out.extend(
                list_entries(&path, 500)
                    .into_iter()
                    .map(|(child, child_is_dir)| {
                        Candidate::new(child, location.kind, child_is_dir, true)
                    }),
            );
        }
    }
    out
}

fn search_locations_names() -> [&'static str; 12] {
    [
        "Application Support",
        "Caches",
        "Preferences",
        "Containers",
        "Group Containers",
        "LaunchAgents",
        "Saved Application State",
        "Logs",
        "HTTPStorages",
        "WebKit",
        "Application Scripts",
        "Cookies",
    ]
}

/// 目录的直接子项（路径，是不是目录；符号链接不算目录），最多 `limit` 个，超过就一个都不给。
pub(crate) fn list_entries(dir: &Path, limit: usize) -> Vec<(PathBuf, bool)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        if name == ".DS_Store" || name == ".localized" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((entry.path(), is_dir));
        if out.len() > limit {
            return Vec::new();
        }
    }
    out
}

/// 规则 / cask 点名的路径属于哪一类：看它落在哪个搜索位置下。
pub(crate) fn kind_for_path(env: &Env, locations: &[SearchLocation], path: &Path) -> LeftoverKind {
    let mut best: Option<&SearchLocation> = None;
    for location in locations {
        if path.starts_with(&location.path)
            && path != location.path
            && best.is_none_or(|b| location.path.components().count() > b.path.components().count())
        {
            best = Some(location);
        }
    }
    match best {
        Some(location) if location.kind == LeftoverKind::HomeDotfile => {
            let dotfile = path.parent() == Some(env.home.as_path())
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if dotfile {
                LeftoverKind::HomeDotfile
            } else {
                LeftoverKind::Other
            }
        }
        Some(location) => location.kind,
        None => LeftoverKind::Other,
    }
}

/// matcher 的证据，加上 cask 的 zap 段、Caskroom 里 cask 自己的文件夹，和收据。
fn judge(
    env: &Env,
    claimant: &Claimant,
    candidate: &Candidate,
    rules: &RuleSet,
    cask_hits: &HashMap<PathBuf, CaskHit>,
) -> Option<Evidence> {
    let mut best = claimant.evidence(env, candidate, rules);
    if let Some(hit) = cask_hits.get(&candidate.path) {
        // cask 点名的路径只是线索：名字里有这个 App 的才算 likely，`Google` 这种厂商目录只算 possible；
        // 按 `.app` 文件名猜到的 cask 更只算 possible。
        let name = candidate
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let confidence = if hit.trusted && claimant.name_mentions_app(&name) {
            Confidence::Likely
        } else {
            Confidence::Possible
        };
        let cask = Evidence {
            kind: EvidenceKind::CaskZap {
                token: hit.token.clone(),
            },
            confidence,
            matched: hit.token.clone(),
        };
        if best.as_ref().is_none_or(|b| cask.confidence > b.confidence) {
            best = Some(cask);
        }
    }
    if candidate.kind == LeftoverKind::HomebrewCask {
        if let (AppSource::Homebrew(token), Some(prefix)) =
            (&claimant.app.source, &env.homebrew_prefix)
        {
            if candidate.path == prefix.join("Caskroom").join(token) {
                best = Some(Evidence {
                    kind: EvidenceKind::CaskZap {
                        token: token.clone(),
                    },
                    confidence: Confidence::Certain,
                    matched: token.clone(),
                });
            }
        }
    }
    if candidate.kind == LeftoverKind::Receipt {
        if let Some(evidence) = best.as_mut() {
            if evidence.kind == EvidenceKind::BundleId {
                evidence.kind = EvidenceKind::Receipt;
            }
        }
    }
    best
}

/// 只凭名字、厂商、Team 或 cask 沾边，或者把握不到 certain 的证据：目录里可能有别人的东西。
fn is_loose(evidence: &Evidence) -> bool {
    evidence.confidence < Confidence::Certain
        || matches!(
            evidence.kind,
            EvidenceKind::Name
                | EvidenceKind::BundleIdPrefix
                | EvidenceKind::TeamId
                | EvidenceKind::CaskZap { .. }
        )
}

/// 其它也拿到 likely 以上证据的已安装 App（包括同一个 App 的另一份拷贝），和各自的把握。
/// 别的 App 的 cask 点名了它，也算认领。
fn rivals_for(
    env: &Env,
    others: &[Other],
    candidate: &Candidate,
    rules: &RuleSet,
    ours: &Evidence,
) -> Vec<(AppRef, Confidence)> {
    others
        .iter()
        .filter_map(|other| claim_of(env, other, candidate, rules, Some(ours)))
        .collect()
}

fn claim_of(
    env: &Env,
    other: &Other,
    candidate: &Candidate,
    rules: &RuleSet,
    ours: Option<&Evidence>,
) -> Option<(AppRef, Confidence)> {
    let by_evidence = other
        .claimant
        .evidence(env, candidate, rules)
        // 我们用 id 认定的东西，别人只凭 Team 沾边不算。
        .filter(|e| {
            !(e.kind == EvidenceKind::TeamId
                && ours.is_some_and(|o| o.confidence == Confidence::Certain))
        })
        .map(|e| e.confidence)
        .filter(|c| *c >= Confidence::Likely);
    let by_cask = || {
        other
            .cask_patterns
            .iter()
            .any(|(p, _)| pattern_matches(p, &candidate.path))
            .then_some(Confidence::Likely)
    };
    by_evidence
        .or_else(by_cask)
        .map(|c| (other.app_ref.clone(), c))
}

/// 目录的直接子项里，被别的 App 认领的。最多看 200 项。
fn children_claims(
    env: &Env,
    others: &[Other],
    candidate: &Candidate,
    rules: &RuleSet,
) -> Vec<(AppRef, Confidence)> {
    let mut out = Vec::new();
    for (child, is_dir) in list_entries(&candidate.path, 200) {
        let name = child
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with("com.apple.") {
            continue;
        }
        let child = Candidate::new(child, candidate.kind, is_dir, true);
        for other in others {
            if let Some(claim) = claim_of(env, other, &child, rules, None) {
                out.push(claim);
            }
        }
    }
    out
}

pub(crate) fn hold_back_for(env: &Env, path: &Path, rivals: &[AppRef]) -> Option<HoldBack> {
    if let Some(why) = crate::protect::check(env, path) {
        return Some(HoldBack::Protected { why });
    }
    if crate::protect::is_system_owned(env, path) {
        return Some(HoldBack::SystemOwned);
    }
    // SAFETY: geteuid 没有参数，总是成功。
    if needs_admin_as(path, unsafe { libc::geteuid() }) {
        return Some(HoldBack::NeedsAdmin);
    }
    if !rivals.is_empty() {
        return Some(HoldBack::SharedWith {
            apps: rivals.to_vec(),
        });
    }
    if !accessible(path, libc::R_OK) {
        return Some(HoldBack::Unreadable);
    }
    None
}

/// 这个账号（`me`）动不了它：不是它的所有者；或者父目录动不了（写不了，或者带 sticky 位又不是自己的）。
pub(crate) fn needs_admin_as(path: &Path, me: u32) -> bool {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() != me) {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    match std::fs::metadata(parent) {
        Ok(meta) => {
            let sticky = meta.mode() & u32::from(libc::S_ISVTX) != 0;
            (sticky && meta.uid() != me) || !accessible(parent, libc::W_OK)
        }
        Err(_) => true,
    }
}

pub(crate) fn accessible(path: &Path, mode: libc::c_int) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `c` 是合法的 C 字符串，access 只读它。
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn kind_rank(kind: LeftoverKind) -> u8 {
    use LeftoverKind::*;
    match kind {
        AppSupport => 0,
        Container => 1,
        GroupContainer => 2,
        Cache => 3,
        Preferences => 4,
        SavedState => 5,
        Log => 6,
        WebData => 7,
        Cookies => 8,
        ApplicationScripts => 9,
        LaunchAgent => 10,
        LaunchDaemon => 11,
        Receipt => 12,
        HomebrewCask => 13,
        HomeDotfile => 14,
        Other => 15,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::scan_apps;
    use crate::apps::testing::{make_app, make_embedded};
    use crate::rules::{CaskIndex, Rule, RulePath};
    use crate::{ProtectReason, Size};
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _dir: tempfile::TempDir,
        env: Env,
        apps: Vec<InstalledApp>,
    }

    fn mk(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("data.bin"), vec![0u8; 8192]).unwrap();
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let apps_dir = env.app_dirs[0].clone();
        let foo = make_app(&apps_dir, "Foo", "com.acme.foo");
        make_embedded(&foo, "PlugIns", "Share.appex", "com.acme.foo.share");
        make_app(&apps_dir, "Foo Bar", "com.other.foobar");
        make_app(&apps_dir, "Acme Sibling", "com.acme.sibling"); // 同厂商的另一个 App
        make_app(&env.system_app_dirs[0], "Mail", "com.apple.mail");

        let ul = &env.user_library;
        mk(&ul.join("Application Support/com.acme.foo"));
        mk(&ul.join("Application Support/Foo"));
        mk(&ul.join("Application Support/Foo/Nested Cache")); // 父目录已匹配，子项不单列
        mk(&ul.join("Application Support/Acme/foo-data")); // 厂商目录里：名字只是开头像，不算
        mk(&ul.join("Application Support/Acme/Foo")); // 厂商目录里：名字完全相等才算
        mk(&ul.join("Application Support/Electron Thing/Foo Cache")); // 不是厂商目录：不往下看
        mk(&ul.join("Application Support/Electron Thing/Foo")); // 不是厂商目录：就算名字相等也看不到
        mk(&ul.join("Application Support/Knowledge/com.acme.foo")); // 系统目录，不往下看
        mk(&ul.join("Application Support/Unrelated"));
        mk(&ul.join("Caches/com.acme.foo"));
        mk(&ul.join("Caches/com.apple.foo"));
        mk(&ul.join("Caches/com.acme.sibling")); // 同厂商，但 Acme Sibling 用自己的 id 认领 → 不列
        mk(&ul.join("Caches/Foo Bar")); // Foo 只算 possible，Foo Bar 算 likely → rival
        mk(&ul.join("Containers/com.acme.foo.share"));
        mk(&ul.join("Group Containers/group.com.acme.foo"));
        std::fs::create_dir_all(ul.join("Preferences/ByHost")).unwrap();
        std::fs::write(ul.join("Preferences/com.acme.foo.plist"), b"x").unwrap();
        std::fs::write(ul.join("Preferences/com.acme.other.plist"), b"x").unwrap();
        std::fs::write(
            ul.join("Preferences/ByHost/com.acme.foo.0F3A1B2C-1234-5678-9ABC-DEF012345678.plist"),
            b"x",
        )
        .unwrap();
        mk(&ul.join("Saved Application State/com.acme.foo.savedState"));
        std::fs::create_dir_all(ul.join("Cookies")).unwrap();
        std::fs::write(ul.join("Cookies/com.acme.foo.binarycookies"), b"x").unwrap();
        std::fs::write(ul.join("Cookies/Cookies.binarycookies"), b"x").unwrap();
        mk(&ul.join("Mobile Documents/com~acme~foo")); // iCloud：保护区
        mk(&env.home.join(".foo")); // 只有规则点名才会进来
        std::fs::create_dir_all(ul.join("LaunchAgents")).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "Program".into(),
            foo.join("Contents/MacOS/Foo").display().to_string().into(),
        );
        plist::Value::Dictionary(dict)
            .to_file_xml(ul.join("LaunchAgents/com.whatever.plist"))
            .unwrap();
        std::fs::create_dir_all(&env.receipts_dir).unwrap();
        std::fs::write(env.receipts_dir.join("com.acme.foo.pkg.bom"), b"x").unwrap();

        let apps = scan_apps(&env);
        Fixture {
            _dir: dir,
            env,
            apps,
        }
    }

    fn foo(f: &Fixture) -> InstalledApp {
        let mut foo = f
            .apps
            .iter()
            .find(|a| a.name == "Foo" && a.path.starts_with(&f.env.app_dirs[0]))
            .unwrap()
            .clone();
        foo.app_groups = vec!["group.com.acme.foo".into()];
        foo
    }

    fn rules() -> RuleSet {
        RuleSet {
            rules: vec![Rule {
                bundle_id: "com.acme.foo".into(),
                name: "Foo".into(),
                aliases: vec![],
                paths: vec![
                    RulePath {
                        path: "~/.foo".into(),
                        confidence: Confidence::Likely,
                        note: None,
                    },
                    RulePath {
                        path: "~/Library/Mobile Documents/com~acme~foo".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    },
                ],
            }],
            cask: None,
        }
    }

    fn rel(env: &Env, m: &LeftoverMatch) -> String {
        env.display(&m.path)
    }

    fn index_by_path<'a>(
        env: &Env,
        matches: &'a [LeftoverMatch],
    ) -> BTreeMap<String, &'a LeftoverMatch> {
        matches.iter().map(|m| (rel(env, m), m)).collect()
    }

    #[test]
    fn scan_finds_explains_measures_and_holds_back() {
        let f = fixture();
        let app = foo(&f);
        let progress = std::sync::Mutex::new(Vec::new());
        let matches = scan(
            &f.env,
            &app,
            &f.apps,
            &rules(),
            &ScanOptions::default(),
            &|p| progress.lock().unwrap().push(p),
        );

        let by_path = index_by_path(&f.env, &matches);
        let paths: Vec<String> = by_path.keys().cloned().collect();
        let receipt = format!(
            "{}/com.acme.foo.pkg.bom",
            f.env.display(&f.env.receipts_dir)
        );
        let mut expected: Vec<String> = [
            "~/.foo",
            "~/Library/Application Support/Acme/Foo",
            "~/Library/Application Support/Foo",
            "~/Library/Application Support/com.acme.foo",
            "~/Library/Caches/Foo Bar",
            "~/Library/Caches/com.acme.foo",
            "~/Library/Containers/com.acme.foo.share",
            "~/Library/Cookies/com.acme.foo.binarycookies",
            "~/Library/Group Containers/group.com.acme.foo",
            "~/Library/LaunchAgents/com.whatever.plist",
            "~/Library/Mobile Documents/com~acme~foo",
            "~/Library/Preferences/ByHost/com.acme.foo.0F3A1B2C-1234-5678-9ABC-DEF012345678.plist",
            "~/Library/Preferences/com.acme.foo.plist",
            "~/Library/Preferences/com.acme.other.plist",
            "~/Library/Saved Application State/com.acme.foo.savedState",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        expected.push(receipt.clone());
        expected.sort();
        assert_eq!(paths, expected);

        let m = by_path["~/Library/Application Support/com.acme.foo"];
        assert_eq!(m.evidence.kind, EvidenceKind::BundleId);
        assert_eq!(m.kind, LeftoverKind::AppSupport);
        assert!(
            matches!(m.size, Size::Known(n) if n >= 8192),
            "{:?}",
            m.size
        );
        assert!(m.suggested());

        assert!(
            by_path["~/Library/Application Support/Foo"].suggested(),
            "a name equal to the app's is likely"
        );
        assert_eq!(
            by_path["~/Library/Application Support/Acme/Foo"]
                .evidence
                .confidence,
            Confidence::Likely,
            "a nested name equal to the app's, inside the maker's folder"
        );
        assert_eq!(
            by_path["~/Library/Containers/com.acme.foo.share"]
                .evidence
                .kind,
            EvidenceKind::EmbeddedBundleId
        );
        assert_eq!(
            by_path["~/Library/Group Containers/group.com.acme.foo"]
                .evidence
                .kind,
            EvidenceKind::AppGroup
        );
        assert_eq!(
            by_path["~/Library/LaunchAgents/com.whatever.plist"]
                .evidence
                .kind,
            EvidenceKind::LaunchdProgram
        );
        assert_eq!(
            by_path["~/Library/Preferences/com.acme.other.plist"]
                .evidence
                .confidence,
            Confidence::Possible
        );
        assert_eq!(
            by_path["~/Library/Cookies/com.acme.foo.binarycookies"].kind,
            LeftoverKind::Cookies
        );
        assert_eq!(by_path[&receipt].evidence.kind, EvidenceKind::Receipt);
        assert_eq!(by_path[&receipt].evidence.confidence, Confidence::Certain);
        assert_eq!(by_path["~/.foo"].kind, LeftoverKind::HomeDotfile);
        assert!(
            by_path["~/.foo"].suggested(),
            "a rule's likely path with no rivals"
        );

        let shared = by_path["~/Library/Caches/Foo Bar"];
        assert_eq!(shared.rivals.len(), 1);
        assert_eq!(
            shared.rivals[0].bundle_id.as_deref(),
            Some("com.other.foobar")
        );
        assert!(matches!(
            shared.hold_back,
            Some(HoldBack::SharedWith { .. })
        ));
        assert!(!shared.suggested());
        assert!(shared.selectable());

        let icloud = by_path["~/Library/Mobile Documents/com~acme~foo"];
        assert_eq!(
            icloud.hold_back,
            Some(HoldBack::Protected {
                why: ProtectReason::ICloudDrive
            })
        );
        assert!(!icloud.selectable());

        for m in &matches {
            assert!(
                !m.path.ends_with("Nested Cache"),
                "children of a match are folded into it"
            );
            assert!(
                !m.path.ends_with("com.acme.sibling"),
                "named for another app of the maker: not listed"
            );
            assert!(
                !m.path.ends_with("Cookies.binarycookies"),
                "Safari's cookies are protected and not Foo's anyway"
            );
        }

        let progress = progress.lock().unwrap();
        assert!(progress
            .iter()
            .any(|p| matches!(p, ScanProgress::Location(l) if l == "~/Library/Caches")));
        assert!(progress
            .iter()
            .any(|p| matches!(p, ScanProgress::Measuring { .. })));
        assert_eq!(progress.last(), Some(&ScanProgress::Finished));

        // 排序：先按种类，再按路径。
        let kinds: Vec<u8> = matches.iter().map(|m| kind_rank(m.kind)).collect();
        assert!(kinds.windows(2).all(|w| w[0] <= w[1]), "{kinds:?}");
    }

    #[test]
    fn another_copy_of_the_app_shares_everything() {
        let f = fixture();
        let copy = make_app(&f.env.home.join("Applications"), "Foo", "com.acme.foo");
        let apps = scan_apps(&f.env);
        let app = foo(&f);
        let matches = scan(
            &f.env,
            &app,
            &apps,
            &rules(),
            &ScanOptions::default(),
            &|_| {},
        );
        assert!(!matches.is_empty());
        for m in &matches {
            if m.hold_back
                == Some(HoldBack::Protected {
                    why: ProtectReason::ICloudDrive,
                })
            {
                continue;
            }
            if m.evidence.kind == EvidenceKind::LaunchdProgram {
                // 这个任务运行的是被卸载那份里的程序，是它的，另一份不认领。
                assert!(m.suggested());
                continue;
            }
            if m.evidence.confidence >= Confidence::Likely {
                assert!(
                    m.rivals.iter().any(|r| r.path == copy),
                    "the other copy still uses {}",
                    m.path.display()
                );
                assert!(
                    matches!(m.hold_back, Some(HoldBack::SharedWith { .. })),
                    "{}: {:?}",
                    m.path.display(),
                    m.hold_back
                );
            }
            assert!(
                !m.suggested(),
                "nothing is pre-selected while another copy is installed: {}",
                m.path.display()
            );
        }
    }

    #[test]
    fn without_measuring_sizes_are_unknown_and_nothing_is_suggested() {
        let f = fixture();
        let app = foo(&f);
        let opts = ScanOptions {
            measure: false,
            size_budget: Duration::from_secs(1),
            cancel: None,
        };
        let matches = scan(&f.env, &app, &f.apps, &RuleSet::default(), &opts, &|_| {});
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|m| m.size == Size::Unknown));
        assert!(matches.iter().all(|m| !m.suggested()));
    }

    #[test]
    fn an_app_with_nothing_left_has_no_matches() {
        let f = fixture();
        let mut app = foo(&f);
        app.name = "Zzz".into();
        app.bundle_id = Some("com.nobody.zzz".into());
        app.embedded_ids.clear();
        app.app_groups.clear();
        app.path = f.env.app_dirs[0].join("Zzz.app");
        let matches = scan(
            &f.env,
            &app,
            &f.apps,
            &RuleSet::default(),
            &ScanOptions::default(),
            &|_| {},
        );
        assert!(matches.is_empty(), "{matches:?}");
    }

    fn write_cask_index(env: &Env, casks: serde_json::Value) {
        let api = env.user_library.join("Caches/Homebrew/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(
            api.join("cask.jws.json"),
            serde_json::json!({"payload": casks.to_string()}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn cask_paths_are_hints_and_the_caskroom_folder_counts() {
        let f = fixture();
        let mut env = f.env.clone();
        let prefix = f._dir.path().join("opt/homebrew");
        std::fs::create_dir_all(prefix.join("Caskroom/foo/1.0/Foo.app")).unwrap();
        env.homebrew_prefix = Some(prefix.clone());
        mk(&env.user_library.join("Caches/Foo Extra"));
        mk(&env.home.join("Music/Foo"));
        write_cask_index(
            &env,
            serde_json::json!([{"token": "foo", "artifacts": [
                {"app": ["Foo.app"]},
                {"zap": [{"trash": [
                    "~/Library/Application Support/Unrelated",
                    "~/Library/Caches/Foo Extra",
                    "~/Music/Foo"
                ], "rmdir": ["~/Library/Application Support/Acme"]}]}
            ]}]),
        );
        let rules = RuleSet {
            rules: vec![],
            cask: CaskIndex::load(&env),
        };
        assert!(rules.cask.is_some());

        let mut app = foo(&f);
        app.source = AppSource::Homebrew("foo".into());
        let matches = scan(
            &env,
            &app,
            &f.apps,
            &rules,
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&env, &matches);
        let unrelated = by_path["~/Library/Application Support/Unrelated"];
        assert_eq!(
            unrelated.evidence.kind,
            EvidenceKind::CaskZap {
                token: "foo".into()
            }
        );
        assert_eq!(
            unrelated.evidence.confidence,
            Confidence::Possible,
            "named by the cask but nothing in the name says Foo"
        );
        assert!(!unrelated.suggested());
        let extra = by_path["~/Library/Caches/Foo Extra"];
        assert_eq!(
            extra.evidence.confidence,
            Confidence::Likely,
            "named by the cask and named for Foo"
        );
        assert!(extra.suggested());
        assert!(
            !by_path.contains_key("~/Music/Foo"),
            "a cask may not reach outside Library"
        );
        assert!(
            !by_path.contains_key("~/Library/Application Support/Acme"),
            "rmdir of a folder with things in it is ignored"
        );
        let caskroom = matches
            .iter()
            .find(|m| m.path == prefix.join("Caskroom/foo"))
            .expect("the cask's own folder");
        assert_eq!(caskroom.kind, LeftoverKind::HomebrewCask);
        assert_eq!(caskroom.evidence.confidence, Confidence::Certain);

        // 不是 Homebrew 装的，只按 `.app` 文件名猜到 cask：最多 possible。
        let guessed = foo(&f);
        let matches = scan(
            &env,
            &guessed,
            &f.apps,
            &rules,
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&env, &matches);
        assert_eq!(
            by_path["~/Library/Caches/Foo Extra"].evidence.confidence,
            Confidence::Possible
        );
        assert!(!by_path.contains_key("~/Library/Caches/Foo Extra/data.bin"));
    }

    #[test]
    fn a_vendor_folder_with_another_apps_things_in_it_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let apps_dir = env.app_dirs[0].clone();
        make_app(&apps_dir, "JetBrains Toolbox", "com.jetbrains.toolbox");
        make_app(&apps_dir, "IntelliJ IDEA", "com.jetbrains.intellij");
        make_app(&apps_dir, "Google Chrome", "com.google.Chrome");
        make_app(&apps_dir, "Google Drive", "com.google.drivefs");
        let ul = &env.user_library;
        mk(&ul.join("Application Support/JetBrains/com.jetbrains.toolbox"));
        mk(&ul.join("Application Support/JetBrains/IntelliJIdea2024.2"));
        mk(&ul.join("Application Support/Google/Chrome"));
        mk(&ul.join("Application Support/Google/com.google.drivefs"));
        write_cask_index(
            &env,
            serde_json::json!([
                {"token": "jetbrains-toolbox", "artifacts": [{"app": ["JetBrains Toolbox.app"]},
                    {"zap": [{"trash": ["~/Library/Application Support/JetBrains"]}]}]},
                {"token": "google-chrome", "artifacts": [{"app": ["Google Chrome.app"]},
                    {"zap": [{"trash": ["~/Library/Application Support/Google"]}]}]}
            ]),
        );
        let rules = RuleSet {
            rules: vec![
                Rule {
                    bundle_id: "com.jetbrains.intellij".into(),
                    name: "IntelliJ IDEA".into(),
                    aliases: vec![],
                    paths: vec![RulePath {
                        path: "~/Library/Application Support/JetBrains/IntelliJIdea*".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    }],
                },
                Rule {
                    bundle_id: "com.google.Chrome".into(),
                    name: "Google Chrome".into(),
                    aliases: vec!["Chrome".into()],
                    paths: vec![],
                },
            ],
            cask: CaskIndex::load(&env),
        };
        let mut apps = scan_apps(&env);
        for app in apps.iter_mut() {
            match app.name.as_str() {
                "JetBrains Toolbox" => app.source = AppSource::Homebrew("jetbrains-toolbox".into()),
                "Google Chrome" => app.source = AppSource::Homebrew("google-chrome".into()),
                _ => {}
            }
        }

        let toolbox = apps.iter().find(|a| a.name == "JetBrains Toolbox").unwrap();
        let matches = scan(
            &env,
            toolbox,
            &apps,
            &rules,
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&env, &matches);
        let jetbrains = by_path["~/Library/Application Support/JetBrains"];
        assert_eq!(jetbrains.evidence.confidence, Confidence::Possible);
        assert!(
            jetbrains.rivals.iter().any(|r| r.name == "IntelliJ IDEA"),
            "IntelliJ keeps its settings in there: {:?}",
            jetbrains.rivals
        );
        assert!(matches!(
            jetbrains.hold_back,
            Some(HoldBack::SharedWith { .. })
        ));
        assert!(!jetbrains.suggested());
        assert!(
            by_path.contains_key("~/Library/Application Support/JetBrains/com.jetbrains.toolbox"),
            "Toolbox's own folder inside is listed on its own"
        );
        assert!(
            by_path["~/Library/Application Support/JetBrains/com.jetbrains.toolbox"].suggested()
        );

        let chrome = apps.iter().find(|a| a.name == "Google Chrome").unwrap();
        let matches = scan(
            &env,
            chrome,
            &apps,
            &rules,
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&env, &matches);
        let google = by_path["~/Library/Application Support/Google"];
        assert!(!google.suggested());
        assert!(google.rivals.iter().any(|r| r.name == "Google Drive"));
        assert!(
            by_path["~/Library/Application Support/Google/Chrome"].suggested(),
            "Chrome's own folder inside the vendor folder"
        );
    }

    #[test]
    fn children_of_a_parent_that_cannot_move_stay_listed() {
        let f = fixture();
        let acme = f.env.user_library.join("Application Support/Acme");
        std::fs::create_dir_all(
            acme.join("Browser/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn"),
        )
        .unwrap();
        let rules = RuleSet {
            rules: vec![Rule {
                bundle_id: "com.acme.foo".into(),
                name: "Foo".into(),
                aliases: vec![],
                paths: vec![
                    RulePath {
                        path: "~/Library/Application Support/Acme".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    },
                    RulePath {
                        path: "~/Library/Application Support/Acme/Foo".into(),
                        confidence: Confidence::Certain,
                        note: None,
                    },
                ],
            }],
            cask: None,
        };
        let matches = scan(
            &f.env,
            &foo(&f),
            &f.apps,
            &rules,
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&f.env, &matches);
        assert_eq!(
            by_path["~/Library/Application Support/Acme"].hold_back,
            Some(HoldBack::Protected {
                why: ProtectReason::Wallet
            })
        );
        let inner = by_path
            .get("~/Library/Application Support/Acme/Foo")
            .expect("the parent cannot move, so the child is listed on its own");
        assert!(inner.suggested());
    }

    #[test]
    fn locations_only_exist_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        assert!(search_locations(&env).is_empty());
        std::fs::create_dir_all(env.user_library.join("Caches")).unwrap();
        let locations = search_locations(&env);
        assert_eq!(
            locations.len(),
            3,
            "the home, ~/Library itself and Caches: {locations:?}"
        );
        assert!(locations
            .iter()
            .any(|l| l.kind == LeftoverKind::Cache && l.depth == 2));
        std::fs::create_dir_all(env.user_library.join("Cookies")).unwrap();
        assert!(search_locations(&env)
            .iter()
            .any(|l| l.kind == LeftoverKind::Cookies && l.depth == 1));
    }

    #[test]
    fn root_owned_things_need_admin() {
        // SAFETY: geteuid 总是成功。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let f = fixture();
        let app = foo(&f);
        let daemons = f.env.system_library.join("LaunchDaemons");
        std::fs::create_dir_all(&daemons).unwrap();
        std::fs::write(daemons.join("com.acme.foo.plist"), b"x").unwrap();
        std::fs::set_permissions(&daemons, std::fs::Permissions::from_mode(0o555)).unwrap();
        let matches = scan(
            &f.env,
            &app,
            &f.apps,
            &RuleSet::default(),
            &ScanOptions::default(),
            &|_| {},
        );
        std::fs::set_permissions(&daemons, std::fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = matches
            .iter()
            .find(|m| m.kind == LeftoverKind::LaunchDaemon)
            .expect("listed");
        assert_eq!(daemon.hold_back, Some(HoldBack::NeedsAdmin));
        assert!(!daemon.selectable());
    }

    #[test]
    fn things_that_are_not_ours_in_a_sticky_folder_need_admin() {
        let dir = tempfile::tempdir().unwrap();
        let sticky = dir.path().join("Caches");
        std::fs::create_dir_all(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let item = sticky.join("com.acme.foo");
        std::fs::create_dir_all(&item).unwrap();
        // SAFETY: geteuid 总是成功。
        let me = unsafe { libc::geteuid() };
        assert!(
            !needs_admin_as(&item, me),
            "my own folder in a sticky folder moves fine"
        );
        assert!(
            needs_admin_as(&item, me + 1),
            "someone else's folder: the item is not theirs"
        );
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(plain.join("x")).unwrap();
        assert!(!needs_admin_as(&plain.join("x"), me));
        assert!(
            needs_admin_as(&plain.join("x"), me + 1),
            "not the owner, so not theirs to move"
        );
    }

    #[test]
    fn cancelling_stops_the_scan_early() {
        let f = fixture();
        let app = foo(&f);
        let cancel = std::sync::Arc::new(AtomicBool::new(true));
        let opts = ScanOptions {
            cancel: Some(cancel),
            ..ScanOptions::default()
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let matches = scan(&f.env, &app, &f.apps, &RuleSet::default(), &opts, &|p| {
            seen.lock().unwrap().push(p)
        });
        assert!(matches.is_empty(), "a cancelled scan returns nothing");
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|p| matches!(p, ScanProgress::Finished)));
    }

    #[test]
    fn an_unverifiable_signature_loses_its_team_and_groups() {
        let f = fixture();
        let mut app = foo(&f);
        app.team_id = Some("TEAM1234".into());
        app.app_groups = vec!["group.com.acme.shared".into()];
        mk(&f
            .env
            .user_library
            .join("Group Containers/TEAM1234.com.other.thing"));
        mk(&f
            .env
            .user_library
            .join("Group Containers/group.com.acme.shared"));
        let matches = scan(
            &f.env,
            &app,
            &f.apps,
            &RuleSet::default(),
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&f.env, &matches);
        assert!(
            by_path.contains_key("~/Library/Group Containers/TEAM1234.com.other.thing"),
            "an unsigned app's team is whatever we were told"
        );
        assert!(by_path.contains_key("~/Library/Group Containers/group.com.acme.shared"));

        // 有签名、但签名链不到 Apple：Team 和 App Group 都不信。
        std::fs::create_dir_all(app.path.join("Contents/_CodeSignature")).unwrap();
        std::fs::write(
            app.path.join("Contents/_CodeSignature/CodeResources"),
            b"not really",
        )
        .unwrap();
        let matches = scan(
            &f.env,
            &app,
            &f.apps,
            &RuleSet::default(),
            &ScanOptions::default(),
            &|_| {},
        );
        let by_path = index_by_path(&f.env, &matches);
        assert!(!by_path.contains_key("~/Library/Group Containers/TEAM1234.com.other.thing"));
        assert!(!by_path.contains_key("~/Library/Group Containers/group.com.acme.shared"));
        assert!(
            by_path.contains_key("~/Library/Caches/com.acme.foo"),
            "the bundle id still counts"
        );
    }
}
