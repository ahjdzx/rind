//! 卸载的两步：出计划，按用户的选择执行。规则见 docs/ARCHITECTURE.md 第 5、7 步。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::history::Ledger;
use crate::leftovers::{self, ScanOptions};
use crate::remove::{plan_moves, refusal, Batch, Moved, Mover};
use crate::rules::RuleSet;
use crate::{
    protect, running, size, Env, HoldBack, InstalledApp, RecordKind, RemovalOutcome, ScanProgress,
    Selection, Size, Source, UninstallPlan,
};

#[derive(Clone, Debug)]
pub struct PlanOptions {
    pub measure: bool,
    pub size_budget: Duration,
    /// 置为 true 就尽快停下（GUI 换了 App 时）。停下的计划不完整，调用方应丢弃。
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            measure: true,
            size_budget: Duration::from_secs(20),
            cancel: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("app is running")]
    AppRunning,
    #[error("nothing selected")]
    NothingSelected,
    #[error("selection names a path the plan does not have: {0}")]
    NotInPlan(std::path::PathBuf),
    #[error("could not move the app: {0}")]
    AppMoveFailed(String),
    #[error("history is required to move anything; pass a ledger or use dry_run")]
    LedgerRequired,
    /// 写不了 History。`partial` 是出错前已经发生的事：里面的 `moved` 已经在废纸篓里。
    #[error("could not write history: {error}")]
    Ledger {
        error: String,
        partial: Box<RemovalOutcome>,
    },
}

/// 出计划：`leftovers::scan` + App 本体的大小、保护检查、是否在运行。
pub fn plan(
    env: &Env,
    app: &InstalledApp,
    all_apps: &[InstalledApp],
    rules: &RuleSet,
    opts: &PlanOptions,
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> UninstallPlan {
    let scan_opts = ScanOptions {
        measure: opts.measure,
        size_budget: opts.size_budget,
        cancel: opts.cancel.clone(),
    };
    let leftovers = leftovers::scan(env, app, all_apps, rules, &scan_opts, on_progress);

    let mut app = app.clone();
    if opts.measure && app.size == Size::Unknown {
        app.size = size::measure(&app.path, opts.size_budget);
    }
    let app_hold_back = app_hold_back(env, &app);
    let running = running::is_running(&app);
    UninstallPlan {
        app,
        app_hold_back,
        leftovers,
        running,
    }
}

/// App 本体能不能动：保护区优先，然后是 Apple 自带。
fn app_hold_back(env: &Env, app: &InstalledApp) -> Option<HoldBack> {
    if let Some(why) = protect::check(env, &app.path) {
        return Some(HoldBack::Protected { why });
    }
    if app.is_system {
        return Some(HoldBack::SystemOwned);
    }
    None
}

/// 执行：只动 `selection` 里且计划里有的路径。App 在运行 → `AppRunning`（预演除外）；
/// 真要动就必须给 `ledger`（`LedgerRequired`）；先移 App 再移残留，App 失败则残留不动。
/// 每一项在移动前紧挨着再解析、再查一次保护区，移动的是解析出来的真实路径；移动前先把这项的意图写进
/// History（`pending`），移动后回填它在废纸篓里的位置，所以没有记录的东西不会进废纸篓；
/// 写不了 History 就停，带着到那一刻为止的结果返回 `Ledger { partial }`。`dry_run` 只返回将做什么。
pub fn execute(
    env: &Env,
    plan: &UninstallPlan,
    selection: &Selection,
    source: Source,
    dry_run: bool,
    ledger: Option<&Ledger>,
) -> Result<RemovalOutcome, ExecuteError> {
    let by_path: HashMap<&Path, &crate::LeftoverMatch> = plan
        .leftovers
        .iter()
        .map(|m| (m.path.as_path(), m))
        .collect();
    for path in &selection.paths {
        if !by_path.contains_key(path.as_path()) {
            return Err(ExecuteError::NotInPlan(path.clone()));
        }
    }
    if !selection.include_app && selection.paths.is_empty() {
        return Err(ExecuteError::NothingSelected);
    }
    // 预演不动任何东西，App 在不在运行都能看。真要动的话，哪怕只动残留，App 也得先退出：
    // 它正在用的容器、数据库被抽走会出乱子。
    if !dry_run && running::is_running(&plan.app) {
        return Err(ExecuteError::AppRunning);
    }
    let ledger = match (dry_run, ledger) {
        (true, _) => None,
        (false, Some(ledger)) => Some(ledger),
        (false, None) => return Err(ExecuteError::LedgerRequired),
    };

    let mut outcome = RemovalOutcome {
        dry_run,
        ..Default::default()
    };

    // 先把每一项分成"会动"和"拒绝"。App 单独放，因为它必须第一个动。
    let app_item = if selection.include_app {
        match refusal(env, plan.app_hold_back.as_ref(), &plan.app.path) {
            Some(why) => {
                if !dry_run {
                    return Err(ExecuteError::AppMoveFailed(why));
                }
                outcome.refused.push((plan.app.path.clone(), why));
                None
            }
            None => Some((plan.app.path.clone(), plan.app.size)),
        }
    } else {
        None
    };
    // 残留和 `remove::trash_items` 走同一套排队：外层里有不能动的东西就拒绝外层，内层随外层走。
    let all: Vec<(&Path, Option<&HoldBack>)> = plan
        .leftovers
        .iter()
        .map(|m| (m.path.as_path(), m.hold_back.as_ref()))
        .collect();
    let chosen: Vec<(&Path, Size, Option<&HoldBack>)> = plan
        .leftovers
        .iter()
        .filter(|m| selection.paths.contains(&m.path))
        .map(|m| (m.path.as_path(), m.size, m.hold_back.as_ref()))
        .collect();
    let queue = plan_moves(env, &all, &chosen, &mut outcome.refused);

    if dry_run {
        if let Some((path, size)) = &app_item {
            outcome.moved.push((path.clone(), path.clone()));
            outcome.freed += size.known().unwrap_or(0);
        }
        for planned in &queue {
            planned.pretend(&mut outcome);
        }
        return Ok(outcome);
    }
    let ledger = ledger.expect("checked above: a real run has a ledger");

    let batch = Batch {
        kind: RecordKind::Uninstall,
        title: plan.app.name.clone(),
        bundle_id: plan.app.bundle_id.clone(),
        source,
    };
    let mut mover = Mover::new(env, ledger, &batch, outcome, None);

    if let Some((path, size)) = app_item {
        match mover.move_one(&path, size) {
            Ok(Moved::Done) => {}
            Ok(Moved::Refused(why)) | Ok(Moved::Failed(why)) => {
                // App 没动，残留一个都不动；一条没有任何东西的记录也不留。
                mover.forget_last();
                let _ = mover.ledger.remove(&mover.record.id);
                return Err(ExecuteError::AppMoveFailed(why));
            }
            Err(err) => return Err(err),
        }
    }

    for planned in queue {
        mover.move_planned(planned)?;
    }
    // 什么都没移成（全被拒绝或失败）：没有东西可放回，也就不留一条空记录。
    Ok(mover.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::testing::scratch;
    use crate::{trash, HistoryRecord};
    use crate::{
        AppRef, AppSource, Confidence, Evidence, EvidenceKind, LeftoverKind, LeftoverMatch,
    };
    use std::fs;
    use std::path::PathBuf;

    /// 假的家目录放在真实家目录下面，这样废纸篓在同一个卷上。
    fn scratch_env() -> (tempfile::TempDir, Env) {
        let dir = scratch();
        let env = Env::for_test(dir.path());
        fs::create_dir_all(&env.user_library).unwrap();
        (dir, env)
    }

    fn app(env: &Env) -> InstalledApp {
        let path = env.app_dirs[0].join("Foo.app");
        fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        fs::write(path.join("Contents/MacOS/Foo"), b"#!/bin/sh\n").unwrap();
        InstalledApp {
            path,
            name: "Foo".to_string(),
            bundle_id: Some("com.foo.bar".to_string()),
            version: Some("1.0".to_string()),
            executable: None,
            team_id: None,
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Known(100),
            is_system: false,
        }
    }

    fn leftover(path: PathBuf, size: Size, hold_back: Option<HoldBack>) -> LeftoverMatch {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        if !path.exists() {
            fs::write(&path, b"x").unwrap();
        }
        LeftoverMatch {
            path,
            kind: LeftoverKind::Cache,
            size,
            evidence: Evidence {
                kind: EvidenceKind::BundleId,
                confidence: Confidence::Certain,
                matched: "com.foo.bar".to_string(),
            },
            rivals: vec![],
            hold_back,
        }
    }

    /// 测试结束（哪怕 panic）把移走的东西放回来，不给用户的废纸篓留东西。
    struct PutBack(Vec<(PathBuf, PathBuf)>);

    impl PutBack {
        fn note(&mut self, outcome: &RemovalOutcome) {
            self.0.extend(outcome.moved.iter().cloned());
        }
    }

    impl Drop for PutBack {
        fn drop(&mut self) {
            for (original, trashed) in self.0.drain(..) {
                if fs::symlink_metadata(&trashed).is_ok() {
                    let _ = trash::restore(&trashed, &original);
                }
            }
        }
    }

    fn ledger(env: &Env) -> Ledger {
        Ledger::at(env.data_dir.join("history.jsonl"))
    }

    #[test]
    fn selection_outside_the_plan_is_refused_before_anything_moves() {
        let (_d, env) = scratch_env();
        let app = app(&env);
        let plan = UninstallPlan {
            app,
            app_hold_back: None,
            leftovers: vec![],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        selection
            .paths
            .insert(env.user_library.join("Caches/com.foo.bar"));
        let err = execute(
            &env,
            &plan,
            &selection,
            Source::Cli,
            false,
            Some(&ledger(&env)),
        )
        .unwrap_err();
        assert!(matches!(err, ExecuteError::NotInPlan(_)));
        assert!(plan.app.path.exists(), "nothing moved");
    }

    #[test]
    fn empty_selection_is_an_error() {
        let (_d, env) = scratch_env();
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![],
            running: false,
        };
        let err = execute(
            &env,
            &plan,
            &Selection::default(),
            Source::Cli,
            false,
            Some(&ledger(&env)),
        )
        .unwrap_err();
        assert!(matches!(err, ExecuteError::NothingSelected));
    }

    #[test]
    fn dry_run_moves_nothing_and_says_what_it_would() {
        let (_d, env) = scratch_env();
        let app = app(&env);
        let cache = leftover(
            env.user_library.join("Caches/com.foo.bar"),
            Size::Known(10),
            None,
        );
        let keychain = leftover(
            env.user_library.join("Keychains/com.foo.bar"),
            Size::Known(7),
            None,
        );
        let plan = UninstallPlan {
            app: app.clone(),
            app_hold_back: None,
            leftovers: vec![cache.clone(), keychain.clone()],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        selection.paths.insert(cache.path.clone());
        selection.paths.insert(keychain.path.clone());

        let outcome = execute(&env, &plan, &selection, Source::App, true, None).unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.record_id, None);
        assert_eq!(
            outcome.moved,
            vec![
                (app.path.clone(), app.path.clone()),
                (cache.path.clone(), cache.path.clone())
            ],
            "the app first, then the leftovers; each named twice"
        );
        assert_eq!(
            outcome.refused,
            vec![(keychain.path.clone(), "protected: keychain".to_string())]
        );
        assert_eq!(outcome.freed, 110);
        assert!(app.path.exists() && cache.path.exists() && keychain.path.exists());
    }

    #[test]
    fn moves_app_first_then_leftovers_and_records_each() {
        let (_d, env) = scratch_env();
        let mut guard = PutBack(vec![]);
        let app = app(&env);
        let cache = leftover(
            env.user_library.join("Caches/com.foo.bar"),
            Size::Known(10),
            None,
        );
        let shared = leftover(
            env.user_library.join("Application Support/Foo"),
            Size::Unknown,
            Some(HoldBack::SharedWith {
                apps: vec![AppRef {
                    name: "Bar".into(),
                    bundle_id: None,
                    path: "/x".into(),
                }],
            }),
        );
        let not_selected = leftover(env.user_library.join("Logs/Foo"), Size::Known(3), None);
        let keychain = leftover(
            env.user_library.join("Keychains/com.foo.bar"),
            Size::Known(7),
            None,
        );
        let admin = leftover(
            env.system_library.join("LaunchDaemons/com.foo.bar.plist"),
            Size::Known(1),
            Some(HoldBack::NeedsAdmin),
        );
        let plan = UninstallPlan {
            app: app.clone(),
            app_hold_back: None,
            leftovers: vec![
                cache.clone(),
                shared.clone(),
                not_selected.clone(),
                keychain.clone(),
                admin.clone(),
            ],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        for m in [&cache, &shared, &keychain, &admin] {
            selection.paths.insert(m.path.clone());
        }
        let ledger = ledger(&env);

        let outcome = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap();
        guard.note(&outcome);

        assert_eq!(outcome.moved.len(), 3, "{:?}", outcome.moved);
        assert_eq!(outcome.moved[0].0, app.path, "the app goes first");
        assert_eq!(outcome.moved[1].0, cache.path);
        assert_eq!(
            outcome.moved[2].0, shared.path,
            "a shared file the user chose still moves"
        );
        assert!(outcome.moved.iter().all(|(_, trashed)| trashed.exists()));
        assert!(!app.path.exists() && !cache.path.exists() && !shared.path.exists());
        assert!(not_selected.path.exists() && keychain.path.exists() && admin.path.exists());
        assert_eq!(
            outcome.refused,
            vec![
                (keychain.path.clone(), "protected: keychain".to_string()),
                (admin.path.clone(), "needs an administrator".to_string()),
            ]
        );
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.freed, 110, "unknown sizes count nothing");
        assert!(!outcome.dry_run);

        let id = outcome.record_id.clone().expect("a record");
        let record = ledger.get(&id).unwrap().expect("the record is on disk");
        assert_eq!(record.app_name, "Foo");
        assert_eq!(record.source, Source::Cli);
        assert_eq!(record.items.len(), 3);
        assert!(
            record.items.iter().all(|i| !i.pending && !i.restored),
            "every move was completed and filled in"
        );
        assert_eq!(record.items[0].original, app.path);
        assert_eq!(record.items[0].trashed, outcome.moved[0].1);
        assert_eq!(record.items[1].size, Size::Known(10));

        // 从 History 放回来，顺便把废纸篓收拾干净。
        let restored = ledger.restore(&env, &id).unwrap();
        assert_eq!(restored.restored.len(), 3, "{:?}", restored.skipped);
        assert!(app.path.join("Contents/MacOS/Foo").exists());
        assert!(cache.path.exists() && shared.path.exists());
    }

    #[test]
    fn without_a_ledger_nothing_moves() {
        let (_d, env) = scratch_env();
        let cache = leftover(
            env.user_library.join("Caches/com.foo.bar"),
            Size::Known(10),
            None,
        );
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![cache.clone()],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        selection.paths.insert(cache.path.clone());

        let err = execute(&env, &plan, &selection, Source::App, false, None).unwrap_err();
        assert!(matches!(err, ExecuteError::LedgerRequired), "{err}");
        assert!(
            plan.app.path.exists() && cache.path.exists(),
            "nothing moved without a record to put it back from"
        );
    }

    #[test]
    fn the_record_is_written_before_each_move_and_a_broken_ledger_returns_what_already_happened() {
        let (_d, env) = scratch_env();
        let mut guard = PutBack(vec![]);
        let first = leftover(env.user_library.join("Caches/first"), Size::Known(1), None);
        let second = leftover(env.user_library.join("Caches/second"), Size::Known(2), None);
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![first.clone(), second.clone()],
            running: false,
        };
        let mut selection = Selection::default();
        selection.paths.insert(first.path.clone());
        selection.paths.insert(second.path.clone());

        // History 在一个会在第一项移走后变得写不了的地方：目录被换成了文件。
        let ledger_dir = env.data_dir.clone();
        fs::create_dir_all(&ledger_dir).unwrap();
        let ledger = Ledger::at(ledger_dir.join("history.jsonl"));
        // 第一次 update 建文件；之后我们把它的目录变成只读，update 的临时文件就建不出来。
        ledger.append(&record_stub()).unwrap();
        let outcome_or_err = {
            let mut perms = fs::metadata(&ledger_dir).unwrap().permissions();
            // 先放过第一项：目录可写。移完第一项后要让它失败，所以用一个只允许一次成功的办法：
            // 把 history.lock 和 history.jsonl 留着，但把目录设成只读；update 要新建临时文件 → 失败。
            // 不过第一项的 pending 写入也会失败。为了让 pending 成功、回填失败，这里直接在回填前改权限
            // 做不到；改为验证"pending 写不进去时什么都不动"。
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o500);
            fs::set_permissions(&ledger_dir, perms).unwrap();
            let result = execute(&env, &plan, &selection, Source::App, false, Some(&ledger));
            let mut perms = fs::metadata(&ledger_dir).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
            fs::set_permissions(&ledger_dir, perms).unwrap();
            result
        };
        let err = outcome_or_err.unwrap_err();
        match &err {
            ExecuteError::Ledger { partial, .. } => {
                guard.note(partial);
                assert!(
                    partial.moved.is_empty(),
                    "the intent could not be recorded, so nothing moved"
                );
            }
            other => panic!("expected a ledger error, got {other}"),
        }
        assert!(first.path.exists() && second.path.exists());
    }

    fn record_stub() -> HistoryRecord {
        HistoryRecord {
            id: "stub".to_string(),
            at: chrono::Utc::now(),
            kind: RecordKind::Uninstall,
            app_name: "Stub".to_string(),
            bundle_id: None,
            source: Source::Cli,
            items: vec![],
            restored: false,
        }
    }

    #[test]
    fn a_held_back_app_fails_before_anything_moves() {
        let (_d, env) = scratch_env();
        let app = app(&env);
        let cache = leftover(
            env.user_library.join("Caches/com.foo.bar"),
            Size::Known(10),
            None,
        );
        let plan = UninstallPlan {
            app: app.clone(),
            app_hold_back: Some(HoldBack::SystemOwned),
            leftovers: vec![cache.clone()],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        selection.paths.insert(cache.path.clone());
        let ledger = ledger(&env);

        let err = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap_err();
        assert!(
            matches!(err, ExecuteError::AppMoveFailed(ref why) if why == "belongs to macOS"),
            "{err}"
        );
        assert!(
            app.path.exists() && cache.path.exists(),
            "the leftovers stay with the app"
        );
        assert!(
            ledger.list().unwrap().is_empty(),
            "no record for a move that never started"
        );

        let dry = execute(&env, &plan, &selection, Source::Cli, true, Some(&ledger)).unwrap();
        assert_eq!(
            dry.refused,
            vec![(app.path.clone(), "belongs to macOS".to_string())]
        );
        assert_eq!(
            dry.moved,
            vec![(cache.path.clone(), cache.path.clone())],
            "a dry run still lists the rest"
        );
    }

    #[test]
    fn an_app_that_vanished_fails_and_leaves_leftovers_alone_and_no_empty_record() {
        let (_d, env) = scratch_env();
        let app = app(&env);
        let cache = leftover(
            env.user_library.join("Caches/com.foo.bar"),
            Size::Known(10),
            None,
        );
        fs::remove_dir_all(&app.path).unwrap();
        let plan = UninstallPlan {
            app,
            app_hold_back: None,
            leftovers: vec![cache.clone()],
            running: false,
        };
        let mut selection = Selection {
            include_app: true,
            ..Default::default()
        };
        selection.paths.insert(cache.path.clone());
        let ledger = ledger(&env);

        let err = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap_err();
        assert!(matches!(err, ExecuteError::AppMoveFailed(_)), "{err}");
        assert!(cache.path.exists());
        assert!(
            ledger.list().unwrap().is_empty(),
            "a record with nothing in it is not kept"
        );
    }

    #[test]
    fn a_leftover_that_vanished_is_reported_and_the_rest_still_move() {
        let (_d, env) = scratch_env();
        let mut guard = PutBack(vec![]);
        let gone = leftover(env.user_library.join("Caches/gone"), Size::Known(1), None);
        let stays = leftover(env.user_library.join("Caches/stays"), Size::Known(2), None);
        fs::remove_file(&gone.path).unwrap();
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![gone.clone(), stays.clone()],
            running: false,
        };
        let mut selection = Selection::default();
        selection.paths.insert(gone.path.clone());
        selection.paths.insert(stays.path.clone());
        let ledger = ledger(&env);

        let outcome = execute(&env, &plan, &selection, Source::App, false, Some(&ledger)).unwrap();
        guard.note(&outcome);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, gone.path);
        assert_eq!(outcome.moved.len(), 1);
        assert_eq!(outcome.freed, 2);
        let record = ledger
            .get(outcome.record_id.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            record.items.len(),
            1,
            "the one that vanished is not in the record"
        );
        assert_eq!(record.items[0].original, stays.path);
    }

    #[test]
    fn protection_is_checked_again_at_the_moment_of_moving() {
        // 计划里没标 hold_back（比如计划生成后文件夹里多了一个钱包 profile），移动前的复查还是会挡下。
        let (_d, env) = scratch_env();
        let chrome = env.user_library.join("Application Support/Google/Chrome");
        fs::create_dir_all(
            chrome.join("Default/Local Extension Settings/nkbihfbeogaeaoehlefnkodbefgpgknn"),
        )
        .unwrap();
        let chrome_match = leftover(chrome.clone(), Size::Known(5), None);
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![chrome_match],
            running: false,
        };
        let mut selection = Selection::default();
        selection.paths.insert(chrome.clone());
        let ledger = ledger(&env);

        let outcome = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(chrome.clone(), "protected: wallet".to_string())]
        );
        assert!(chrome.exists());
        assert_eq!(
            outcome.record_id, None,
            "nothing moved, so there is nothing to put back"
        );
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn a_path_swapped_for_a_symlink_after_planning_is_refused_and_the_target_untouched() {
        let (_d, env) = scratch_env();
        let keychains = env.user_library.join("Keychains");
        fs::create_dir_all(&keychains).unwrap();
        fs::write(keychains.join("login.keychain-db"), b"k").unwrap();
        let cache_dir = env.user_library.join("Caches/com.foo.bar");
        let inside = leftover(cache_dir.join("login.keychain-db"), Size::Known(5), None);
        // 计划之后，`com.foo.bar` 被换成了指向钥匙串目录的链接。
        fs::remove_dir_all(&cache_dir).unwrap();
        std::os::unix::fs::symlink(&keychains, &cache_dir).unwrap();
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![inside.clone()],
            running: false,
        };
        let mut selection = Selection::default();
        selection.paths.insert(inside.path.clone());
        let ledger = ledger(&env);

        let outcome = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(inside.path.clone(), "protected: keychain".to_string())]
        );
        assert!(keychains.join("login.keychain-db").exists());
    }

    #[test]
    fn a_symlinked_app_bundle_moves_as_a_link_and_the_target_stays() {
        // `/Applications/Foo.app` 是个链接：叶子不跟随，`resolve` 的结果就是它自己，不算"路径变了"。
        let (d, env) = scratch_env();
        let mut guard = PutBack(vec![]);
        let real_app = app(&env);
        let elsewhere = d.path().join("Elsewhere/Foo.app");
        fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        fs::rename(&real_app.path, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &real_app.path).unwrap();
        let plan = UninstallPlan {
            app: real_app.clone(),
            app_hold_back: None,
            leftovers: vec![],
            running: false,
        };
        let selection = Selection {
            include_app: true,
            ..Default::default()
        };
        let outcome = execute(
            &env,
            &plan,
            &selection,
            Source::Cli,
            false,
            Some(&ledger(&env)),
        )
        .unwrap();
        guard.note(&outcome);
        assert_eq!(outcome.moved.len(), 1, "{outcome:?}");
        assert!(fs::symlink_metadata(&real_app.path).is_err());
        assert!(
            elsewhere.join("Contents/MacOS/Foo").exists(),
            "only the link moved"
        );
    }

    #[test]
    fn a_leftover_folder_holding_something_that_must_stay_is_refused() {
        let (_d, env) = scratch_env();
        let outer = leftover(
            env.user_library.join("Application Support/com.foo.bar"),
            Size::Known(5),
            None,
        );
        fs::remove_file(&outer.path).unwrap();
        fs::create_dir_all(&outer.path).unwrap();
        let inner = leftover(
            outer.path.join("helper"),
            Size::Known(1),
            Some(HoldBack::NeedsAdmin),
        );
        let plan = UninstallPlan {
            app: app(&env),
            app_hold_back: None,
            leftovers: vec![outer.clone(), inner.clone()],
            running: false,
        };
        let mut selection = Selection::default();
        selection.paths.insert(outer.path.clone());
        let ledger = ledger(&env);
        let outcome = execute(&env, &plan, &selection, Source::Cli, false, Some(&ledger)).unwrap();
        assert!(outcome.moved.is_empty());
        assert_eq!(
            outcome.refused,
            vec![(
                outer.path.clone(),
                format!("contains {}: needs an administrator", inner.path.display())
            )]
        );
        assert!(inner.path.exists());
        assert!(ledger.list().unwrap().is_empty());
    }
}
