//! 所有路径都从这里来，测试时注入一个临时目录就能跑完整流程。

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    /// 家目录。
    pub home: PathBuf,
    /// `~/Library`。
    pub user_library: PathBuf,
    /// `/Library`。
    pub system_library: PathBuf,
    /// 扫描 App 的目录：`/Applications`、`~/Applications`。
    pub app_dirs: Vec<PathBuf>,
    /// `/System/Applications` 等 Apple 自带 App 的位置，只用来识别，不列为可卸载。
    pub system_app_dirs: Vec<PathBuf>,
    /// Rind 自己的数据：`~/Library/Application Support/Rind`。
    pub data_dir: PathBuf,
    /// `/private/var/db/receipts`。
    pub receipts_dir: PathBuf,
    /// Homebrew 的前缀（`/opt/homebrew` 或 `/usr/local`），没装就是 None。
    pub homebrew_prefix: Option<PathBuf>,
}

impl Env {
    /// 真实机器。`RIND_HOME` / `RIND_ROOT` 能把家目录和系统根指到别处，给 CLI 的集成测试用。
    pub fn detect() -> Env {
        if let (Ok(home), Ok(root)) = (std::env::var("RIND_HOME"), std::env::var("RIND_ROOT")) {
            return Env::rooted(Path::new(&home), Path::new(&root), None);
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let homebrew_prefix = ["/opt/homebrew", "/usr/local/Homebrew"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.join("bin/brew").exists())
            .map(|p| {
                if p.ends_with("Homebrew") {
                    PathBuf::from("/usr/local")
                } else {
                    p
                }
            });
        Env::rooted(&home, Path::new("/"), homebrew_prefix)
    }

    /// 测试用：家目录和系统根都在 `root` 下面。
    pub fn for_test(root: &Path) -> Env {
        let home = root.join("Users/tester");
        Env::rooted(&home, root, None)
    }

    fn rooted(home: &Path, system_root: &Path, homebrew_prefix: Option<PathBuf>) -> Env {
        let user_library = home.join("Library");
        Env {
            home: home.to_path_buf(),
            user_library: user_library.clone(),
            system_library: system_root.join("Library"),
            app_dirs: vec![system_root.join("Applications"), home.join("Applications")],
            system_app_dirs: vec![
                system_root.join("System/Applications"),
                system_root.join("System/Applications/Utilities"),
                system_root.join("System/Library/CoreServices"),
            ],
            data_dir: user_library.join("Application Support/Rind"),
            receipts_dir: system_root.join("private/var/db/receipts"),
            homebrew_prefix,
        }
    }

    /// 把 `~/...` 展开成绝对路径。
    pub fn expand(&self, path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else if path == "~" {
            self.home.clone()
        } else {
            PathBuf::from(path)
        }
    }

    /// 把绝对路径缩成 `~/...`，给人看。
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_and_displays_home() {
        let env = Env::for_test(Path::new("/tmp/root"));
        assert_eq!(
            env.expand("~/Library/Caches"),
            PathBuf::from("/tmp/root/Users/tester/Library/Caches")
        );
        assert_eq!(
            env.display(Path::new("/tmp/root/Users/tester/Library")),
            "~/Library"
        );
        assert_eq!(env.display(Path::new("/Library")), "/Library");
    }
}
