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
        let script = match &app.bundle_id {
            Some(id) => format!("tell application id \"{}\" to quit", id.replace('"', "")),
            None => format!(
                "tell application \"{}\" to quit",
                app.path.display().to_string().replace('"', "")
            ),
        };
        let output = crate::apps::run_with_timeout(
            "/usr/bin/osascript",
            &["-e", &script],
            Duration::from_secs(10),
        );
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
