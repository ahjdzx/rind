//! 数据驱动的规则：`rules/*.toml`（编译时内嵌）和 Homebrew cask 的 zap 段。
//!
//! cask 的数据是用户可写的缓存，Rind 不验证它的签名，所以只把它当**线索**，不当事实：
//! 只收 `trash` / `delete`；`rmdir`（Homebrew 的语义是"空了才删"）只在目录真的空时才算；
//! 只认 `~/Library`、`/Library` 和 Caskroom 下的路径；通配符不能跨目录，也不能整段都是 `*`。

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use include_dir::{include_dir, Dir};
use serde::Deserialize;

use crate::{AppSource, Confidence, Env, Evidence, EvidenceKind, InstalledApp};

static RULES_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../rules");

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RulePath {
    /// 支持 `~` 与 glob。
    pub path: String,
    pub confidence: Confidence,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub bundle_id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub paths: Vec<RulePath>,
}

#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    /// Homebrew 的 cask 索引，`with_casks` 加载一次，之后每次扫描复用。
    pub cask: Option<CaskIndex>,
}

impl RuleSet {
    /// 编译进来的 `rules/*.toml`，不带 cask 索引。解析不了的文件被跳过（`builtin_rules_all_parse` 测试保证不会有）。
    pub fn builtin() -> RuleSet {
        let rules = RULES_DIR
            .files()
            .filter(|f| f.path().extension().is_some_and(|e| e == "toml"))
            .filter_map(|f| f.contents_utf8())
            .filter_map(|text| Self::parse(text).ok())
            .collect();
        RuleSet { rules, cask: None }
    }

    /// 内嵌规则加上这台机器的 Homebrew cask 索引（没有 Homebrew 缓存就只有规则）。
    pub fn with_casks(env: &Env) -> RuleSet {
        RuleSet {
            cask: CaskIndex::load(env),
            ..Self::builtin()
        }
    }

    pub fn parse(toml_text: &str) -> anyhow::Result<Rule> {
        Ok(toml::from_str(toml_text)?)
    }

    /// 适用于这个 App 的规则（按 bundle id 精确匹配，不分大小写）。
    pub fn for_app<'a>(&'a self, app: &InstalledApp) -> Vec<&'a Rule> {
        let Some(id) = app.bundle_id.as_deref() else {
            return Vec::new();
        };
        self.rules
            .iter()
            .filter(|r| r.bundle_id.eq_ignore_ascii_case(id))
            .collect()
    }

    /// 规则点名了这条路径吗？点名了就给 `EvidenceKind::Rule` 的证据（取把握最大的一条）。
    pub fn evidence(&self, env: &Env, app: &InstalledApp, path: &Path) -> Option<Evidence> {
        let mut best: Option<Evidence> = None;
        for rule in self.for_app(app) {
            for rp in &rule.paths {
                if !pattern_matches(&substitute(env, &rp.path, app), path) {
                    continue;
                }
                let evidence = Evidence {
                    kind: EvidenceKind::Rule {
                        rule: rule.name.clone(),
                    },
                    confidence: rp.confidence,
                    matched: rp.path.clone(),
                };
                if best
                    .as_ref()
                    .is_none_or(|b| evidence.confidence > b.confidence)
                {
                    best = Some(evidence);
                }
            }
        }
        best
    }

    /// 规则声明的全部路径（展开 glob），用来补充候选。只返回磁盘上存在的。
    pub fn declared_paths(&self, env: &Env, app: &InstalledApp) -> Vec<PathBuf> {
        let mut out = BTreeSet::new();
        for rule in self.for_app(app) {
            for rp in &rule.paths {
                out.extend(existing_matches(&substitute(env, &rp.path, app)));
            }
        }
        out.into_iter().collect()
    }

    /// 规则里写到的目录名（归一化后），用来认厂商目录：`JetBrains/IntelliJIdea*` 里的 `jetbrains`。
    pub(crate) fn mentioned_folders(&self, app: &InstalledApp) -> Vec<String> {
        let mut out = BTreeSet::new();
        for rule in self.for_app(app) {
            for rp in &rule.paths {
                for part in Path::new(&rp.path).components() {
                    if let Component::Normal(name) = part {
                        let name = name.to_string_lossy();
                        if name != "~" && !has_glob(&name) {
                            out.insert(crate::matcher::normalize_name(&name));
                        }
                    }
                }
            }
        }
        out.into_iter().collect()
    }
}

/// 展开 `~`，并把 cask 里的 `$(TeamIdentifierPrefix)` / `$APPDIR` 换掉。Team 未知时按 `*.` 当 glob。
pub(crate) fn substitute(env: &Env, pattern: &str, app: &InstalledApp) -> String {
    let team_prefix = app
        .team_id
        .as_deref()
        .map(|t| format!("{t}."))
        .unwrap_or_else(|| "*.".to_string());
    let appdir = app
        .path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/Applications".into());
    let replaced = pattern
        .replace("$(TeamIdentifierPrefix)", &team_prefix)
        .replace("$APPDIR", &appdir);
    env.expand(&replaced).display().to_string()
}

pub(crate) fn has_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// 一个（已展开的）模式匹配这条路径吗？glob 不跨 `/`，不分大小写。
pub(crate) fn pattern_matches(pattern: &str, path: &Path) -> bool {
    if has_glob(pattern) {
        let options = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        glob::Pattern::new(pattern)
            .map(|p| p.matches_path_with(path, options))
            .unwrap_or(false)
    } else {
        Path::new(pattern) == path
    }
}

/// 一个（已展开的）模式在磁盘上对应的路径。
pub(crate) fn existing_matches(pattern: &str) -> Vec<PathBuf> {
    if has_glob(pattern) {
        let options = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        glob::glob_with(pattern, options)
            .map(|paths| paths.flatten().collect())
            .unwrap_or_default()
    } else {
        let path = PathBuf::from(pattern);
        if path.symlink_metadata().is_ok() {
            vec![path]
        } else {
            Vec::new()
        }
    }
}

/// cask 点名的一条路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaskPath {
    /// 已展开 `~` 和 `$HOMEBREW_PREFIX`；`$(TeamIdentifierPrefix)` / `$APPDIR` 留到查询时换。
    pub pattern: String,
    /// 来自 `rmdir`：只有目录空了才算数。
    pub only_if_empty: bool,
}

/// cask 点名、磁盘上存在、且通过了检查的一条路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaskHit {
    pub token: String,
    pub path: PathBuf,
    /// cask 是按 App 的来源（Homebrew 装的）找到的，而不是按 `.app` 文件名猜的。
    pub trusted: bool,
}

/// Homebrew 的 cask 索引。新版 Homebrew（7.x）在
/// `~/Library/Caches/Homebrew/api/internal/packages.<tag>.jws.json(.payload)`，
/// 旧版在 `~/Library/Caches/Homebrew/api/cask.jws.json`。两种都认；没有就不用。
#[derive(Clone, Debug, Default)]
pub struct CaskIndex {
    /// cask token → 它的 zap / uninstall 段点名的路径。
    pub zap_paths: HashMap<String, Vec<CaskPath>>,
    /// `.app` 文件名（如 `Obsidian.app`）→ token。几个 cask 装同一个名字时取排序最小的那个，每次都一样。
    pub app_tokens: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Jws {
    payload: String,
}

impl CaskIndex {
    pub fn load(env: &Env) -> Option<CaskIndex> {
        let api = env.user_library.join("Caches/Homebrew/api");
        let mut index = CaskIndex::default();
        let mut found = false;

        if let Ok(entries) = std::fs::read_dir(api.join("internal")) {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            // 同一份数据可能同时有 `.jws.json` 和 `.jws.json.payload`，后者不用反转义，优先。
            let payloads: Vec<&String> = names
                .iter()
                .filter(|n| n.starts_with("packages.") && n.ends_with(".jws.json.payload"))
                .collect();
            let wrappers: Vec<&String> = names
                .iter()
                .filter(|n| n.starts_with("packages.") && n.ends_with(".jws.json"))
                .collect();
            let chosen = if payloads.is_empty() {
                wrappers
            } else {
                payloads
            };
            for name in chosen {
                let path = api.join("internal").join(name);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let json = if name.ends_with(".payload") {
                    text.split_once('\n')
                        .map(|(_, rest)| rest.to_string())
                        .unwrap_or(text)
                } else {
                    let Ok(jws) = serde_json::from_str::<Jws>(&text) else {
                        continue;
                    };
                    jws.payload
                };
                // 逐个 cask 容错：一个 cask 的结构不对，别的照样读。
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(&json) else {
                    continue;
                };
                let Some(casks) = payload.get("casks").and_then(|c| c.as_object()) else {
                    continue;
                };
                for (token, cask) in casks {
                    let artifacts = cask.get("raw_artifacts").and_then(|a| a.as_array());
                    let artifacts = artifacts
                        .into_iter()
                        .flatten()
                        .filter_map(internal_artifact);
                    index.absorb(env, token, artifacts);
                }
                found = true;
            }
        }

        if let Ok(text) = std::fs::read_to_string(api.join("cask.jws.json")) {
            if let Ok(jws) = serde_json::from_str::<Jws>(&text) {
                if let Ok(casks) = serde_json::from_str::<Vec<serde_json::Value>>(&jws.payload) {
                    for cask in &casks {
                        let Some(token) = cask.get("token").and_then(|t| t.as_str()) else {
                            continue;
                        };
                        let artifacts = cask.get("artifacts").and_then(|a| a.as_array());
                        let artifacts = artifacts.into_iter().flatten().filter_map(old_artifact);
                        index.absorb(env, token, artifacts);
                    }
                    found = true;
                }
            }
        }

        found.then_some(index)
    }

    fn absorb<'a>(
        &mut self,
        env: &Env,
        token: &str,
        artifacts: impl Iterator<Item = (String, &'a serde_json::Value)>,
    ) {
        for (kind, args) in artifacts {
            match kind.as_str() {
                "app" => {
                    for name in app_names(args) {
                        let entry = self
                            .app_tokens
                            .entry(name)
                            .or_insert_with(|| token.to_string());
                        if token < entry.as_str() {
                            *entry = token.to_string();
                        }
                    }
                }
                "zap" | "uninstall" => {
                    let mut trash = Vec::new();
                    let mut rmdir = Vec::new();
                    collect_paths(args, &mut trash, &mut rmdir);
                    let entry = self.zap_paths.entry(token.to_string()).or_default();
                    let declared = trash
                        .into_iter()
                        .map(|p| (p, false))
                        .chain(rmdir.into_iter().map(|p| (p, true)));
                    for (raw, only_if_empty) in declared {
                        let Some(pattern) = accepted_cask_pattern(env, &raw) else {
                            continue;
                        };
                        let cask_path = CaskPath {
                            pattern,
                            only_if_empty,
                        };
                        if !entry.contains(&cask_path) {
                            entry.push(cask_path);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// 这个 App 对应的 cask token，以及它是不是可信的来源（Homebrew 装的）。
    /// 没有 Homebrew 来源时按 `.app` 文件名猜，那只是线索。
    pub fn token_for(&self, app: &InstalledApp) -> Option<(String, bool)> {
        match &app.source {
            AppSource::Homebrew(token) => Some((token.clone(), true)),
            _ => app
                .path
                .file_name()
                .and_then(|n| self.app_tokens.get(&n.to_string_lossy().to_string()))
                .map(|t| (t.clone(), false)),
        }
    }

    /// 这个 App 的 cask 点名、通过检查、且磁盘上存在的路径。`rmdir` 的只在目录空时给。
    pub fn paths_for(&self, env: &Env, app: &InstalledApp) -> Vec<CaskHit> {
        let Some((token, trusted)) = self.token_for(app) else {
            return Vec::new();
        };
        let mut out: Vec<CaskHit> = Vec::new();
        for (pattern, only_if_empty) in self.patterns_for(env, app) {
            for found in existing_matches(&pattern) {
                if !path_is_allowed(env, &found) {
                    continue;
                }
                if only_if_empty && !is_empty_dir(&found) {
                    continue;
                }
                if !out.iter().any(|hit| hit.path == found) {
                    out.push(CaskHit {
                        token: token.clone(),
                        path: found,
                        trusted,
                    });
                }
            }
        }
        out
    }

    /// 这个 App 的 cask 点名的模式（已替换 Team / APPDIR、通过检查），和各自是不是 `rmdir`。
    /// 给"别的 App 也认领了吗"用：`rmdir` 也算认领，它说明这个目录是共用的。
    pub fn patterns_for(&self, env: &Env, app: &InstalledApp) -> Vec<(String, bool)> {
        let Some((token, _)) = self.token_for(app) else {
            return Vec::new();
        };
        let Some(paths) = self.zap_paths.get(&token) else {
            return Vec::new();
        };
        let team_prefix = app
            .team_id
            .as_deref()
            .map(|t| format!("{t}."))
            .unwrap_or_else(|| "*.".to_string());
        let appdir = app
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        paths
            .iter()
            .filter_map(|cp| {
                let pattern = cp
                    .pattern
                    .replace("$(TeamIdentifierPrefix)", &team_prefix)
                    .replace("$APPDIR", &appdir);
                pattern_is_allowed(env, &pattern).then_some((pattern, cp.only_if_empty))
            })
            .collect()
    }
}

/// cask 里的一条原始路径，展开并检查后的模式；不合格就 None。
fn accepted_cask_pattern(env: &Env, raw: &str) -> Option<String> {
    let mut pattern = raw.trim().to_string();
    if pattern.contains("$HOMEBREW_PREFIX") {
        let prefix = env.homebrew_prefix.as_ref()?;
        pattern = pattern.replace("$HOMEBREW_PREFIX", &prefix.display().to_string());
    }
    // 除了查询时才换的两个变量，别的 `$` 都说明这条路径没法确定。
    let without_known = pattern
        .replace("$(TeamIdentifierPrefix)", "T")
        .replace("$APPDIR", "/A");
    if without_known.contains('$') {
        return None;
    }
    let expanded = env.expand(&pattern).display().to_string();
    pattern_is_allowed(env, &expanded).then_some(expanded)
}

/// 模式必须是绝对路径、没有 `..`、没有 `**`、没有整段只是 `*` 的部分，并且在允许的根下面。
fn pattern_is_allowed(env: &Env, pattern: &str) -> bool {
    if !pattern.starts_with('/') || pattern.contains("**") {
        return false;
    }
    let path = Path::new(pattern);
    for part in path.components() {
        match part {
            Component::ParentDir | Component::CurDir => return false,
            Component::Normal(name) if name == "*" => return false,
            _ => {}
        }
    }
    path_is_allowed(env, path)
}

/// 只认 `~/Library`、`/Library` 和 Caskroom 下面的东西，而且不能是这些根本身。
fn path_is_allowed(env: &Env, path: &Path) -> bool {
    let mut roots = vec![env.user_library.clone(), env.system_library.clone()];
    if let Some(prefix) = &env.homebrew_prefix {
        roots.push(prefix.join("Caskroom"));
    }
    roots
        .iter()
        .any(|root| path.starts_with(root) && path != root)
}

/// 空目录，或只剩 `.DS_Store`。
fn is_empty_dir(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().all(|e| e.file_name() == ".DS_Store")
}

/// 新格式：`[":app", ["Foo.app"]]` → `("app", 参数)`。
fn internal_artifact(value: &serde_json::Value) -> Option<(String, &serde_json::Value)> {
    let items = value.as_array()?;
    let kind = items.first()?.as_str()?.trim_start_matches(':').to_string();
    let args = items.get(1)?;
    Some((kind, args))
}

/// 旧格式：`{"app": ["Foo.app"]}` → `("app", 参数)`。
fn old_artifact(value: &serde_json::Value) -> Option<(String, &serde_json::Value)> {
    let (kind, args) = value.as_object()?.iter().next()?;
    Some((kind.trim_start_matches(':').to_string(), args))
}

/// `app` 段里装进 Applications 的名字：字符串，以及 `target` 指定的改名。
fn app_names(args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    match args {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => out.push(s.clone()),
                    serde_json::Value::Object(map) => {
                        for (k, v) in map {
                            if k.trim_start_matches(':') == "target" {
                                if let Some(s) = v.as_str() {
                                    out.push(s.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out.into_iter()
        .filter(|n| n.ends_with(".app"))
        .map(|n| n.rsplit('/').next().unwrap_or(&n).to_string())
        .collect()
}

/// zap / uninstall 段里的路径：`trash` / `delete` 进 `trash`，`rmdir` 进 `rmdir`；字符串或数组都行。
fn collect_paths(value: &serde_json::Value, trash: &mut Vec<String>, rmdir: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| collect_paths(v, trash, rmdir))
        }
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                let into = match key.trim_start_matches(':') {
                    "trash" | "delete" => &mut *trash,
                    "rmdir" => &mut *rmdir,
                    _ => continue,
                };
                match v {
                    serde_json::Value::String(s) => into.push(s.clone()),
                    serde_json::Value::Array(items) => {
                        into.extend(items.iter().filter_map(|i| i.as_str().map(str::to_string)))
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Size;

    fn app(root: &Path, team: Option<&str>) -> InstalledApp {
        InstalledApp {
            path: root.join("Applications/Foo.app"),
            name: "Foo".into(),
            bundle_id: Some("com.acme.foo".into()),
            version: None,
            executable: None,
            team_id: team.map(str::to_string),
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        }
    }

    #[test]
    fn builtin_rules_all_parse() {
        let files = RULES_DIR
            .files()
            .filter(|f| f.path().extension().is_some_and(|e| e == "toml"))
            .count();
        assert!(files >= 8, "there are at least 8 rule files, found {files}");
        let set = RuleSet::builtin();
        assert_eq!(set.rules.len(), files, "every rule file parses");
        assert!(set.cask.is_none());
        for rule in &set.rules {
            assert!(!rule.paths.is_empty(), "{} declares paths", rule.bundle_id);
            for p in &rule.paths {
                assert!(
                    p.path.starts_with("~/") || p.path.starts_with('/'),
                    "{}: {}",
                    rule.bundle_id,
                    p.path
                );
            }
        }
        let docker = set
            .rules
            .iter()
            .find(|r| r.bundle_id == "com.docker.docker")
            .unwrap();
        let dot_docker = docker.paths.iter().find(|p| p.path == "~/.docker").unwrap();
        assert_eq!(
            dot_docker.confidence,
            Confidence::Possible,
            "shared with the docker CLI, so never pre-selected"
        );
    }

    #[test]
    fn parses_and_matches_rule_paths() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let app = app(dir.path(), None);
        let rule = RuleSet::parse(
            r#"
bundle_id = "com.acme.foo"
name = "Foo"
aliases = ["Foo Classic"]

[[paths]]
path = "~/Library/Application Support/FooData"
confidence = "certain"
note = "settings"

[[paths]]
path = "~/Library/Group Containers/*.com.acme.foo"
confidence = "likely"

[[paths]]
path = "~/Library/Caches/Acme/Foo*"
confidence = "likely"
"#,
        )
        .unwrap();
        assert_eq!(rule.aliases, vec!["Foo Classic"]);
        let set = RuleSet {
            rules: vec![rule],
            cask: None,
        };
        assert_eq!(set.for_app(&app).len(), 1);
        assert_eq!(
            set.mentioned_folders(&app),
            vec![
                "acme",
                "applicationsupport",
                "caches",
                "foodata",
                "groupcontainers",
                "library"
            ]
        );

        let exact = env.user_library.join("Application Support/FooData");
        let e = set
            .evidence(&env, &app, &exact)
            .expect("exact path matches");
        assert_eq!(e.confidence, Confidence::Certain);
        assert_eq!(e.kind, EvidenceKind::Rule { rule: "Foo".into() });

        let globbed = env
            .user_library
            .join("Group Containers/ABC123.com.acme.foo");
        assert_eq!(
            set.evidence(&env, &app, &globbed).unwrap().confidence,
            Confidence::Likely
        );
        assert!(set
            .evidence(
                &env,
                &app,
                &env.user_library
                    .join("Group Containers/ABC123.com.acme.foo/sub")
            )
            .is_none());
        assert!(set
            .evidence(
                &env,
                &app,
                &env.user_library.join("Application Support/Other")
            )
            .is_none());

        assert!(
            set.declared_paths(&env, &app).is_empty(),
            "nothing on disk yet"
        );
        std::fs::create_dir_all(&globbed).unwrap();
        std::fs::create_dir_all(&exact).unwrap();
        assert_eq!(set.declared_paths(&env, &app), vec![exact, globbed]);
    }

    fn write_internal_payload(env: &Env, casks: serde_json::Value) {
        let internal = env.user_library.join("Caches/Homebrew/api/internal");
        std::fs::create_dir_all(&internal).unwrap();
        let payload = serde_json::json!({
            "metadata": {"homebrew_version": "7.0"},
            "formulae": {"a2ps": {"desc": "x"}},
            "casks": casks
        });
        let text = format!("{{\"protected\":\"x\",\"signature\":\"y\"}}\n{payload}");
        std::fs::write(internal.join("packages.arm64_test.jws.json.payload"), text).unwrap();
    }

    #[test]
    fn reads_new_homebrew_payload_and_substitutes_team() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        write_internal_payload(
            &env,
            serde_json::json!({
                "foo": {
                    "names": ["Foo"],
                    "raw_artifacts": [
                        [":uninstall", {":quit": "com.acme.foo", ":delete": format!("{}/PrivilegedHelperTools/com.acme.foo.helper", env.system_library.display())}],
                        [":app", ["Foo.app"]],
                        [":zap", {":trash": [
                            "~/Library/Caches/com.acme.foo",
                            "~/Library/Group Containers/$(TeamIdentifierPrefix)com.acme.foo",
                            "~/Library/Preferences/com.acme.foo*.plist"
                        ], ":rmdir": "~/Library/Application Support/Foo"}]
                    ]
                },
                "lark": {"raw_artifacts": [[":app", ["Lark.app", {":target": "Feishu.app"}]]]}
            }),
        );

        let index = CaskIndex::load(&env).expect("the index loads");
        assert_eq!(
            index.app_tokens.get("Foo.app").map(String::as_str),
            Some("foo")
        );
        assert_eq!(
            index.app_tokens.get("Feishu.app").map(String::as_str),
            Some("lark")
        );
        assert_eq!(
            index.app_tokens.get("Lark.app").map(String::as_str),
            Some("lark")
        );
        let foo = &index.zap_paths["foo"];
        let patterns: Vec<&str> = foo.iter().map(|p| p.pattern.as_str()).collect();
        assert!(patterns.contains(
            &env.user_library
                .join("Caches/com.acme.foo")
                .to_str()
                .unwrap()
        ));
        assert!(patterns.contains(
            &env.system_library
                .join("PrivilegedHelperTools/com.acme.foo.helper")
                .to_str()
                .unwrap()
        ));
        let rmdir = foo
            .iter()
            .find(|p| p.pattern.ends_with("Application Support/Foo"))
            .expect("rmdir is kept");
        assert!(rmdir.only_if_empty);

        // 磁盘上什么都没有 → 什么都不返回。
        assert!(index
            .paths_for(&env, &app(dir.path(), Some("TEAM1")))
            .is_empty());

        std::fs::create_dir_all(env.user_library.join("Caches/com.acme.foo")).unwrap();
        std::fs::create_dir_all(env.user_library.join("Group Containers/TEAM1.com.acme.foo"))
            .unwrap();
        std::fs::create_dir_all(env.user_library.join("Group Containers/OTHER.com.acme.foo"))
            .unwrap();
        std::fs::create_dir_all(env.user_library.join("Preferences")).unwrap();
        std::fs::write(
            env.user_library
                .join("Preferences/com.acme.foo.helper.plist"),
            b"",
        )
        .unwrap();

        let paths = |team: Option<&str>| -> Vec<PathBuf> {
            index
                .paths_for(&env, &app(dir.path(), team))
                .into_iter()
                .map(|h| h.path)
                .collect()
        };
        let with_team = paths(Some("TEAM1"));
        assert!(with_team.contains(&env.user_library.join("Caches/com.acme.foo")));
        assert!(with_team.contains(&env.user_library.join("Group Containers/TEAM1.com.acme.foo")));
        assert!(
            !with_team.contains(&env.user_library.join("Group Containers/OTHER.com.acme.foo")),
            "another team's"
        );
        assert!(
            with_team.contains(
                &env.user_library
                    .join("Preferences/com.acme.foo.helper.plist")
            ),
            "globs expand"
        );

        let no_team = paths(None);
        assert!(
            no_team.contains(&env.user_library.join("Group Containers/TEAM1.com.acme.foo")),
            "unknown team → any team"
        );
        assert!(no_team.contains(&env.user_library.join("Group Containers/OTHER.com.acme.foo")));

        let by_name = index.paths_for(&env, &app(dir.path(), None));
        assert!(
            by_name.iter().all(|h| !h.trusted),
            "found by the .app name, so only a hint"
        );
        let mut brew_app = app(dir.path(), None);
        brew_app.path = dir.path().join("Applications/Renamed.app");
        brew_app.source = AppSource::Homebrew("foo".into());
        let by_source = index.paths_for(&env, &brew_app);
        assert!(
            !by_source.is_empty(),
            "the source's token wins over the file name"
        );
        assert!(by_source.iter().all(|h| h.trusted));
    }

    #[test]
    fn rmdir_counts_only_for_an_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        write_internal_payload(
            &env,
            serde_json::json!({"foo": {"raw_artifacts": [
                [":app", ["Foo.app"]],
                [":zap", {":rmdir": ["~/Library/Application Support/Acme", "~/Library/Caches/Acme"]}]
            ]}}),
        );
        let full = env.user_library.join("Application Support/Acme");
        std::fs::create_dir_all(full.join("Other App")).unwrap();
        let empty = env.user_library.join("Caches/Acme");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join(".DS_Store"), b"").unwrap();

        let index = CaskIndex::load(&env).unwrap();
        let paths: Vec<PathBuf> = index
            .paths_for(&env, &app(dir.path(), None))
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(
            paths,
            vec![empty],
            "a folder with another app's things in it is not Foo's to take"
        );
        let claimed: Vec<String> = index
            .patterns_for(&env, &app(dir.path(), None))
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            claimed.len(),
            2,
            "for sharing purposes both folders count as claimed"
        );
    }

    #[test]
    fn cask_paths_outside_library_or_with_wild_globs_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env::for_test(dir.path());
        let prefix = dir.path().join("opt/homebrew");
        std::fs::create_dir_all(prefix.join("Caskroom")).unwrap();
        env.homebrew_prefix = Some(prefix.clone());
        write_internal_payload(
            &env,
            serde_json::json!({"foo": {"raw_artifacts": [[":zap", {":trash": [
                "~/Documents/bruno",
                "~/Music/Ableton",
                "~/Creative Cloud Files",
                "/Users/Shared/Adobe",
                "~/*",
                "~/Library/*",
                "~/Library/**/Foo",
                "~/Library/Application Support/../Keychains",
                "~/Library",
                "$HOMEBREW_PREFIX/share/foo",
                "$HOMEBREW_PREFIX/Caskroom/foo-extra",
                "$UNKNOWN/thing",
                "relative/path",
                "~/Library/Application Support/Foo*",
                format!("{}/LaunchAgents/com.acme.foo.plist", env.system_library.display()),
                "/Library/LaunchAgents/com.acme.foo.plist",
                "$APPDIR/Foo Helper.app"
            ]}]]}}),
        );
        let index = CaskIndex::load(&env).unwrap();
        let mut kept: Vec<String> = index.zap_paths["foo"]
            .iter()
            .map(|p| p.pattern.clone())
            .collect();
        kept.sort();
        let mut expected = vec![
            env.system_library
                .join("LaunchAgents/com.acme.foo.plist")
                .display()
                .to_string(),
            env.user_library
                .join("Application Support/Foo*")
                .display()
                .to_string(),
            prefix.join("Caskroom/foo-extra").display().to_string(),
        ];
        expected.sort();
        assert_eq!(kept, expected);

        let mut no_brew = Env::for_test(dir.path());
        no_brew.homebrew_prefix = None;
        let index = CaskIndex::load(&no_brew).unwrap();
        assert_eq!(
            index.zap_paths["foo"].len(),
            2,
            "without Homebrew, $HOMEBREW_PREFIX paths are unknown and dropped"
        );
    }

    #[test]
    fn same_app_name_in_several_casks_picks_the_same_one_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        write_internal_payload(
            &env,
            serde_json::json!({
                "firefox@beta": {"raw_artifacts": [[":app", ["Firefox.app"]]]},
                "firefox": {"raw_artifacts": [[":app", ["Firefox.app"]]]},
                "firefox@esr": {"raw_artifacts": [[":app", ["Firefox.app"]]]},
                "broken": {"raw_artifacts": null},
                "weird": "not an object"
            }),
        );
        let index = CaskIndex::load(&env).expect("broken casks do not sink the index");
        assert_eq!(index.app_tokens["Firefox.app"], "firefox");
    }

    #[test]
    fn reads_old_homebrew_cask_json() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        let api = env.user_library.join("Caches/Homebrew/api");
        std::fs::create_dir_all(&api).unwrap();
        let casks = serde_json::json!([
            {"token": "foo", "artifacts": [
                {"app": ["Foo.app"]},
                {"zap": [{"trash": ["~/Library/Caches/com.acme.foo"], "rmdir": ["~/Library/Foo"]}]},
                {"uninstall": [{"launchctl": "com.acme.foo.agent", "delete": format!("{}/LaunchDaemons/com.acme.foo.plist", env.system_library.display())}]}
            ]},
            {"token": "bad"},
            42
        ]);
        let wrapper = serde_json::json!({"payload": casks.to_string(), "signature": "s"});
        std::fs::write(api.join("cask.jws.json"), wrapper.to_string()).unwrap();

        let index = CaskIndex::load(&env).unwrap();
        assert_eq!(index.app_tokens["Foo.app"], "foo");
        let foo = &index.zap_paths["foo"];
        assert_eq!(foo.len(), 3);
        assert!(foo
            .iter()
            .any(|p| p.pattern.ends_with("LaunchDaemons/com.acme.foo.plist") && !p.only_if_empty));
        assert!(foo
            .iter()
            .any(|p| p.pattern.ends_with("Library/Foo") && p.only_if_empty));
    }

    #[test]
    fn no_homebrew_cache_means_no_index() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::for_test(dir.path());
        assert!(CaskIndex::load(&env).is_none());
        assert!(RuleSet::with_casks(&env).cask.is_none());
        assert!(!RuleSet::with_casks(&env).rules.is_empty());
    }
}
