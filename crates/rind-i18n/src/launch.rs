//! 启动项的文案。

use rind_core::launch::{LaunchDomain, LaunchStatus};

use crate::{pick, Lang};

pub fn describe_launch_domain(lang: Lang, domain: LaunchDomain) -> &'static str {
    let (zh, en) = match domain {
        LaunchDomain::UserAgent => ("当前用户", "This user"),
        LaunchDomain::GlobalAgent => ("所有用户", "All users"),
        LaunchDomain::GlobalDaemon => ("系统后台", "System daemon"),
    };
    pick(lang, zh, en)
}

pub fn describe_launch_status(lang: Lang, status: LaunchStatus) -> String {
    match (status, lang) {
        (LaunchStatus::Loaded { pid: Some(pid) }, Lang::ZhCn) => format!("运行中（PID {pid}）"),
        (LaunchStatus::Loaded { pid: Some(pid) }, Lang::En) => format!("Running (PID {pid})"),
        (LaunchStatus::Loaded { pid: None }, _) => pick(lang, "已加载", "Loaded").to_string(),
        (LaunchStatus::NotLoaded, _) => pick(lang, "未加载", "Not loaded").to_string(),
        (LaunchStatus::Disabled, _) => pick(lang, "已停用", "Disabled").to_string(),
        (LaunchStatus::Unknown, _) => pick(lang, "状态未知", "Unknown").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_in_both_languages() {
        for d in [
            LaunchDomain::UserAgent,
            LaunchDomain::GlobalAgent,
            LaunchDomain::GlobalDaemon,
        ] {
            assert_ne!(
                describe_launch_domain(Lang::ZhCn, d),
                describe_launch_domain(Lang::En, d)
            );
        }
        for s in [
            LaunchStatus::Loaded { pid: Some(1) },
            LaunchStatus::Loaded { pid: None },
            LaunchStatus::NotLoaded,
            LaunchStatus::Disabled,
            LaunchStatus::Unknown,
        ] {
            assert_ne!(
                describe_launch_status(Lang::ZhCn, s),
                describe_launch_status(Lang::En, s)
            );
        }
    }
}
