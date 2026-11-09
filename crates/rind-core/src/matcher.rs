//! 判断一个候选属不属于某个 App，以及凭什么。规则见 docs/ARCHITECTURE.md 第 3 步。
//!
//! 一个 App 的各种 token（bundle id、内嵌组件、App Group、Team、名字）先在 `Claimant` 里算好一次，
//! 之后每个候选、每个 App 的判断只做字符串比较。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::leftovers::Candidate;
use crate::rules::RuleSet;
use crate::{Confidence, Env, Evidence, EvidenceKind, InstalledApp, LeftoverKind};

/// 这个候选属于 `app` 吗？属于就给出证据（取最强的一条），不属于返回 None。
/// `com.apple.*` 与系统目录对任何非 Apple App 都返回 None。
/// 一次性的判断用这个；要对很多候选判断时先建一个 `Claimant`。
pub fn evidence_for(
    env: &Env,
    app: &InstalledApp,
    candidate: &Candidate,
    rules: &RuleSet,
) -> Option<Evidence> {
    Claimant::new(app, rules).evidence(env, candidate, rules)
}

/// 一个 App 认领文件时用到的全部 token，算好一次。
#[derive(Clone, Debug)]
pub struct Claimant<'a> {
    pub app: &'a InstalledApp,
    is_apple: bool,
    /// bundle id，小写。
    id_l: Option<String>,
    /// 内嵌组件的 id（小写）和它是不是同一厂商的。
    embedded: Vec<(String, bool)>,
    /// 内嵌组件的 id，小写，用来认 `id.<suffix>` 里的 suffix。
    embedded_set: HashSet<String>,
    groups_l: Vec<String>,
    /// `TEAMID.`，小写。
    team_prefix_l: Option<String>,
    /// App 的名字、文件名和规则里的别名，原样。
    names: Vec<String>,
    /// 和 `names` 一一对应，归一化后。
    names_norm: Vec<String>,
    /// `com.foo`（小写），没有三段就 None。
    vendor: Option<String>,
    /// bundle id 的最后一段，小写。
    app_last_l: Option<String>,
    /// 归一化后算厂商目录的名字：bundle id 的中间几段、Team、别名、规则里写到的目录。
    vendor_dirs: HashSet<String>,
}

impl<'a> Claimant<'a> {
    pub fn new(app: &'a InstalledApp, rules: &RuleSet) -> Self {
        let id_l = app.bundle_id.as_deref().map(str::to_lowercase);
        let vendor = id_l.as_deref().and_then(vendor_of);
        let embedded: Vec<(String, bool)> = app
            .embedded_ids
            .iter()
            .map(|e| {
                let e_l = e.to_lowercase();
                let same_maker = vendor.is_some() && vendor_of(&e_l) == vendor;
                (e_l, same_maker)
            })
            .collect();
        let embedded_set = embedded.iter().map(|(e, _)| e.clone()).collect();

        let mut names: Vec<String> = vec![app.name.clone()];
        if let Some(file_stem) = app.path.file_stem() {
            names.push(file_stem.to_string_lossy().to_string());
        }
        for rule in rules.for_app(app) {
            names.extend(rule.aliases.iter().cloned());
        }
        let names_norm: Vec<String> = names.iter().map(|n| normalize_name(n)).collect();

        let mut vendor_dirs: HashSet<String> = HashSet::new();
        if let Some(id) = &id_l {
            let parts: Vec<&str> = id.split('.').collect();
            // `com.google.Chrome` 的 `google`；第一段（`com`、`org`……）和最后一段（App 自己）不算。
            if parts.len() >= 3 {
                for part in &parts[1..parts.len() - 1] {
                    let norm = normalize_name(part);
                    if norm.chars().count() >= 3 && !STOP_NAMES.contains(&norm.as_str()) {
                        vendor_dirs.insert(norm);
                    }
                }
            }
        }
        if let Some(team) = app.team_id.as_deref() {
            vendor_dirs.insert(team.to_lowercase());
        }
        vendor_dirs.extend(names_norm.iter().cloned());
        vendor_dirs.extend(rules.mentioned_folders(app));

        Claimant {
            is_apple: id_l
                .as_deref()
                .is_some_and(|id| id.starts_with("com.apple.")),
            app_last_l: id_l
                .as_deref()
                .and_then(|id| id.rsplit('.').next())
                .map(str::to_string),
            id_l,
            embedded,
            embedded_set,
            groups_l: app.app_groups.iter().map(|g| g.to_lowercase()).collect(),
            team_prefix_l: app
                .team_id
                .as_deref()
                .map(|t| format!("{}.", t.to_lowercase())),
            names,
            names_norm,
            vendor,
            vendor_dirs,
            app,
        }
    }

    /// 这个候选属于这个 App 吗？
    pub fn evidence(&self, env: &Env, candidate: &Candidate, rules: &RuleSet) -> Option<Evidence> {
        let name = candidate.path.file_name()?.to_string_lossy().to_string();
        if !self.is_apple
            && (is_apple_name(&name) || crate::protect::is_system_owned(env, &candidate.path))
        {
            return None;
        }

        let stem = candidate_stem(&name);
        let stem_l = stem.to_lowercase();
        let mut best: Option<Evidence> = None;
        let mut consider = |evidence: Evidence| {
            let stronger = match &best {
                None => true,
                Some(b) => {
                    (evidence.confidence, evidence.matched.len()) > (b.confidence, b.matched.len())
                }
            };
            if stronger {
                best = Some(evidence);
            }
        };

        // launchd 任务看它运行的程序，不看文件名。
        if matches!(
            candidate.kind,
            LeftoverKind::LaunchAgent | LeftoverKind::LaunchDaemon
        ) {
            match &candidate.program {
                Some(program) if program.starts_with(&self.app.path) => {
                    consider(Evidence {
                        kind: EvidenceKind::LaunchdProgram,
                        confidence: Confidence::Certain,
                        matched: program.display().to_string(),
                    });
                }
                Some(program) if belongs_to_another_bundle(program, &self.app.path) => return None,
                _ => {}
            }
        }

        if let Some(evidence) = rules.evidence(env, self.app, &candidate.path) {
            consider(evidence);
        }

        if let Some(id) = self.id_l.as_deref() {
            if let Some(hit) = id_hit(&stem_l, id) {
                // `com.acme.foo.ce` 是兄弟 App，不是它自己：只有已知的 helper 后缀或内嵌组件才算它的。
                let certain = match &hit {
                    None => true,
                    Some(suffix) => self.is_own_suffix(id, suffix),
                };
                consider(Evidence {
                    kind: EvidenceKind::BundleId,
                    confidence: if certain {
                        Confidence::Certain
                    } else {
                        Confidence::Likely
                    },
                    matched: self.app.bundle_id.clone().unwrap_or_default(),
                });
            }
            for (embedded, same_maker) in &self.embedded {
                if let Some(hit) = id_hit(&stem_l, embedded) {
                    let exact = match &hit {
                        None => true,
                        Some(suffix) => self.is_own_suffix(embedded, suffix),
                    };
                    let confidence = match (same_maker, exact) {
                        (true, true) => Confidence::Certain,
                        (true, false) | (false, true) => Confidence::Likely,
                        (false, false) => Confidence::Possible,
                    };
                    consider(Evidence {
                        kind: EvidenceKind::EmbeddedBundleId,
                        confidence,
                        matched: embedded.clone(),
                    });
                }
            }
            if let Some(evidence) = self.prefix_evidence(&stem_l) {
                consider(evidence);
            }
        }

        for group in &self.groups_l {
            if let Some(hit) = id_hit(&stem_l, group) {
                consider(Evidence {
                    kind: EvidenceKind::AppGroup,
                    confidence: if hit.is_none() {
                        Confidence::Certain
                    } else {
                        Confidence::Likely
                    },
                    matched: group.clone(),
                });
            }
        }

        if let Some(prefix) = self.team_prefix_l.as_deref() {
            if stem_l.len() > prefix.len() && stem_l.starts_with(prefix) {
                consider(Evidence {
                    kind: EvidenceKind::TeamId,
                    confidence: Confidence::Likely,
                    matched: self.app.team_id.clone().unwrap_or_default(),
                });
            }
        }

        if let Some(evidence) = self.name_evidence(&stem, candidate.nested) {
            consider(evidence);
        }

        best
    }

    /// 一层目录的名字像不像这个 App 的厂商目录（`Google`、`JetBrains`、`Acme`）：像的话才看它里面。
    pub fn is_vendor_dir(&self, name: &str) -> bool {
        self.vendor_dirs.contains(&normalize_name(name))
    }

    /// 候选的文件名里有没有这个 App 的 bundle id、名字或别名（归一化后）。cask 点名的路径靠这个分出
    /// "点名的是它自己的东西"还是"点名了整个厂商目录"。
    pub fn name_mentions_app(&self, file_name: &str) -> bool {
        let stem_l = candidate_stem(file_name).to_lowercase();
        if self
            .id_l
            .as_deref()
            .is_some_and(|id| id_hit(&stem_l, id).is_some())
        {
            return true;
        }
        let norm = normalize_name(&stem_l);
        let words: Vec<String> = stem_l
            .split([' ', '.', '_', '-'])
            .map(normalize_name)
            .filter(|w| !w.is_empty())
            .collect();
        self.names_norm.iter().any(|n| {
            n.chars().count() >= 3
                && !STOP_NAMES.contains(&n.as_str())
                && (norm == *n
                    || words.contains(n)
                    || (n.chars().count() >= 5 && norm.contains(n.as_str())))
        })
    }

    /// `id.<suffix>` 里的 suffix 是它自己的一部分吗：已知的 helper 后缀，或者内嵌组件的 id。
    fn is_own_suffix(&self, id: &str, suffix: &str) -> bool {
        let suffix_l = suffix.to_lowercase();
        HELPER_SUFFIXES.contains(&suffix_l.as_str())
            || self.embedded_set.contains(&format!("{id}.{suffix_l}"))
    }

    /// 同一厂商的别的 id：最后一段是 App 的最后一段加已知变体（`VSCode` ↔ `VSCodeInsiders`）→ likely，
    /// 否则 possible。
    fn prefix_evidence(&self, stem_l: &str) -> Option<Evidence> {
        let vendor = self.vendor.as_deref()?;
        if GENERIC_VENDORS.contains(&vendor) {
            return None;
        }
        let prefix = format!("{vendor}.");
        let rest = stem_l.strip_prefix(&prefix)?;
        let candidate_last = rest.split('.').next().filter(|s| !s.is_empty())?;
        let app_last = self.app_last_l.as_deref()?;
        let confidence = match candidate_last.strip_prefix(app_last) {
            Some(tail) => {
                let tail = tail.trim_start_matches(['-', '_']);
                if KNOWN_VARIANTS.contains(&tail) {
                    Confidence::Likely
                } else {
                    Confidence::Possible
                }
            }
            None => Confidence::Possible,
        };
        Some(Evidence {
            kind: EvidenceKind::BundleIdPrefix,
            confidence,
            matched: vendor.to_string(),
        })
    }

    /// 名字相等 → likely；名字作为一个词出现（或长名字被包含）→ possible。
    /// 第二层的东西只认相等：`Electron App/Code Cache` 里的 `Code` 不说明是 VS Code 的。
    fn name_evidence(&self, stem: &str, nested: bool) -> Option<Evidence> {
        let stem_norm = normalize_name(stem);
        let words: Vec<String> = stem
            .split([' ', '.', '_', '-'])
            .map(normalize_name)
            .filter(|w| !w.is_empty())
            .collect();
        let mut best: Option<Evidence> = None;
        for (name, norm) in self.names.iter().zip(&self.names_norm) {
            if norm.chars().count() < 3 || STOP_NAMES.contains(&norm.as_str()) {
                continue;
            }
            let as_word = words.contains(norm)
                || (norm.chars().count() >= 5 && stem_norm.contains(norm.as_str()));
            let confidence = if stem_norm == *norm {
                Confidence::Likely
            } else if !nested && as_word {
                Confidence::Possible
            } else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|b| (confidence, name.len()) > (b.confidence, b.matched.len()))
            {
                best = Some(Evidence {
                    kind: EvidenceKind::Name,
                    confidence,
                    matched: name.clone(),
                });
            }
        }
        best
    }
}

/// 名字归一化：小写、去掉空格 / 连字符 / 下划线 / `.app`。
pub fn normalize_name(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let lower = lower.strip_suffix(".app").unwrap_or(&lower);
    lower
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect()
}

fn is_apple_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("com.apple.") || lower.starts_with("group.com.apple.") || lower == "com.apple"
}

/// 去掉 `.plist` / `.savedState` 这类说明种类的后缀，和 ByHost 的 `.<UUID>`。
pub(crate) fn candidate_stem(name: &str) -> String {
    const SUFFIXES: [&str; 17] = [
        ".plist",
        ".savedState",
        ".binarycookies",
        ".sfl3",
        ".sfl2",
        ".sfl",
        ".bom",
        ".log",
        ".json",
        ".db",
        ".sqlite",
        ".xml",
        ".dat",
        ".lock",
        ".pid",
        ".txt",
        ".ini",
    ];
    let mut stem = name.to_string();
    for suffix in SUFFIXES {
        let cut = stem.len().saturating_sub(suffix.len());
        if cut > 0 && stem.is_char_boundary(cut) && stem[cut..].eq_ignore_ascii_case(suffix) {
            stem.truncate(cut);
            break;
        }
    }
    static UUID_TAIL: OnceLock<Regex> = OnceLock::new();
    let uuid_tail = UUID_TAIL.get_or_init(|| {
        Regex::new(r"(?i)\.[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
    });
    uuid_tail.replace(&stem, "").to_string()
}

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| matches!(c, '.' | '_' | '-' | ' '))
}

/// `id`（小写）作为完整片段出现在 `stem_l`（小写）里吗？
/// `None`：没有。`Some(None)`：有，后面是结尾或 `_ - 空格`。`Some(Some(suffix))`：有，后面紧跟 `.suffix`
/// （`com.acme.foo.helper` 的 `helper`，`com.acme.foo.ce` 的 `ce`），由调用方决定 suffix 算不算它自己的。
fn id_hit(stem_l: &str, id: &str) -> Option<Option<String>> {
    if id.is_empty() {
        return None;
    }
    let mut found: Option<Option<String>> = None;
    let mut from = 0;
    while let Some(pos) = stem_l[from..].find(id) {
        let start = from + pos;
        let end = start + id.len();
        let before_ok = is_boundary(stem_l[..start].chars().next_back());
        let after = stem_l[end..].chars().next();
        if before_ok && is_boundary(after) {
            match after {
                Some('.') => {
                    let suffix: String = stem_l[end + 1..]
                        .chars()
                        .take_while(|c| !is_boundary(Some(*c)))
                        .collect();
                    if suffix.is_empty() {
                        return Some(None);
                    }
                    if found.is_none() {
                        found = Some(Some(suffix));
                    }
                }
                _ => return Some(None),
            }
        }
        from = start + stem_l[start..].chars().next().map_or(1, char::len_utf8);
        if from >= stem_l.len() {
            break;
        }
    }
    found
}

/// `com.foo.bar` → `com.foo`；不足三段就没有厂商可言。输入已是小写。
fn vendor_of(bundle_id: &str) -> Option<String> {
    let parts: Vec<&str> = bundle_id.split('.').collect();
    (parts.len() >= 3).then(|| parts[..2].join("."))
}

/// 这些前缀谁都在用，不能说明厂商。
const GENERIC_VENDORS: [&str; 4] = ["com.electron", "com.github", "io.github", "org.electron"];

/// `id.<suffix>` 里说明还是它自己的那些 suffix。
pub(crate) const HELPER_SUFFIXES: [&str; 29] = [
    "helper",
    "helpers",
    "renderer",
    "gpu",
    "plugin",
    "plugins",
    "shipit",
    "updater",
    "update",
    "autoupdate",
    "xpc",
    "agent",
    "daemon",
    "service",
    "launcher",
    "login",
    "loginitem",
    "loginhelper",
    "crashreporter",
    "crashpad",
    "notifications",
    "extension",
    "share",
    "quicklook",
    "finder",
    "networking",
    "watchdog",
    "monitor",
    "pkg",
];

/// 同一厂商、名字只多了这些的，是它的变体（`VSCodeInsiders`），不是别的产品。
const KNOWN_VARIANTS: [&str; 7] = [
    "insiders", "beta", "canary", "nightly", "dev", "preview", "helper",
];

/// 太普通、说明不了归属的名字。
const STOP_NAMES: [&str; 16] = [
    "app", "apps", "helper", "mac", "macos", "update", "updater", "launcher", "agent", "service",
    "pro", "lite", "desktop", "client", "beta", "tool",
];

/// launchd plist 运行的程序：`Program`，否则 `ProgramArguments[0]`。超过 1 MB 的文件不读。
pub(crate) fn launchd_program(plist_path: &Path) -> Option<PathBuf> {
    let value = crate::apps::read_plist_bounded(plist_path)?;
    launchd_program_in(value.as_dictionary()?)
}

/// 同上，给已经解析好的 plist 用（启动项管理还要读别的键，不想读两遍文件）。
pub(crate) fn launchd_program_in(dict: &plist::Dictionary) -> Option<PathBuf> {
    if let Some(program) = dict.get("Program").and_then(|v| v.as_string()) {
        return Some(program.into());
    }
    dict.get("ProgramArguments")?
        .as_array()?
        .first()?
        .as_string()
        .map(Into::into)
}

/// 程序在另一个 `.app` 包里（而不是 `own` 里）。
fn belongs_to_another_bundle(program: &Path, own: &Path) -> bool {
    program
        .ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "app"))
        .is_some_and(|bundle| bundle != own && bundle.join("Contents/Info.plist").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::testing::make_app;
    use crate::rules::Rule;
    use crate::{AppSource, Size};

    fn app(dir: &Path) -> InstalledApp {
        InstalledApp {
            path: dir.join("Applications/Foo.app"),
            name: "Foo".into(),
            bundle_id: Some("com.acme.foo".into()),
            version: None,
            executable: None,
            team_id: Some("TEAM1234".into()),
            embedded_ids: vec![
                "com.acme.foo.share".into(),
                "org.sparkle-project.Sparkle".into(),
            ],
            app_groups: vec!["group.com.acme.foo".into()],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        }
    }

    fn candidate(env: &Env, rel: &str, kind: LeftoverKind) -> Candidate {
        Candidate::new(env.user_library.join(rel), kind, !rel.contains('.'), false)
    }

    #[test]
    fn nested_names_must_equal_or_start_with_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path());
        let nested = |rel: &str| {
            let mut c = candidate(&env, rel, LeftoverKind::AppSupport);
            c.nested = true;
            evidence_for(&env, &app, &c, &RuleSet::default()).map(|e| e.confidence)
        };
        assert_eq!(
            nested("Application Support/Electron/Foo Cache"),
            None,
            "a word inside a nested name says nothing"
        );
        assert_eq!(
            nested("Application Support/Acme/Foo"),
            Some(Confidence::Likely)
        );
        assert_eq!(
            nested("Application Support/Acme/foo-data"),
            None,
            "nor does a nested name that starts with it"
        );
        assert_eq!(
            nested("Application Support/Acme/com.acme.foo"),
            Some(Confidence::Certain),
            "identifiers still count"
        );
    }

    fn judge(
        env: &Env,
        app: &InstalledApp,
        rel: &str,
        kind: LeftoverKind,
    ) -> Option<(EvidenceKind, Confidence)> {
        evidence_for(env, app, &candidate(env, rel, kind), &RuleSet::default())
            .map(|e| (e.kind, e.confidence))
    }

    #[test]
    fn non_ascii_names_do_not_break_anything() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let mut app = app(dir.path());
        app.name = "腾讯会议".into();
        app.path = dir.path().join("Applications/腾讯会议.app");
        use LeftoverKind::*;
        assert_eq!(
            judge(&env, &app, "Preferences/腾讯会议.plist", Preferences),
            Some((EvidenceKind::Name, Confidence::Likely))
        );
        assert_eq!(
            judge(&env, &app, "Caches/腾讯会议 日志", Cache),
            Some((EvidenceKind::Name, Confidence::Possible))
        );
        assert_eq!(judge(&env, &app, "Caches/会议", Cache), None);
        assert_eq!(judge(&env, &app, "Caches/列表.db", Cache), None);
        assert_eq!(
            judge(&env, &app, "Caches/TEAM1234.列表", Cache),
            Some((EvidenceKind::TeamId, Confidence::Likely))
        );
        assert_eq!(
            judge(&env, &app, "Caches/com.acme.foo 列表", Cache),
            Some((EvidenceKind::BundleId, Confidence::Certain))
        );
        assert_eq!(id_hit("列表.com.acme.foo", "com.acme.foo"), Some(None));
        assert_eq!(id_hit("列表com.acme.foo", "com.acme.foo"), None);
        assert_eq!(
            id_hit("com.acme.foo.列表", "com.acme.foo"),
            Some(Some("列表".into()))
        );
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_name("Visual Studio Code.app"), "visualstudiocode");
        assert_eq!(normalize_name("Foo-Bar_baz"), "foobarbaz");
        assert_eq!(normalize_name("  微信 "), "微信");
    }

    #[test]
    fn identifiers_are_certain() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path());
        use Confidence::*;
        use EvidenceKind::*;
        use LeftoverKind::*;
        assert_eq!(
            judge(&env, &app, "Caches/com.acme.foo", Cache),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(&env, &app, "Preferences/com.acme.foo.plist", Preferences),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Preferences/ByHost/com.acme.foo.0F3A1B2C-1234-5678-9ABC-DEF012345678.plist",
                Preferences
            ),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Containers/TEAM1234.com.acme.foo.helper",
                Container
            ),
            Some((BundleId, Certain)),
            "a helper suffix is still it"
        );
        assert_eq!(
            judge(&env, &app, "Containers/com.acme.foo.ShipIt", Container),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(&env, &app, "Containers/com.acme.foo.share", Container),
            Some((EmbeddedBundleId, Certain)),
            "a suffix that is an embedded component is it"
        );
        assert_eq!(
            judge(&env, &app, "Caches/org.sparkle-project.Sparkle", Cache),
            Some((EmbeddedBundleId, Likely)),
            "another maker's framework"
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Caches/org.sparkle-project.Sparkle.other",
                Cache
            ),
            Some((EmbeddedBundleId, Possible)),
            "another maker's framework, and not even exactly"
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Group Containers/group.com.acme.foo",
                GroupContainer
            ),
            Some((AppGroup, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Group Containers/group.com.acme.foo.other",
                GroupContainer
            ),
            Some((AppGroup, Likely))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Group Containers/TEAM1234.com.other.thing",
                GroupContainer
            ),
            Some((TeamId, Likely))
        );
        assert_eq!(
            judge(&env, &app, "Caches/com.acme.foobar", Cache),
            Some((BundleIdPrefix, Possible)),
            "same maker, a longer name: another product"
        );
        assert_eq!(
            judge(&env, &app, "Caches/com.acme.fo", Cache),
            Some((BundleIdPrefix, Possible)),
            "same maker, a different name: possible"
        );
    }

    #[test]
    fn sibling_bundle_ids_are_not_certain() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        use Confidence::*;
        use EvidenceKind::*;
        use LeftoverKind::*;

        let mut intellij = app(dir.path());
        intellij.bundle_id = Some("com.jetbrains.intellij".into());
        intellij.name = "IntelliJ IDEA".into();
        intellij.embedded_ids.clear();
        assert_eq!(
            judge(
                &env,
                &intellij,
                "Preferences/com.jetbrains.intellij.plist",
                Preferences
            ),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &intellij,
                "Preferences/com.jetbrains.intellij.ce.plist",
                Preferences
            ),
            Some((BundleId, Likely)),
            "Community Edition is a sibling"
        );

        let mut chrome = app(dir.path());
        chrome.bundle_id = Some("com.google.Chrome".into());
        chrome.name = "Google Chrome".into();
        chrome.embedded_ids = vec!["com.google.Chrome.helper".into()];
        assert_eq!(
            judge(&env, &chrome, "Caches/com.google.Chrome.canary", Cache),
            Some((BundleId, Likely)),
            "Canary is a sibling"
        );
        assert_eq!(
            judge(&env, &chrome, "Caches/com.google.Chrome.helper", Cache),
            Some((EmbeddedBundleId, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &chrome,
                "Preferences/com.google.Chrome.plist",
                Preferences
            ),
            Some((BundleId, Certain))
        );
        assert_eq!(
            judge(
                &env,
                &chrome,
                "Preferences/com.google.Keystone.plist",
                Preferences
            ),
            Some((BundleIdPrefix, Possible))
        );

        let mut qq = app(dir.path());
        qq.bundle_id = Some("com.tencent.qq".into());
        qq.name = "QQ".into();
        qq.embedded_ids.clear();
        assert_eq!(
            judge(&env, &qq, "Containers/com.tencent.qqmusic", Container),
            Some((BundleIdPrefix, Possible)),
            "QQ Music is another product"
        );
        assert_eq!(
            judge(&env, &qq, "Containers/com.tencent.qqlive", Container),
            Some((BundleIdPrefix, Possible))
        );
        assert_eq!(
            judge(&env, &qq, "Containers/com.tencent.qq", Container),
            Some((BundleId, Certain))
        );
    }

    #[test]
    fn vendor_prefix_is_likely_only_for_known_variants() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path());
        use LeftoverKind::*;
        assert_eq!(
            judge(
                &env,
                &app,
                "Preferences/com.acme.fooinsiders.plist",
                Preferences
            ),
            Some((EvidenceKind::BundleIdPrefix, Confidence::Likely))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "Preferences/com.acme.foomusic.plist",
                Preferences
            ),
            Some((EvidenceKind::BundleIdPrefix, Confidence::Possible))
        );
        assert_eq!(
            judge(&env, &app, "Preferences/com.acme.other.plist", Preferences),
            Some((EvidenceKind::BundleIdPrefix, Confidence::Possible))
        );
        assert_eq!(
            judge(&env, &app, "Preferences/com.acme.plist", Preferences),
            None,
            "the vendor alone says nothing"
        );

        let mut electron = app.clone();
        electron.bundle_id = Some("com.electron.lark".into());
        electron.name = "Lark".into();
        assert_eq!(
            judge(
                &env,
                &electron,
                "Preferences/com.electron.other.plist",
                Preferences
            ),
            None,
            "generic vendors do not count"
        );
    }

    #[test]
    fn names_are_likely_when_equal_and_possible_as_a_word() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path());
        use LeftoverKind::*;
        assert_eq!(
            judge(&env, &app, "Application Support/Foo", AppSupport),
            Some((EvidenceKind::Name, Confidence::Likely))
        );
        assert_eq!(
            judge(&env, &app, "Application Support/foo", AppSupport),
            Some((EvidenceKind::Name, Confidence::Likely))
        );
        assert_eq!(
            judge(&env, &app, "Logs/Foo Helper", Log),
            Some((EvidenceKind::Name, Confidence::Possible))
        );
        assert_eq!(
            judge(&env, &app, "Logs/foo_helper.log", Log),
            Some((EvidenceKind::Name, Confidence::Possible))
        );
        assert_eq!(
            judge(&env, &app, "Caches/Food", Cache),
            None,
            "a longer word is a different word"
        );
        assert_eq!(
            judge(&env, &app, "Caches/FooBar", Cache),
            None,
            "short names must stand alone"
        );

        let mut long = app.clone();
        long.name = "Visual Studio Code".into();
        long.path = dir.path().join("Applications/Visual Studio Code.app");
        assert_eq!(
            judge(
                &env,
                &long,
                "Application Support/VisualStudioCodeData",
                AppSupport
            ),
            Some((EvidenceKind::Name, Confidence::Possible)),
            "long names may be embedded"
        );
        assert_eq!(
            judge(&env, &long, "Application Support/Code", AppSupport),
            None,
            "a word of the name is not the name"
        );

        let mut generic = app.clone();
        generic.name = "Helper".into();
        generic.path = dir.path().join("Applications/App.app");
        assert_eq!(
            judge(&env, &generic, "Application Support/Helper", AppSupport),
            None,
            "stop words never match"
        );
    }

    #[test]
    fn vendor_dirs_and_name_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let mut chrome = app(dir.path());
        chrome.bundle_id = Some("com.google.Chrome".into());
        chrome.name = "Google Chrome".into();
        chrome.path = dir.path().join("Applications/Google Chrome.app");
        let rules = RuleSet {
            rules: vec![Rule {
                bundle_id: "com.google.Chrome".into(),
                name: "Google Chrome".into(),
                aliases: vec!["Chrome".into()],
                paths: vec![crate::rules::RulePath {
                    path: "~/Library/Application Support/Google/Chrome".into(),
                    confidence: Confidence::Certain,
                    note: None,
                }],
            }],
            cask: None,
        };
        let claimant = Claimant::new(&chrome, &rules);
        assert!(
            claimant.is_vendor_dir("Google"),
            "a segment of the bundle id"
        );
        assert!(claimant.is_vendor_dir("TEAM1234"));
        assert!(claimant.is_vendor_dir("Chrome"), "an alias");
        assert!(!claimant.is_vendor_dir("Electron Thing"));
        assert!(
            !claimant.is_vendor_dir("com"),
            "the first segment says nothing"
        );

        assert!(claimant.name_mentions_app("com.google.Chrome.plist"));
        assert!(claimant.name_mentions_app("Chrome"));
        assert!(
            claimant.name_mentions_app("Chrome Extra"),
            "a short name still counts as a whole word"
        );
        assert!(
            claimant.name_mentions_app("Google Chrome Canary"),
            "contains the whole name"
        );
        assert!(
            !claimant.name_mentions_app("Google"),
            "the vendor folder is not it"
        );
        assert!(!claimant.name_mentions_app("com.google.Keystone"));
    }

    #[test]
    fn rule_aliases_count_as_names_and_rules_win() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path());
        let rules = RuleSet {
            rules: vec![Rule {
                bundle_id: "com.acme.foo".into(),
                name: "Foo".into(),
                aliases: vec!["Foozle".into()],
                paths: vec![crate::rules::RulePath {
                    path: "~/Library/Application Support/Strange Name".into(),
                    confidence: Confidence::Certain,
                    note: None,
                }],
            }],
            cask: None,
        };
        let alias = evidence_for(
            &env,
            &app,
            &candidate(&env, "Caches/Foozle", LeftoverKind::Cache),
            &rules,
        )
        .unwrap();
        assert_eq!(
            (alias.kind, alias.confidence),
            (EvidenceKind::Name, Confidence::Likely)
        );
        let ruled = evidence_for(
            &env,
            &app,
            &candidate(
                &env,
                "Application Support/Strange Name",
                LeftoverKind::AppSupport,
            ),
            &rules,
        )
        .unwrap();
        assert_eq!(
            (ruled.kind, ruled.confidence),
            (
                EvidenceKind::Rule { rule: "Foo".into() },
                Confidence::Certain
            )
        );
    }

    #[test]
    fn apple_and_system_things_never_match() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let mut app = app(dir.path());
        app.name = "Knowledge".into();
        app.bundle_id = Some("com.acme.knowledge".into());
        use LeftoverKind::*;
        assert_eq!(judge(&env, &app, "Caches/com.apple.knowledge", Cache), None);
        assert_eq!(
            judge(&env, &app, "Application Support/Knowledge", AppSupport),
            None,
            "a system folder by a plain name"
        );
        assert_eq!(
            judge(&env, &app, "Caches/Knowledge", Cache),
            Some((EvidenceKind::Name, Confidence::Likely)),
            "only directly under its own folder"
        );
    }

    #[test]
    fn launchd_jobs_are_judged_by_their_program() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let foo_path = make_app(&env.app_dirs[0], "Foo", "com.acme.foo");
        let bar_path = make_app(&env.app_dirs[0], "Bar", "com.other.bar");
        let mut app = app(dir.path());
        app.path = foo_path.clone();
        let agents = env.user_library.join("LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();

        let write = |name: &str, program: &str, use_args: bool| {
            let mut dict = plist::Dictionary::new();
            dict.insert("Label".into(), name.into());
            if use_args {
                dict.insert(
                    "ProgramArguments".into(),
                    plist::Value::Array(vec![program.into(), "--x".into()]),
                );
            } else {
                dict.insert("Program".into(), program.into());
            }
            plist::Value::Dictionary(dict)
                .to_file_xml(agents.join(format!("{name}.plist")))
                .unwrap();
        };
        write(
            "com.whatever.job",
            &foo_path.join("Contents/MacOS/Foo").display().to_string(),
            false,
        );
        write(
            "com.whatever.args",
            &foo_path
                .join("Contents/Helpers/agent")
                .display()
                .to_string(),
            true,
        );
        write(
            "com.acme.foo.agent",
            &bar_path.join("Contents/MacOS/Bar").display().to_string(),
            false,
        );
        write(
            "com.acme.foo.gone",
            "/nowhere/Gone.app/Contents/MacOS/Gone",
            false,
        );

        use LeftoverKind::LaunchAgent;
        assert_eq!(
            judge(
                &env,
                &app,
                "LaunchAgents/com.whatever.job.plist",
                LaunchAgent
            ),
            Some((EvidenceKind::LaunchdProgram, Confidence::Certain))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "LaunchAgents/com.whatever.args.plist",
                LaunchAgent
            ),
            Some((EvidenceKind::LaunchdProgram, Confidence::Certain))
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "LaunchAgents/com.acme.foo.agent.plist",
                LaunchAgent
            ),
            None,
            "it runs Bar, so it is Bar's whatever its name"
        );
        assert_eq!(
            judge(
                &env,
                &app,
                "LaunchAgents/com.acme.foo.gone.plist",
                LaunchAgent
            ),
            Some((EvidenceKind::BundleId, Confidence::Likely)),
            "a program that is gone leaves the name to decide, and `gone` is not a known suffix"
        );

        // 太大的 plist 不读：名字说了算。
        let huge = agents.join("com.acme.foo.huge.plist");
        std::fs::write(&huge, vec![b' '; 1_100_000]).unwrap();
        assert_eq!(launchd_program(&huge), None);
    }
}
