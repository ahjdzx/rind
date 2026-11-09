//! 空间分析：选一个目录，看什么最占地方。「目录」页签可以下钻（超过已分析深度时再分析那个子目录，
//! 结果缓存），「大文件」页签列出最大的文件。可以勾选后移到废纸篓，受保护的不能选。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::remove::{Batch, Removable};
use rind_core::space::{NodeKind, SpaceNode, SpaceReport};
use rind_core::{HoldBack, RecordKind, RemovalOutcome, ScanProgress, Size, Source};

use crate::model;
use crate::services::{Services, TrashOp};
use crate::store::{Kind, Scans};
use crate::strings::{self, ui, Key, Lang, LangSetting};
use crate::ui::{self, Tone, TrashHost, TrashPlan};

/// 正在对某个子目录再分析一次。
struct Drill {
    path: PathBuf,
    generation: u64,
    cancel: Arc<AtomicBool>,
    progress: Option<ScanProgress>,
}

fn selectable(hold_back: Option<&HoldBack>) -> bool {
    hold_back.is_none_or(HoldBack::selectable)
}

pub struct SpacePage {
    services: Arc<Services>,
    scans: Entity<Scans>,
    current: Option<PathBuf>,
    seen_epoch: u64,
    /// 下钻时额外分析过的子目录。
    subtrees: Vec<Arc<SpaceReport>>,
    drill: Option<Drill>,
    drill_generation: u64,
    tab: usize,
    selection: BTreeSet<PathBuf>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl SpacePage {
    pub fn new(services: Arc<Services>, scans: Entity<Scans>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&scans, |this, _, cx| this.sync(cx)),
            cx.observe_global::<LangSetting>(|_, cx| cx.notify()),
        ];
        let mut page = Self {
            services,
            scans,
            current: None,
            seen_epoch: 0,
            subtrees: Vec::new(),
            drill: None,
            drill_generation: 0,
            tab: 0,
            selection: BTreeSet::new(),
            busy: false,
            _subscriptions: subscriptions,
        };
        page.sync(cx);
        page
    }

    fn report(&self, cx: &App) -> Option<Arc<SpaceReport>> {
        self.scans.read(cx).space.data.clone()
    }

    /// 新的一次分析：回到根，清掉下钻缓存和选择。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (epoch, report) = {
            let slot = &self.scans.read(cx).space;
            (slot.epoch, slot.data.clone())
        };
        if let Some(report) = report {
            if epoch != self.seen_epoch {
                self.seen_epoch = epoch;
                self.current = Some(report.root.path.clone());
                self.subtrees.clear();
                self.selection.clear();
                self.stop_drill();
            }
        }
        cx.notify();
    }

    fn analyze(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.stop_drill();
        self.scans.update(cx, |s, cx| s.analyze_space(root, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.scans.update(cx, |s, cx| s.cancel(Kind::Space, cx));
    }

    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let lang = strings::lang(cx);
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(ui(lang, Key::SpaceChoose).into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.analyze(path, cx)).ok();
                }
            }
        })
        .detach();
    }

    fn stop_drill(&mut self) {
        if let Some(drill) = self.drill.take() {
            drill.cancel.store(true, Ordering::Relaxed);
        }
        self.drill_generation += 1;
    }

    fn reports(&self, main: &Arc<SpaceReport>) -> Vec<Arc<SpaceReport>> {
        let mut all = vec![main.clone()];
        all.extend(self.subtrees.iter().cloned());
        all
    }

    /// 进一个目录；它超过了已分析的深度就再分析它一次。
    fn open_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(main) = self.report(cx) else { return };
        let reports = self.reports(&main);
        let node = model::locate(reports.iter().map(|r| r.as_ref()), &path);
        let analyzed = self.subtrees.iter().any(|r| r.root.path == path);
        let drill = node.is_some_and(model::needs_drill) && !analyzed;
        self.current = Some(path.clone());
        self.stop_drill();
        if drill {
            self.start_drill(path, cx);
        }
        cx.notify();
    }

    fn start_drill(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let generation = self.drill_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.drill = Some(Drill {
            path: path.clone(),
            generation,
            cancel: cancel.clone(),
            progress: None,
        });
        let services = self.services.clone();
        cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<ScanProgress>();
            let task = cx.background_spawn(async move {
                let report = move |p: ScanProgress| {
                    let _ = tx.unbounded_send(p);
                };
                services.space(&path, cancel, &report)
            });
            while let Some(mut progress) = rx.next().await {
                while let Ok(next) = rx.try_recv() {
                    progress = next;
                }
                let alive = this.update(cx, |this, cx| {
                    if let Some(drill) = this.drill.as_mut().filter(|d| d.generation == generation)
                    {
                        drill.progress = Some(progress);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
            let report = task.await;
            this.update(cx, |this, cx| {
                let Some(drill) = this.drill.take_if(|d| d.generation == generation) else {
                    return;
                };
                if !drill.cancel.load(Ordering::Relaxed) {
                    this.subtrees.push(Arc::new(report));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 所有已知节点和大文件：路径 → (大小, 阻拦)。勾选、汇总、移动都用它。
    fn index(&self, main: &Arc<SpaceReport>) -> HashMap<PathBuf, (Size, Option<HoldBack>)> {
        fn walk(node: &SpaceNode, out: &mut HashMap<PathBuf, (Size, Option<HoldBack>)>) {
            if node.kind != NodeKind::Other {
                out.insert(node.path.clone(), (node.size, node.hold_back.clone()));
            }
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut out = HashMap::new();
        for report in self.reports(main) {
            walk(&report.root, &mut out);
            for file in &report.large_files {
                out.insert(
                    file.path.clone(),
                    (Size::Known(file.size), file.hold_back.clone()),
                );
            }
        }
        out
    }

    fn toggle(&mut self, path: PathBuf, can: bool, on: bool, cx: &mut Context<Self>) {
        model::set_one(&mut self.selection, &path, can, on);
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(main) = self.report(cx) else { return };
        if self.busy || self.selection.is_empty() {
            return;
        }
        let index = self.index(&main);
        let candidates: Vec<Removable> = self
            .selection
            .iter()
            .filter_map(|p| {
                index.get(p).map(|(size, hold_back)| Removable {
                    path: p.clone(),
                    size: *size,
                    hold_back: hold_back.clone(),
                })
            })
            .collect();
        let picked = model::picked(
            &self.selection,
            candidates.iter().map(|c| (c.path.as_path(), c.size)),
        );
        let lang = strings::lang(cx);
        let plan = TrashPlan {
            op: TrashOp::Paths {
                batch: Batch {
                    kind: RecordKind::Files,
                    title: strings::space_batch_title(
                        lang,
                        &self.services.env().display(&main.root.path),
                    ),
                    bundle_id: None,
                    source: Source::App,
                },
                candidates,
            },
            selected: self.selection.iter().cloned().collect(),
            picked,
            extra: Vec::new(),
        };
        ui::confirm_trash(plan, window, cx);
    }

    fn render_roots(&self, lang: Lang, running: bool, cx: &mut Context<Self>) -> Div {
        let env = self.services.env();
        let current_root = self.scans.read(cx).space_root.clone();
        let roots: [(Key, PathBuf, IconName); 5] = [
            (Key::SpaceHome, env.home.clone(), IconName::House),
            (
                Key::SpaceDownloads,
                env.home.join("Downloads"),
                IconName::Download,
            ),
            (
                Key::SpaceDocuments,
                env.home.join("Documents"),
                IconName::FileText,
            ),
            (
                Key::SpaceLibrary,
                env.user_library.clone(),
                IconName::Library,
            ),
            (
                Key::SpaceApplications,
                PathBuf::from("/Applications"),
                IconName::AppWindow,
            ),
        ];
        h_flex()
            .px_6()
            .pb_3()
            .gap_1()
            .flex_wrap()
            .children(
                roots
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (key, path, icon))| {
                        let active = current_root.as_ref() == Some(&path);
                        Button::new(("space-root", ix))
                            .small()
                            .when(active, |b| b.primary())
                            .when(!active, |b| b.ghost())
                            .icon(Icon::new(icon))
                            .label(ui(lang, key))
                            .disabled(running)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.analyze(path.clone(), cx)),
                            )
                    }),
            )
            .child(
                Button::new("space-pick")
                    .small()
                    .ghost()
                    .icon(Icon::new(IconName::FolderOpen))
                    .label(ui(lang, Key::SpaceChoose))
                    .disabled(running)
                    .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx))),
            )
    }

    fn render_breadcrumbs(&self, root: &Path, current: &Path, cx: &mut Context<Self>) -> Div {
        let env = self.services.env().clone();
        let crumbs = model::breadcrumbs(root, current);
        let last = crumbs.len().saturating_sub(1);
        h_flex()
            .gap_0p5()
            .flex_wrap()
            .children(crumbs.into_iter().enumerate().map(|(ix, path)| {
                let label = if ix == 0 {
                    env.display(&path)
                } else {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                h_flex()
                    .gap_0p5()
                    .when(ix > 0, |this| {
                        this.child(
                            Icon::new(IconName::ChevronRight)
                                .xsmall()
                                .text_color(ui::muted(cx)),
                        )
                    })
                    .child(
                        Button::new(("crumb", ix))
                            .xsmall()
                            .ghost()
                            .label(label)
                            .disabled(ix == last)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.open_dir(path.clone(), cx)),
                            ),
                    )
            }))
    }

    fn render_children(&self, node: &SpaceNode, lang: Lang, cx: &mut Context<Self>) -> Div {
        let muted = ui::muted(cx);
        let primary = cx.theme().primary;
        let env = self.services.env().clone();
        if node.children.is_empty() {
            return ui::empty_state(
                IconName::Folder,
                ui(lang, Key::SpaceEmptyDir),
                env.display(&node.path),
                cx,
            );
        }
        v_flex().children(node.children.iter().enumerate().map(|(ix, child)| {
            let fraction = model::share(child.size, node.size);
            let can = child.kind != NodeKind::Other && selectable(child.hold_back.as_ref());
            let path = child.path.clone();
            let open = child.path.clone();
            let is_dir = child.kind == NodeKind::Dir;
            let icon = match child.kind {
                NodeKind::Dir => IconName::Folder,
                NodeKind::File => IconName::File,
                NodeKind::Other => IconName::Ellipsis,
            };
            let name = if child.kind == NodeKind::Other {
                strings::other_items(lang, child.files)
            } else {
                child.name.clone()
            };
            let hold_back = child
                .hold_back
                .as_ref()
                .map(|h| rind_i18n::describe_hold_back(lang, h));
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .when(ix > 0, |this| {
                    this.border_t_1().border_color(cx.theme().border)
                })
                .child(
                    Checkbox::new(SharedString::from(format!("sn-{ix}")))
                        .checked(self.selection.contains(&child.path))
                        .disabled(!can)
                        .on_change(cx.listener(move |this, on: &bool, _, cx| {
                            this.toggle(path.clone(), can, *on, cx)
                        })),
                )
                .child(
                    h_flex()
                        .id(SharedString::from(format!("sn-row-{ix}")))
                        .flex_1()
                        .min_w_0()
                        .gap_3()
                        .when(is_dir, |this| {
                            this.cursor_pointer()
                                .hover(|s| s.text_color(primary))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.open_dir(open.clone(), cx)
                                }))
                        })
                        .child(Icon::new(icon).small().text_color(if is_dir {
                            primary
                        } else {
                            muted
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis_middle()
                                        .child(name),
                                )
                                .when_some(hold_back, |this, reason| {
                                    this.child(
                                        h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(Icon::new(IconName::ShieldCheck).xsmall())
                                            .child(div().truncate().child(reason)),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(140.))
                                .child(ui::bar(fraction, primary, cx)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(48.))
                                .text_xs()
                                .text_right()
                                .text_color(muted)
                                .child(format!("{:.1}%", fraction * 100.0)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(96.))
                                .text_xs()
                                .text_right()
                                .text_color(muted)
                                .child(strings::file_count(lang, child.files)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(84.))
                                .text_sm()
                                .text_right()
                                .font_semibold()
                                .child(format!(
                                    "{}{}",
                                    rind_i18n::format_size(lang, child.size),
                                    if child.partial { "+" } else { "" }
                                )),
                        ),
                )
        }))
    }

    fn render_large_files(&self, report: &SpaceReport, lang: Lang, cx: &mut Context<Self>) -> Div {
        let muted = ui::muted(cx);
        let env = self.services.env().clone();
        if report.large_files.is_empty() {
            return ui::empty_state(
                IconName::File,
                ui(lang, Key::SpaceNoLargeFiles),
                ui(lang, Key::SpaceNoLargeFilesBody),
                cx,
            );
        }
        let now = chrono::Utc::now();
        v_flex().children(report.large_files.iter().enumerate().map(|(ix, file)| {
            let can = selectable(file.hold_back.as_ref());
            let path = file.path.clone();
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let when = |label: Key, at: Option<chrono::DateTime<chrono::Utc>>| {
                at.map(|at| {
                    format!(
                        "{} {}",
                        ui(lang, label),
                        strings::relative_time(lang, now, at)
                    )
                })
                .unwrap_or_default()
            };
            h_flex()
                .gap_3()
                .px_4()
                .py_2()
                .when(ix > 0, |this| {
                    this.border_t_1().border_color(cx.theme().border)
                })
                .child(
                    Checkbox::new(SharedString::from(format!("lf-{ix}")))
                        .checked(self.selection.contains(&file.path))
                        .disabled(!can)
                        .on_change(cx.listener(move |this, on: &bool, _, cx| {
                            this.toggle(path.clone(), can, *on, cx)
                        })),
                )
                .child(Icon::new(IconName::File).small().text_color(muted))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(name))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_middle()
                                .child(env.display(&file.path)),
                        ),
                )
                .child(
                    v_flex()
                        .flex_shrink_0()
                        .w(px(150.))
                        .text_xs()
                        .text_color(muted)
                        .child(when(Key::SpaceModified, file.modified))
                        .child(when(Key::SpaceAccessed, file.accessed)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(84.))
                        .text_sm()
                        .text_right()
                        .font_semibold()
                        .child(ui::bytes(lang, file.size)),
                )
        }))
    }
}

impl TrashHost for SpacePage {
    fn services(&self) -> Arc<Services> {
        self.services.clone()
    }
    fn scans(&self) -> Entity<Scans> {
        self.scans.clone()
    }
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }
    fn after_trash(&mut self, outcome: &RemovalOutcome, cx: &mut Context<Self>) {
        let moved: HashSet<PathBuf> = outcome.moved.iter().map(|(p, _)| p.clone()).collect();
        self.subtrees = self
            .subtrees
            .iter()
            .map(|r| Arc::new(model::prune_space(r, &moved)))
            .collect();
        self.selection.retain(|p| !model::is_gone(p, &moved));
        if let Some(current) = &self.current {
            if model::is_gone(current, &moved) {
                self.current = self.report(cx).map(|r| r.root.path.clone());
            }
        }
        cx.notify();
    }
}

impl Render for SpacePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = strings::lang(cx);
        let (running, progress, report, root) = {
            let scans = self.scans.read(cx);
            (
                scans.space.is_running(),
                scans.space.progress().cloned(),
                scans.space.data.clone(),
                scans.space_root.clone(),
            )
        };
        let action = if running {
            Button::new("space-cancel")
                .small()
                .outline()
                .icon(Icon::new(IconName::X))
                .label(ui(lang, Key::Cancel))
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                .into_any_element()
        } else if let Some(root) = root.clone() {
            Button::new("space-again")
                .small()
                .outline()
                .icon(Icon::new(IconName::RefreshCw))
                .label(ui(lang, Key::Rescan))
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, _, cx| this.analyze(root.clone(), cx)))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        let header = ui::page_header(
            IconName::ChartPie,
            ui(lang, Key::NavSpace),
            ui(lang, Key::SpaceSubtitle),
            action,
            cx,
        );
        let mut page = v_flex()
            .size_full()
            .child(header)
            .child(self.render_roots(lang, running, cx));

        if running {
            let root_label = root
                .as_ref()
                .map(|r| self.services.env().display(r))
                .unwrap_or_default();
            page = page.child(
                div().px_6().pb_3().child(
                    ui::card(cx)
                        .p_4()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(strings::analyzing(lang, &root_label)),
                        )
                        .child(ui::progress_block(
                            "space-progress",
                            lang,
                            progress.as_ref(),
                            cx,
                        )),
                ),
            );
        }
        let Some(report) = report else {
            if running {
                return page;
            }
            let home = self.services.env().home.clone();
            return page.child(
                ui::empty_state(
                    IconName::ChartPie,
                    ui(lang, Key::SpaceIdleTitle),
                    ui(lang, Key::SpaceIdleBody),
                    cx,
                )
                .child(
                    Button::new("space-start")
                        .primary()
                        .icon(Icon::new(IconName::ScanSearch))
                        .label(ui(lang, Key::SpaceAnalyzeHome))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.analyze(home.clone(), cx)),
                        ),
                ),
            );
        };

        // 当前看的目录：找不到（比如被移走了）就回到根。
        let reports = self.reports(&report);
        let current = self
            .current
            .clone()
            .filter(|c| model::locate(reports.iter().map(|r| r.as_ref()), c).is_some())
            .unwrap_or_else(|| report.root.path.clone());
        let node = model::locate(reports.iter().map(|r| r.as_ref()), &current)
            .cloned()
            .unwrap_or_else(|| report.root.clone());

        let mut notices = v_flex().px_6().gap_2();
        if report.incomplete {
            notices = notices.child(ui::banner(
                Tone::Warning,
                IconName::TriangleAlert,
                ui(lang, Key::SpaceIncomplete),
                cx,
            ));
        }
        if !report.unreadable.is_empty() {
            notices = notices.child(ui::banner(
                Tone::Info,
                IconName::Info,
                strings::unreadable_dirs(lang, report.unreadable.len()),
                cx,
            ));
        }
        if let Some(message) = self
            .services
            .ledger_error()
            .map(|e| strings::ledger_unavailable(lang, e))
        {
            notices = notices.child(ui::banner(
                Tone::Danger,
                IconName::TriangleAlert,
                message,
                cx,
            ));
        }

        let summary = h_flex()
            .px_6()
            .py_3()
            .gap_6()
            .child(ui::stat(
                ui(lang, Key::SpaceTotal),
                format!(
                    "{}{}",
                    rind_i18n::format_size(lang, report.root.size),
                    if report.root.partial { "+" } else { "" }
                ),
                cx,
            ))
            .child(ui::stat(
                ui(lang, Key::SpaceFiles),
                strings::file_count(lang, report.root.files),
                cx,
            ))
            .child(ui::stat(
                ui(lang, Key::CardLargeFiles),
                strings::count(lang, report.large_files.len()),
                cx,
            ))
            .child(div().flex_1())
            .child(
                TabBar::new("space-tabs")
                    .segmented()
                    .selected_index(self.tab)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.tab = *ix;
                        cx.notify();
                    }))
                    .child(Tab::new().label(ui(lang, Key::SpaceTabDirs)))
                    .child(Tab::new().label(ui(lang, Key::SpaceTabLarge))),
            );

        let drilling = self.drill.as_ref().filter(|d| d.path == current);
        let content = if self.tab == 0 {
            let crumbs = self.render_breadcrumbs(&report.root.path, &current, cx);
            let list = match drilling {
                Some(drill) => v_flex().p_4().child(ui::progress_block(
                    "space-drill",
                    lang,
                    drill.progress.as_ref(),
                    cx,
                )),
                None => self.render_children(&node, lang, cx),
            };
            v_flex()
                .gap_2()
                .child(crumbs)
                .when(node.partial, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(ui::muted(cx))
                            .child(ui(lang, Key::SpacePartialDir)),
                    )
                })
                .child(ui::card(cx).overflow_hidden().child(list))
        } else {
            v_flex().child(
                ui::card(cx)
                    .overflow_hidden()
                    .child(self.render_large_files(&report, lang, cx)),
            )
        };

        let picked = {
            let index = self.index(&report);
            model::picked(
                &self.selection,
                self.selection
                    .iter()
                    .filter_map(|p| index.get(p).map(|(size, _)| (p.as_path(), *size)))
                    .collect::<Vec<_>>(),
            )
        };
        let no_ledger = self.services.ledger_error().is_some();
        page.child(notices)
            .child(summary)
            .child(
                div()
                    .id("space-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(div().px_6().pb_4().child(content)),
            )
            .child(
                ui::footer_bar(cx)
                    .child(ui::picked_summary(lang, picked, cx))
                    .child(
                        Button::new("space-trash")
                            .danger()
                            .icon(Icon::new(IconName::Trash))
                            .label(ui(lang, Key::MoveToTrash))
                            .loading(self.busy)
                            .disabled(picked.count == 0 || self.busy || running || no_ledger)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}
