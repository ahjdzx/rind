//! 扫描结果的共享仓库。总览的「智能扫描」和各页自己的扫描都写这里：换页不丢结果、不重复扫；
//! 任何一页把东西移到废纸篓后，统一在这里把它们从所有结果里剪掉。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::*;
use rind_core::cleanup::CleanupReport;
use rind_core::disk::DiskUsage;
use rind_core::launch::{LaunchItem, LaunchStatus};
use rind_core::orphans::OrphanReport;
use rind_core::space::SpaceReport;
use rind_core::ScanProgress;

use crate::model;
use crate::services::{AppEntry, CleanupPrefs, Services};

/// 正在跑的一次扫描。
#[derive(Clone, Debug)]
pub struct Running {
    pub progress: Option<ScanProgress>,
    /// 是不是总览的「智能扫描」发起的。
    pub smart: bool,
}

/// 一类结果：最近一次完整的数据，加上正在跑的那次。
pub struct Slot<T> {
    pub data: Option<Arc<T>>,
    pub running: Option<Running>,
    /// 每有一份新的扫描结果加一（剪枝不算），页面据此重置选择。
    pub epoch: u64,
    generation: u64,
    cancel: Option<Arc<AtomicBool>>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            data: None,
            running: None,
            epoch: 0,
            generation: 0,
            cancel: None,
        }
    }
}

impl<T> Slot<T> {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn is_smart(&self) -> bool {
        self.running.as_ref().is_some_and(|r| r.smart)
    }

    pub fn progress(&self) -> Option<&ScanProgress> {
        self.running.as_ref().and_then(|r| r.progress.as_ref())
    }

    /// 让正在跑的那次停下，它晚到的结果会被丢掉；已有的数据保留。
    fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        self.running = None;
    }
}

/// 哪一类结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Orphans,
    Cleanup,
    Space,
}

pub struct Scans {
    services: Arc<Services>,
    pub apps: Slot<Vec<AppEntry>>,
    pub orphans: Slot<OrphanReport>,
    pub cleanup: Slot<CleanupReport>,
    pub launch: Slot<Vec<LaunchItem>>,
    pub space: Slot<SpaceReport>,
    /// 空间分析正在看（或最近看过）的目录。
    pub space_root: Option<PathBuf>,
    pub disk: Option<DiskUsage>,
}

fn apps_slot(s: &mut Scans) -> &mut Slot<Vec<AppEntry>> {
    &mut s.apps
}
fn orphans_slot(s: &mut Scans) -> &mut Slot<OrphanReport> {
    &mut s.orphans
}
fn cleanup_slot(s: &mut Scans) -> &mut Slot<CleanupReport> {
    &mut s.cleanup
}
fn launch_slot(s: &mut Scans) -> &mut Slot<Vec<LaunchItem>> {
    &mut s.launch
}
fn space_slot(s: &mut Scans) -> &mut Slot<SpaceReport> {
    &mut s.space
}

impl Scans {
    pub fn new(services: Arc<Services>, cx: &mut Context<Self>) -> Self {
        let mut scans = Self {
            services,
            apps: Slot::default(),
            orphans: Slot::default(),
            cleanup: Slot::default(),
            launch: Slot::default(),
            space: Slot::default(),
            space_root: None,
            disk: None,
        };
        scans.load_apps(cx);
        scans.refresh_disk(cx);
        scans
    }

    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }

    /// 在后台跑 `work`，进度和结果写进 `slot` 选中的那一格。同一格再跑一次会先停掉上一次。
    fn run<T, F>(
        &mut self,
        slot: fn(&mut Scans) -> &mut Slot<T>,
        smart: bool,
        work: F,
        cx: &mut Context<Self>,
    ) where
        T: Send + Sync + 'static,
        F: FnOnce(&Services, Arc<AtomicBool>, &(dyn Fn(ScanProgress) + Sync)) -> T + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let s = slot(self);
        s.stop();
        let generation = s.generation;
        s.cancel = Some(cancel.clone());
        s.running = Some(Running {
            progress: None,
            smart,
        });
        cx.notify();
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<ScanProgress>();
            let task = cx.background_spawn(async move {
                let report = move |progress: ScanProgress| {
                    let _ = tx.unbounded_send(progress);
                };
                work(&services, cancel, &report)
            });
            while let Some(mut progress) = rx.next().await {
                // 进度可能很密（空间分析每个文件一次）：只要最新的那条，并且最多 20 次/秒。
                while let Ok(next) = rx.try_recv() {
                    progress = next;
                }
                let alive = this.update(cx, |this, cx| {
                    let s = slot(this);
                    if s.generation == generation {
                        if let Some(running) = s.running.as_mut() {
                            running.progress = Some(progress);
                            cx.notify();
                        }
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
            let result = task.await;
            this.update(cx, |this, cx| {
                let s = slot(this);
                if s.generation != generation {
                    return;
                }
                s.running = None;
                let cancelled = s.cancel.take().is_some_and(|c| c.load(Ordering::Relaxed));
                if !cancelled {
                    s.data = Some(Arc::new(result));
                    s.epoch += 1;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn load_apps(&mut self, cx: &mut Context<Self>) {
        self.run(apps_slot, false, |s, _, _| s.apps(), cx);
    }

    pub fn scan_orphans(&mut self, smart: bool, cx: &mut Context<Self>) {
        self.run(orphans_slot, smart, |s, cancel, p| s.orphans(cancel, p), cx);
    }

    pub fn scan_cleanup(&mut self, prefs: CleanupPrefs, smart: bool, cx: &mut Context<Self>) {
        self.run(
            cleanup_slot,
            smart,
            move |s, cancel, p| s.cleanup(&prefs, cancel, p),
            cx,
        );
    }

    pub fn load_launch(&mut self, smart: bool, cx: &mut Context<Self>) {
        self.run(launch_slot, smart, |s, _, _| s.launch_items(), cx);
    }

    pub fn analyze_space(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.space_root = Some(root.clone());
        self.run(
            space_slot,
            false,
            move |s, cancel, p| s.space(&root, cancel, p),
            cx,
        );
    }

    /// 智能扫描：孤儿、缓存、启动项并行跑；App 列表没有就顺便读；磁盘容量刷新一次。
    pub fn smart_scan(&mut self, prefs: CleanupPrefs, cx: &mut Context<Self>) {
        if self.smart_running() {
            return;
        }
        self.scan_orphans(true, cx);
        self.scan_cleanup(prefs, true, cx);
        self.load_launch(true, cx);
        if self.apps.data.is_none() && !self.apps.is_running() {
            self.load_apps(cx);
        }
        self.refresh_disk(cx);
    }

    pub fn smart_running(&self) -> bool {
        self.orphans.is_smart() || self.cleanup.is_smart() || self.launch.is_smart()
    }

    /// 取消智能扫描：没跑完的那几项丢弃，已有的旧结果保留。
    pub fn cancel_smart(&mut self, cx: &mut Context<Self>) {
        if self.orphans.is_smart() {
            self.orphans.stop();
        }
        if self.cleanup.is_smart() {
            self.cleanup.stop();
        }
        if self.launch.is_smart() {
            self.launch.stop();
        }
        cx.notify();
    }

    /// 取消某一页自己发起（或智能扫描里）的那一项。
    pub fn cancel(&mut self, kind: Kind, cx: &mut Context<Self>) {
        match kind {
            Kind::Orphans => self.orphans.stop(),
            Kind::Cleanup => self.cleanup.stop(),
            Kind::Space => self.space.stop(),
        }
        cx.notify();
    }

    pub fn refresh_disk(&mut self, cx: &mut Context<Self>) {
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let disk = cx
                .background_spawn(async move { services.disk_usage() })
                .await;
            this.update(cx, |this, cx| {
                this.disk = disk;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 这些路径已经进了废纸篓：从所有结果里剪掉，再刷新磁盘容量。
    pub fn forget(&mut self, moved: &HashSet<PathBuf>, cx: &mut Context<Self>) {
        if moved.is_empty() {
            return;
        }
        if let Some(data) = &self.orphans.data {
            self.orphans.data = Some(Arc::new(model::prune_orphans(data, moved)));
        }
        if let Some(data) = &self.cleanup.data {
            self.cleanup.data = Some(Arc::new(model::prune_cleanup(data, moved)));
        }
        if let Some(data) = &self.launch.data {
            self.launch.data = Some(Arc::new(model::prune_launch(data, moved)));
        }
        if let Some(data) = &self.space.data {
            self.space.data = Some(Arc::new(model::prune_space(data, moved)));
        }
        if let Some(data) = &self.apps.data {
            let kept: Vec<AppEntry> = data
                .iter()
                .filter(|a| !model::is_gone(&a.app.path, moved))
                .cloned()
                .collect();
            self.apps.data = Some(Arc::new(kept));
        }
        self.refresh_disk(cx);
        cx.notify();
    }

    /// 停用/启用成功后先在本地改状态，界面立刻有反馈。
    pub fn set_launch_status(
        &mut self,
        plist: &PathBuf,
        status: LaunchStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(data) = &self.launch.data {
            let mut items: Vec<LaunchItem> = data.as_ref().clone();
            if let Some(item) = items.iter_mut().find(|i| &i.plist == plist) {
                item.status = status;
            }
            self.launch.data = Some(Arc::new(items));
            cx.notify();
        }
    }
}

/// 当前的缓存清理偏好；全局没设时用默认值。
pub fn cleanup_prefs(cx: &App) -> CleanupPrefs {
    cx.try_global::<CleanupPrefs>().cloned().unwrap_or_default()
}
