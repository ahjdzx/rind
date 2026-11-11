//! `rind` 的端到端测试：在临时目录里搭一个假的家目录和 `.app`，用 `RIND_HOME` / `RIND_ROOT`
//! 把 Rind 指过去。`uninstall --yes` 会真的把东西移进用户的废纸篓，所以每个这样的测试都把它们
//! 放回来，并有一个 `Drop` 守卫兜底。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 一个假的家目录：`Users/tester`，里面有 `Applications/FakeApp.app` 和它的三处残留。
struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    app: PathBuf,
    leftovers: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        Self::at(tempfile::tempdir().expect("tempdir"))
    }

    /// 会真进废纸篓的测试用：暂存目录建在真实家目录下，和废纸篓同卷，移动是 rename。
    fn on_trash_volume() -> Self {
        let base =
            PathBuf::from(std::env::var("HOME").expect("HOME")).join("Library/Caches/rind-tests");
        fs::create_dir_all(&base).unwrap();
        Self::at(tempfile::tempdir_in(base).expect("tempdir"))
    }

    fn at(dir: tempfile::TempDir) -> Self {
        // `/var` 是指向 `/private/var` 的符号链接；给 Rind 的路径用盘上的真名，免得保护区检查认不出来。
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let home = root.join("Users/tester");
        let app = make_app(&home.join("Applications"), "FakeApp", "com.rind.fake");
        let library = home.join("Library");
        let leftovers = vec![
            library.join("Caches/com.rind.fake/blob"),
            library.join("Preferences/com.rind.fake.plist"),
            library.join("Application Support/com.rind.fake/data"),
        ];
        for path in &leftovers {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"leftover").unwrap();
        }
        Self {
            _root: dir,
            root,
            home,
            app,
            leftovers,
        }
    }

    fn rind(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rind"));
        cmd.env("RIND_HOME", &self.home)
            .env("RIND_ROOT", &self.root)
            .env("RIND_LANG", "zh")
            .env_remove("RIND_DEMO");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.rind().args(args).output().expect("run rind")
    }

    fn json(&self, args: &[&str]) -> (Output, serde_json::Value) {
        let output = self.run(args);
        let text = String::from_utf8_lossy(&output.stdout);
        let value = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!(
                "stdout is not one JSON document ({e}):\n{text}\nstderr:\n{}",
                stderr(&output)
            )
        });
        (output, value)
    }

    fn everything_in_place(&self) -> bool {
        self.app.exists() && self.leftovers.iter().all(|p| p.exists())
    }
}

/// 测试没走完就把东西留在废纸篓里的话，这里按 History 把它们放回去。
struct PutBack<'a>(&'a Fixture);

impl Drop for PutBack<'_> {
    fn drop(&mut self) {
        // 不只看 App 和残留：清理、移文件的测试也靠它兜底，所以每条没放回的记录都放回。
        let output = self.0.run(&["history", "--json"]);
        let Ok(records) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
            return;
        };
        for record in records {
            if record["restored"].as_bool() == Some(true) {
                continue;
            }
            if let Some(id) = record["id"].as_str() {
                let _ = self.0.run(&["restore", id]);
            }
        }
    }
}

fn make_app(dir: &Path, name: &str, bundle_id: &str) -> PathBuf {
    let app = dir.join(format!("{name}.app"));
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleName</key><string>{name}</string>
<key>CFBundleShortVersionString</key><string>0.1</string>
<key>CFBundleExecutable</key><string>{name}</string>
</dict></plist>"#
        ),
    )
    .unwrap();
    fs::write(app.join(format!("Contents/MacOS/{name}")), b"#!/bin/sh\n").unwrap();
    app
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn apps_lists_the_fake_app_as_text_and_json() {
    let fx = Fixture::new();
    let output = fx.run(&["apps"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FakeApp"), "{text}");
    assert!(text.contains("com.rind.fake"), "{text}");

    let (output, value) = fx.json(&["apps", "--json"]);
    assert!(output.status.success());
    let ids: Vec<&str> = value
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["bundle_id"].as_str())
        .collect();
    assert!(ids.contains(&"com.rind.fake"), "{ids:?}");
}

#[test]
fn leftovers_json_names_every_residue_and_suggests_them() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["leftovers", "FakeApp", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let paths: Vec<PathBuf> = value["plan"]["leftovers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| PathBuf::from(m["path"].as_str().unwrap()))
        .collect();
    let caches = fx.home.join("Library/Caches/com.rind.fake");
    let prefs = fx.home.join("Library/Preferences/com.rind.fake.plist");
    assert!(paths.contains(&caches), "{paths:?}");
    assert!(paths.contains(&prefs), "{paths:?}");
    assert_eq!(
        value["selection"]["include_app"],
        serde_json::Value::Bool(true)
    );
    let suggested = value["selection"]["paths"].as_array().unwrap();
    assert!(
        suggested
            .iter()
            .any(|p| p.as_str() == Some(caches.to_str().unwrap())),
        "{suggested:?}"
    );
}

#[test]
fn text_output_fills_every_placeholder() {
    let fx = Fixture::new();
    let output = fx.run(&["leftovers", "FakeApp"]);
    assert!(output.status.success(), "{}", stderr(&output));
    for stream in [stdout(&output), stderr(&output)] {
        for placeholder in ["{location}", "{done}", "{total}", "{n}", "{size}"] {
            assert!(
                !stream.contains(placeholder),
                "unfilled {placeholder} in:\n{stream}"
            );
        }
    }
    assert!(stdout(&output).contains("已选 4 项"), "{}", stdout(&output));
}

#[test]
fn dry_run_json_is_one_document_and_moves_nothing() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["uninstall", "FakeApp", "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(value["dry_run"], serde_json::Value::Bool(true));
    assert!(value["record_id"].is_null());
    let moved = value["moved"].as_array().unwrap();
    assert_eq!(moved.len(), 4, "app + 3 leftovers: {moved:?}");
    assert!(fx.everything_in_place(), "dry run must not move anything");
}

#[test]
fn dry_run_text_says_it_is_a_preview() {
    let fx = Fixture::new();
    let output = fx.run(&["uninstall", "FakeApp", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("预演"), "{text}");
    assert!(text.contains("将释放"), "{text}");
    assert!(fx.everything_in_place());
}

#[test]
fn json_without_yes_refuses_instead_of_asking() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["uninstall", "FakeApp", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        value["error"].as_str().unwrap().contains("--yes"),
        "{value}"
    );
    assert!(fx.everything_in_place());
}

#[test]
fn an_ambiguous_name_lists_each_match_with_its_path() {
    let fx = Fixture::new();
    make_app(
        &fx.root.join("Applications"),
        "FakeApp Beta",
        "com.rind.fakebeta",
    );
    let output = fx.run(&["uninstall", "Fake", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("不止一个"), "{err}");
    assert!(err.contains("~/Applications/FakeApp.app"), "{err}");
    assert!(err.contains("FakeApp Beta.app"), "{err}");

    // 名字完全相等的那个优先，不算歧义。
    let output = fx.run(&["uninstall", "FakeApp", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn an_unknown_name_is_an_error() {
    let fx = Fixture::new();
    let output = fx.run(&["leftovers", "NoSuchApp"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("没找到"), "{}", stderr(&output));
}

#[test]
fn uninstall_records_history_and_restore_puts_everything_back() {
    let fx = Fixture::new();
    let _guard = PutBack(&fx);

    let (output, value) = fx.json(&["uninstall", "FakeApp", "--yes", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(value["dry_run"], serde_json::Value::Bool(false));
    let moved = value["moved"].as_array().unwrap();
    assert_eq!(moved.len(), 4, "{moved:?}");
    assert!(value["failed"].as_array().unwrap().is_empty(), "{value}");
    let record_id = value["record_id"].as_str().expect("record id").to_string();
    assert!(!fx.app.exists(), "the app should be in the Trash");
    for path in &fx.leftovers {
        assert!(!path.exists(), "{} should be in the Trash", path.display());
    }

    let (output, history) = fx.json(&["history", "--json"]);
    assert!(output.status.success());
    let records = history.as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["id"].as_str(), Some(record_id.as_str()));
    assert_eq!(records[0]["restored"], serde_json::Value::Bool(false));
    assert_eq!(records[0]["items"].as_array().unwrap().len(), 4);

    // 只给 id 的前八位也行。
    let short: String = record_id.chars().take(8).collect();
    let output = fx.run(&["restore", &short]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("放回了 4 项"),
        "{}",
        stdout(&output)
    );
    assert!(
        fx.everything_in_place(),
        "everything should be back where it was"
    );

    let (_, history) = fx.json(&["history", "--json"]);
    assert_eq!(history[0]["restored"], serde_json::Value::Bool(true));

    // 再放一次：没有东西可放，退出码 3。
    let output = fx.run(&["restore", &short]);
    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
}

#[test]
fn history_is_empty_before_anything_moved() {
    let fx = Fixture::new();
    let output = fx.run(&["history"]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("还没有移动过"),
        "{}",
        stdout(&output)
    );
    let (_, value) = fx.json(&["history", "--json"]);
    assert!(value.as_array().unwrap().is_empty());
}

#[test]
fn protected_lists_the_keychain() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["protected", "--json"]);
    assert!(output.status.success());
    let reasons: Vec<&str> = value
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["reason"].as_str())
        .collect();
    assert!(reasons.contains(&"keychain"), "{reasons:?}");
}

#[test]
fn a_loose_match_is_only_taken_when_someone_is_watching() {
    let fx = Fixture::new();
    // 没人看着（--yes / --json）：模糊命中不算数，列出它、要求完整名字。
    let output = fx.run(&["uninstall", "Fake", "--dry-run", "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("模糊对上了"), "{err}");
    assert!(err.contains("FakeApp"), "{err}");
    let output = fx.run(&["leftovers", "Fake", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("\"error\""), "{}", stdout(&output));

    // 有人看着：选中，但在 stderr 里说清楚选了谁。
    let output = fx.run(&["leftovers", "Fake"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("按模糊匹配选中了"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("FakeApp"), "{}", stdout(&output));
}

#[test]
fn an_exact_name_with_an_app_suffix_survives_the_no_prompt_rule() {
    let fx = Fixture::new();
    make_app(
        &fx.root.join("Applications"),
        "FakeApp Beta",
        "com.rind.fakebeta",
    );

    // 场上还有一个 "FakeApp Beta" 的模糊命中；".app" 后缀、结尾斜杠和完整路径都不算模糊，
    // 没人看着（--json / --yes）也照常接受。
    for query in [
        "FakeApp.app".to_string(),
        format!("{}/", fx.app.display()),
        fx.app.display().to_string(),
    ] {
        let (output, value) = fx.json(&["leftovers", &query, "--no-size", "--json"]);
        assert!(output.status.success(), "{query}: {}", stderr(&output));
        assert_eq!(
            value["plan"]["app"]["name"].as_str(),
            Some("FakeApp"),
            "{query}"
        );
    }
}

/// 写一个 `size` 字节的真文件（非稀疏，占用块数和大小相当）。
fn write_bytes(path: &Path, size: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![7u8; size]).unwrap();
}

fn paths_of(list: &serde_json::Value, key: usize) -> Vec<PathBuf> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|pair| PathBuf::from(pair[key].as_str().unwrap()))
        .collect()
}

#[test]
fn status_json_has_disk_and_app_count() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["status", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let disk = &value["disk"];
    assert!(disk["total"].as_u64().unwrap() > 0, "{value}");
    assert!(disk["available"].as_u64().unwrap() <= disk["total"].as_u64().unwrap());
    assert!(disk["used"].is_u64());
    assert_eq!(value["apps"].as_u64(), Some(1), "{value}");
    assert!(value["scan"].is_null(), "{value}");

    let (output, value) = fx.json(&["status", "--scan", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let scan = &value["scan"];
    for key in ["orphans", "cleanup"] {
        for field in [
            "items",
            "size",
            "unknown",
            "suggested_items",
            "suggested_size",
        ] {
            assert!(scan[key][field].is_u64(), "{key}.{field}: {value}");
        }
    }
    assert!(scan["orphan_groups"].is_u64(), "{value}");
    assert!(scan["reclaimable"].is_u64(), "{value}");
    let categories = scan["categories"].as_array().unwrap();
    assert!(
        categories.iter().any(|c| c["category"] == "app_caches"),
        "{value}"
    );
}

#[test]
fn clean_dry_run_lists_app_and_npm_caches_and_moves_nothing() {
    let fx = Fixture::new();
    let npm = fx.home.join(".npm/_cacache");
    write_bytes(&npm.join("index-v5/blob"), 4096);
    let caches = fx.home.join("Library/Caches/com.rind.fake");

    let (output, value) = fx.json(&["clean", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let categories: Vec<&str> = value["categories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["category"].as_str())
        .collect();
    assert!(categories.contains(&"app_caches"), "{value}");
    assert!(categories.contains(&"package_caches"), "{value}");
    assert!(value["categories"][0]["explanation"].is_string(), "{value}");

    let (output, value) = fx.json(&["clean", "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(value["dry_run"], serde_json::Value::Bool(true));
    let moved = paths_of(&value["moved"], 0);
    assert!(moved.contains(&caches), "{moved:?}");
    assert!(moved.contains(&npm), "{moved:?}");
    assert!(caches.exists() && npm.exists());

    // 只要一类。
    let (_, value) = fx.json(&[
        "clean",
        "--category",
        "package_caches",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(paths_of(&value["moved"], 0), vec![npm.clone()], "{value}");

    // 类别名不对：clap 直接拒绝并列出能用的。
    let output = fx.run(&["clean", "--category", "junk"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("app_caches"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn clean_trash_records_cleanup_history_and_restore_puts_it_back() {
    let fx = Fixture::on_trash_volume();
    let _guard = PutBack(&fx);
    let npm = fx.home.join(".npm/_cacache");
    write_bytes(&npm.join("index-v5/blob"), 4096);
    let caches = fx.home.join("Library/Caches/com.rind.fake");

    let (output, value) = fx.json(&["clean", "--trash", "--yes", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(value["dry_run"], serde_json::Value::Bool(false));
    let moved = paths_of(&value["moved"], 0);
    assert!(moved.contains(&caches) && moved.contains(&npm), "{moved:?}");
    assert!(!caches.exists() && !npm.exists());
    let record_id = value["record_id"].as_str().expect("record id").to_string();

    let (_, history) = fx.json(&["history", "--json"]);
    assert_eq!(history[0]["id"].as_str(), Some(record_id.as_str()));
    assert_eq!(history[0]["kind"].as_str(), Some("cleanup"), "{history}");
    let text = stdout(&fx.run(&["history"]));
    assert!(text.contains("清理"), "{text}");

    let output = fx.run(&["restore", &record_id]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(caches.exists() && npm.exists(), "both should be back");
}

#[test]
fn analyze_sorts_by_size_and_reports_large_files() {
    let fx = Fixture::new();
    let dir = fx.home.join("space");
    write_bytes(&dir.join("big/a.bin"), 3_000_000);
    write_bytes(&dir.join("mid.bin"), 2_000_000);
    write_bytes(&dir.join("small/b.txt"), 10_000);

    let (output, value) = fx.json(&[
        "analyze",
        dir.to_str().unwrap(),
        "--depth",
        "1",
        "--large-min",
        "1M",
        "--json",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(value["incomplete"], serde_json::Value::Bool(false));
    assert!(value["unreadable"].as_array().unwrap().is_empty());
    let names: Vec<&str> = value["root"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["big", "mid.bin", "small"], "{value}");
    assert_eq!(value["root"]["files"].as_u64(), Some(3));
    let large: Vec<&str> = value["large_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(large.len(), 2, "{large:?}");
    assert!(large[0].ends_with("a.bin") && large[1].ends_with("mid.bin"));

    let output = fx.run(&["analyze", dir.to_str().unwrap(), "--depth", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} in {text}"));
    assert!(
        at("big/") < at("mid.bin") && at("mid.bin") < at("small/"),
        "{text}"
    );

    let output = fx.run(&["analyze", "--large-min", "lots"]);
    assert!(!output.status.success());
}

#[test]
fn trash_refuses_protected_paths_and_can_be_restored() {
    let fx = Fixture::on_trash_volume();
    let _guard = PutBack(&fx);
    let keychains = fx.home.join("Library/Keychains");
    fs::create_dir_all(&keychains).unwrap();
    let output = fx.run(&["trash", keychains.to_str().unwrap(), "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("钥匙串"), "{}", stderr(&output));
    assert!(keychains.exists());

    let note = fx.home.join("Documents/note.txt");
    write_bytes(&note, 100);
    let (output, value) = fx.json(&["trash", note.to_str().unwrap(), "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(paths_of(&value["moved"], 0), vec![note.clone()]);
    assert!(note.exists());

    // 一个能动、一个不能：能动的移走，不能的列在 refused 里，退出码 1。
    let (output, value) = fx.json(&[
        "trash",
        note.to_str().unwrap(),
        keychains.to_str().unwrap(),
        "--yes",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1), "{value}");
    assert_eq!(paths_of(&value["moved"], 0), vec![note.clone()]);
    assert_eq!(paths_of(&value["refused"], 0), vec![keychains.clone()]);
    assert!(!note.exists() && keychains.exists());

    let (_, history) = fx.json(&["history", "--json"]);
    assert_eq!(history[0]["kind"].as_str(), Some("files"), "{history}");
    let id = history[0]["id"].as_str().unwrap().to_string();
    let output = fx.run(&["restore", &id]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(note.exists());

    let output = fx.run(&["trash", fx.home.join("nope").to_str().unwrap(), "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("不存在"), "{}", stderr(&output));
}

#[test]
fn launch_list_shows_a_user_agent_and_explains_what_it_cannot_touch() {
    let fx = Fixture::new();
    let plist = |label: &str, program: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>Program</key><string>{program}</string>
<key>RunAtLoad</key><true/>
</dict></plist>"#
        )
    };
    let agents = fx.home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    let agent = agents.join("com.rind.test.agent.plist");
    fs::write(
        &agent,
        plist("com.rind.test.agent", "/nonexistent/rind-test-agent"),
    )
    .unwrap();
    let global = fx.root.join("Library/LaunchAgents");
    fs::create_dir_all(&global).unwrap();
    fs::write(
        global.join("com.rind.test.global.plist"),
        plist("com.rind.test.global", "/nonexistent/rind-test-global"),
    )
    .unwrap();

    let (output, value) = fx.json(&["launch", "list", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let item = value
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "com.rind.test.agent")
        .unwrap_or_else(|| panic!("agent listed: {value}"));
    assert_eq!(item["domain"].as_str(), Some("user_agent"));
    assert_eq!(item["plist"].as_str(), agent.to_str());
    assert_eq!(item["program_exists"], serde_json::Value::Bool(false));
    assert!(item["status"]["state"].is_string(), "{item}");

    let (_, value) = fx.json(&["launch", "list", "--user", "--json"]);
    assert!(value
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["domain"] == "user_agent"));

    let text = stdout(&fx.run(&["launch", "list"]));
    assert!(text.contains("com.rind.test.agent"), "{text}");

    // 找不到、不能管：在调用 launchctl 之前就说明原因。
    let output = fx.run(&["launch", "disable", "com.rind.no.such"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("没有 label"),
        "{}",
        stderr(&output)
    );
    let output = fx.run(&["launch", "disable", "COM.RIND.TEST.GLOBAL"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("不能操作"), "{}", stderr(&output));
    let output = fx.run(&["launch", "remove", "com.rind.test.global", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(fx
        .root
        .join("Library/LaunchAgents/com.rind.test.global.plist")
        .exists());
}

#[test]
fn orphans_json_has_groups_and_selection() {
    let fx = Fixture::new();
    let (output, value) = fx.json(&["orphans", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(value["groups"].is_array(), "{value}");
    assert!(value["total"].is_u64(), "{value}");
    assert!(value["selected"].is_array(), "{value}");
    assert!(value["selected_size"].is_u64(), "{value}");

    // 预演：输出一个移动结果文档（没选中任何东西时退出码 3），什么都不动。
    let (output, value) = fx.json(&["orphans", "--trash", "--dry-run", "--json"]);
    assert!(
        matches!(output.status.code(), Some(0) | Some(3)),
        "{}",
        stderr(&output)
    );
    assert_eq!(value["dry_run"], serde_json::Value::Bool(true), "{value}");
    assert!(value["moved"].is_array(), "{value}");
    assert!(fx.everything_in_place());

    // --path 只能选扫描结果里的。
    let stranger = fx.home.join("Library/Caches/com.rind.fake");
    let output = fx.run(&["orphans", "--path", stranger.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("不在扫描结果里"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn json_without_yes_refuses_for_every_moving_command() {
    let fx = Fixture::new();
    let note = fx.home.join("note.txt");
    write_bytes(&note, 10);
    for args in [
        vec!["clean", "--trash", "--json"],
        vec!["orphans", "--trash", "--json"],
        vec!["trash", note.to_str().unwrap(), "--json"],
        vec!["launch", "remove", "com.rind.whatever", "--json"],
    ] {
        let (output, value) = fx.json(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            value["error"].as_str().unwrap().contains("--yes"),
            "{args:?}: {value}"
        );
    }
    assert!(fx.everything_in_place());
    assert!(note.exists());
}
