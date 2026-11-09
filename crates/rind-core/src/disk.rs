//! 磁盘整体占用，给总览页的容量条用。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 一个卷的容量。`available` 是普通用户还能写多少（APFS 下含可清除空间之外的部分）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    pub total: u64,
    pub available: u64,
}

impl DiskUsage {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// `path` 所在卷的容量；读不到就是 None。
pub fn usage(path: &Path) -> Option<DiskUsage> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` 是以 NUL 结尾的路径，`stat` 是够大的输出缓冲。
    if unsafe { libc::statfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_bsize as u64;
    Some(DiskUsage {
        total: stat.f_blocks.saturating_mul(block),
        available: stat.f_bavail.saturating_mul(block),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_volume_has_a_size() {
        let u = usage(Path::new("/")).expect("statfs /");
        assert!(u.total > 0);
        assert!(u.available <= u.total);
        assert_eq!(u.used(), u.total - u.available);
    }

    #[test]
    fn missing_path_is_none() {
        assert!(usage(Path::new("/definitely/not/here")).is_none());
    }
}
