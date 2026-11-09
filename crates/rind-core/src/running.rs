//! App 在不在运行、让它退出。

use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, Signal, System, UpdateKind};

use crate::InstalledApp;

/// 有没有进程的可执行文件在 App 包内。
pub fn is_running(app: &InstalledApp) -> bool {
    !running_pids(app).is_empty()
}

/// 可执行文件在 App 包内的每个进程，包括它的 helper。
/// 两边都按盘上的真实路径比：`/Applications/Foo.app` 是个符号链接时，进程报的是链接指向的地方。
pub fn running_pids(app: &InstalledApp) -> Vec<u32> {
    let bundle = real_path(&app.path);
    let mut system = System::new_with_specifics(
        RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::nothing().with_exe(UpdateKind::Always)),
    );
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut pids: Vec<u32> = system
        .processes()
        .iter()
        .filter(|(_, process)| {
            process
                .exe()
                .is_some_and(|exe| real_path(exe).starts_with(&bundle))
        })
        .map(|(pid, _)| pid.as_u32())
        .collect();
    pids.sort_unstable();
    pids
}

/// 解析符号链接后的路径；解析不了（比如程序已经被删了）就原样。
fn real_path(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 正常退出（`osascript` 让 App 自己退），`force` 则给每个进程 SIGKILL。
/// 等最多 5 秒；还在运行就报错。
pub fn quit(app: &InstalledApp, force: bool) -> anyhow::Result<()> {
    if !is_running(app) {
        return Ok(());
    }
    if force {
        let bundle = real_path(&app.path);
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_processes(ProcessRefreshKind::nothing().with_exe(UpdateKind::Always)),
        );
        system.refresh_processes(ProcessesToUpdate::All, true);
        for process in system.processes().values() {
            if process
                .exe()
                .is_some_and(|exe| real_path(exe).starts_with(&bundle))
            {
                process.kill_with(Signal::Kill);
            }
        }
    } else {
        let args = quit_script_args(app);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output =
            crate::apps::run_with_timeout("/usr/bin/osascript", &args, Duration::from_secs(10));
        match output {
            Some(output) if output.status.success() => {}
            Some(output) => {
                anyhow::bail!(
                    "osascript: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            None => anyhow::bail!("osascript did not answer in time"),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_running(app) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("{} is still running", app.name)
}

/// `osascript` 的参数。bundle id 和路径来自 App 包里的文件，谁都能写，所以从不拼进脚本文本：
/// 脚本是固定的，目标经 `on run argv` 当数据传进去。bundle id 还要像个 bundle id（`[A-Za-z0-9.-]`），
/// 不像就按路径找这个 App。
fn quit_script_args(app: &InstalledApp) -> Vec<String> {
    let plain_id = app.bundle_id.as_deref().filter(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    let (tell, target) = match plain_id {
        Some(id) => (
            "tell application id (item 1 of argv) to quit",
            id.to_string(),
        ),
        None => (
            "tell application (item 1 of argv) to quit",
            app.path.to_string_lossy().to_string(),
        ),
    };
    vec![
        "-e".into(),
        "on run argv".into(),
        "-e".into(),
        tell.into(),
        "-e".into(),
        "end run".into(),
        target,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppSource, Size};

    #[test]
    fn an_app_that_is_not_installed_is_not_running() {
        let app = InstalledApp {
            path: "/Applications/Definitely Not Here.app".into(),
            name: "Nope".into(),
            bundle_id: None,
            version: None,
            executable: None,
            team_id: None,
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        };
        assert!(!is_running(&app));
        assert!(quit(&app, false).is_ok(), "nothing to quit is fine");
    }

    #[test]
    fn the_quit_script_never_contains_the_target() {
        let mut app = InstalledApp {
            path: "/Applications/Evil\" to quit\ndo shell script \"x.app".into(),
            name: "Evil".into(),
            bundle_id: Some("com.evil\" \u{ac} do shell script \"touch /tmp/pwned".into()),
            version: None,
            executable: None,
            team_id: None,
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        };
        let args = quit_script_args(&app);
        assert_eq!(
            args[3], "tell application (item 1 of argv) to quit",
            "an odd bundle id falls back to the path"
        );
        assert_eq!(args.last().unwrap(), &app.path.to_string_lossy());
        assert!(args[..args.len() - 1]
            .iter()
            .all(|a| !a.contains("shell") && !a.contains("Evil")));

        app.bundle_id = Some("com.foo.Bar-2".into());
        let args = quit_script_args(&app);
        assert_eq!(args[3], "tell application id (item 1 of argv) to quit");
        assert_eq!(args.last().unwrap(), "com.foo.Bar-2");
    }

    #[test]
    fn finds_a_process_by_its_executable_folder() {
        // 把 `sleep` 当作 App 的可执行文件：进程的 exe 在这个“包”里。
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Sleeper.app");
        let bin = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("sleeper");
        std::fs::copy("/bin/sleep", &exe).unwrap();
        let mut child = std::process::Command::new(&exe).arg("30").spawn().unwrap();
        let app = InstalledApp {
            path: bundle.clone(),
            name: "Sleeper".into(),
            bundle_id: None,
            version: None,
            executable: Some(exe),
            team_id: None,
            embedded_ids: vec![],
            app_groups: vec![],
            source: AppSource::Other,
            size: Size::Unknown,
            is_system: false,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_running(&app) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(running_pids(&app).contains(&child.id()));

        // 通过符号链接找到的 App 也认得出来。
        let alias = dir.path().join("Alias.app");
        std::os::unix::fs::symlink(&bundle, &alias).unwrap();
        let via_alias = InstalledApp {
            path: alias,
            ..app.clone()
        };
        assert!(
            is_running(&via_alias),
            "a symlinked bundle still counts as running"
        );

        quit(&app, true).expect("force quit kills it");
        assert!(!is_running(&app));
        let _ = child.wait();
    }
}
