//! 各页共用的界面零件：页头、卡片、大小条、提示条、空状态、头像，以及统一的"移到废纸篓"流程。
//! 间距与字号层级：页面左右 `px_6`；页标题 `text_lg` 半粗；分节标题 `text_sm` 半粗；正文 `text_sm`；
//! 次要信息 `text_xs` + `muted_foreground`。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rind_core::{Confidence, RemovalOutcome, ScanProgress, Size};

use crate::model::{self, Picked};
use crate::services::{ExecuteFailure, Services, TrashOp};
use crate::store::Scans;
use crate::strings::{self, ui, Key, Lang};

/// 语气：决定颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Info,
    Success,
    Warning,
    Danger,
    Neutral,
}

pub fn tone_color(tone: Tone, cx: &App) -> Hsla {
    let theme = cx.theme();
    match tone {
        Tone::Primary => theme.primary,
        Tone::Info => theme.info,
        Tone::Success => theme.success,
        Tone::Warning => theme.warning,
        Tone::Danger => theme.danger,
        Tone::Neutral => theme.muted_foreground,
    }
}

pub fn muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

/// 带底色的图标块。
pub fn icon_tile(icon: IconName, tone: Tone, size: Pixels, cx: &App) -> Div {
    let color = tone_color(tone, cx);
    h_flex()
        .flex_shrink_0()
        .size(size)
        .justify_center()
        .rounded(cx.theme().radius_lg)
        .bg(color.opacity(0.12))
        .text_color(color)
        .child(Icon::new(icon).small())
}

/// 统一的页头：图标、标题、一句说明，右边是操作区。
pub fn page_header(
    icon: IconName,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    actions: impl IntoElement,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .flex_shrink_0()
        .gap_3()
        .px_6()
        .pt_5()
        .pb_4()
        .child(icon_tile(icon, Tone::Primary, px(36.), cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_lg().font_semibold().child(title.into()))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted(cx))
                        .truncate()
                        .child(subtitle.into()),
                ),
        )
        .child(h_flex().flex_shrink_0().gap_2().child(actions))
}

/// 卡片容器。
pub fn card(cx: &App) -> Div {
    v_flex()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}

/// 细长的比例条。`fraction` 在 0..=1。
pub fn bar(fraction: f32, color: Hsla, cx: &App) -> Div {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .h(px(6.))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(cx.theme().muted)
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(color)
                .w(relative(fraction))
                .when(fraction > 0.0, |this| this.min_w(px(3.))),
        )
}

/// 把握徽章：确定绿、很可能蓝、可能灰。
pub fn confidence_tag(lang: Lang, confidence: Confidence) -> Tag {
    let tag = match confidence {
        Confidence::Certain => Tag::success(),
        Confidence::Likely => Tag::info(),
        Confidence::Possible => Tag::secondary(),
    };
    tag.xsmall()
        .child(rind_i18n::describe_confidence(lang, confidence))
}

/// 一条带底色的提示。
pub fn banner(tone: Tone, icon: IconName, message: impl IntoElement, cx: &App) -> Div {
    let color = tone_color(tone, cx);
    h_flex()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .text_sm()
        .bg(color.opacity(0.1))
        .border_1()
        .border_color(color.opacity(0.25))
        .child(Icon::new(icon).small().text_color(color))
        .child(div().flex_1().min_w_0().child(message))
}

/// 页面中间的空状态：图标、一句标题、一句说明；调用方可以再 `.child(按钮)`。
pub fn empty_state(
    icon: IconName,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .p_8()
        .child(icon_tile(icon, Tone::Neutral, px(48.), cx))
        .child(div().pt_1().font_semibold().child(title.into()))
        .child(
            div()
                .max_w(px(420.))
                .text_sm()
                .text_center()
                .text_color(muted(cx))
                .child(body.into()),
        )
}

/// 扫描进度这一行要说什么。
pub fn scan_message(lang: Lang, progress: Option<&ScanProgress>) -> String {
    match progress {
        Some(ScanProgress::Location(location)) => strings::scanning_location(lang, location),
        Some(ScanProgress::Measuring { done, total }) if *total > 0 => {
            strings::measuring(lang, *done, *total)
        }
        Some(ScanProgress::Measuring { done, .. }) => strings::files_seen(lang, *done),
        _ => ui(lang, Key::Scanning).to_string(),
    }
}

/// 进度块：转圈 + 文字 + 进度条（不知道总量时用不确定动画）。
pub fn progress_block(
    id: &'static str,
    lang: Lang,
    progress: Option<&ScanProgress>,
    cx: &App,
) -> Div {
    let percent = progress.and_then(model::progress_percent);
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(muted(cx))
                .child(Spinner::new().small())
                .child(div().truncate().child(scan_message(lang, progress))),
        )
        .child(
            Progress::new(id)
                .loading(percent.is_none())
                .value(percent.unwrap_or(0.0)),
        )
}

/// 字母头像的底色：按名字散列挑一个，深浅主题下都够亮。
fn avatar_color(name: &str) -> Hsla {
    let hash = name
        .chars()
        .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    hsla((hash % 360) as f32 / 360.0, 0.55, 0.52, 1.0)
}

fn letter_avatar(name: &str, size: Pixels) -> AnyElement {
    let letter: String = name
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default();
    h_flex()
        .flex_shrink_0()
        .size(size)
        .justify_center()
        .rounded(size * 0.24)
        .bg(avatar_color(name))
        .text_color(white())
        .font_semibold()
        .text_size(size * 0.45)
        .child(letter.to_uppercase())
        .into_any_element()
}

/// App 头像：有图标就画图标（读失败时退回字母），没有就画字母。
pub fn app_avatar(name: &str, icon: Option<PathBuf>, size: Pixels) -> AnyElement {
    match icon {
        Some(path) => {
            let name = name.to_string();
            img(path)
                .flex_shrink_0()
                .size(size)
                .with_fallback(move || letter_avatar(&name, size))
                .into_any_element()
        }
        None => letter_avatar(name, size),
    }
}

/// 一个小号的"标签: 值"。
pub fn stat(label: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> Div {
    v_flex()
        .gap_0p5()
        .child(div().text_xs().text_color(muted(cx)).child(label.into()))
        .child(div().text_sm().font_semibold().child(value.into()))
}

/// 已知大小，格式化。
pub fn bytes(lang: Lang, n: u64) -> String {
    rind_i18n::format_size(lang, Size::Known(n))
}

// ---------------------------------------------------------------------------------------------
// 移到废纸篓：确认 → 后台移动 → 通知 → 从结果里剪掉

/// 一次要移动什么。
#[derive(Clone)]
pub struct TrashPlan {
    pub op: TrashOp,
    pub selected: HashSet<PathBuf>,
    pub picked: Picked,
    /// 确认框里额外说的话（比如"包含 3 个启动项"）。
    pub extra: Vec<SharedString>,
}

/// 能发起移动的页面。
pub trait TrashHost: Sized + 'static {
    fn services(&self) -> Arc<Services>;
    fn scans(&self) -> Entity<Scans>;
    fn set_busy(&mut self, busy: bool);
    /// 移动完成后（结果已从共享仓库剪掉）页面自己还要做的事。
    fn after_trash(&mut self, _outcome: &RemovalOutcome, _cx: &mut Context<Self>) {}
}

/// 弹确认框，确认后在后台移动。
pub fn confirm_trash<V: TrashHost>(plan: TrashPlan, window: &mut Window, cx: &mut Context<V>) {
    if plan.selected.is_empty() {
        return;
    }
    let lang = strings::lang(cx);
    let first = if plan.picked.bytes == 0 && plan.picked.unknown == 0 {
        strings::confirm_count(lang, plan.picked.count)
    } else {
        strings::confirm_items(lang, plan.picked.count, &bytes(lang, plan.picked.bytes))
    };
    let mut lines: Vec<SharedString> = vec![first.into()];
    lines.extend(plan.extra.iter().cloned());
    if plan.picked.unknown > 0 {
        lines.push(strings::confirm_unknown(lang, plan.picked.unknown).into());
    }
    lines.push(ui(lang, Key::ConfirmNote).into());
    lines.push(ui(lang, Key::EmptyTrashHint).into());
    let weak = cx.weak_entity();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let weak = weak.clone();
        let plan = plan.clone();
        alert
            .confirm()
            .title(ui(lang, Key::ConfirmTitle))
            .description(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .children(lines.iter().cloned().map(|line| div().child(line))),
            )
            .ok_text(ui(lang, Key::MoveToTrash))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(ui(lang, Key::Cancel))
            .on_ok(move |_, window, cx| {
                let plan = plan.clone();
                weak.update(cx, |this, cx| run_trash(this, plan, window, cx))
                    .ok();
                true
            })
    });
}

fn run_trash<V: TrashHost>(
    this: &mut V,
    plan: TrashPlan,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    this.set_busy(true);
    cx.notify();
    let services = this.services();
    cx.spawn_in(window, async move |this, cx| {
        let result = cx
            .background_spawn(async move { services.trash(&plan.op, &plan.selected) })
            .await;
        this.update_in(cx, |this, window, cx| {
            this.set_busy(false);
            if let Some(outcome) = notify_outcome(&result, window, cx) {
                finish_trash(this, &outcome, cx);
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// 移动之后：从共享结果里剪掉已移走的，再让页面做自己的收尾。
pub fn finish_trash<V: TrashHost>(this: &mut V, outcome: &RemovalOutcome, cx: &mut Context<V>) {
    let moved: HashSet<PathBuf> = outcome.moved.iter().map(|(p, _)| p.clone()).collect();
    this.scans()
        .update(cx, |scans, cx| scans.forget(&moved, cx));
    this.after_trash(outcome, cx);
}

/// 把移动结果变成通知。东西动了（含 History 没记全）就返回那份结果，什么都没动返回 None。
pub fn notify_outcome(
    result: &Result<RemovalOutcome, ExecuteFailure>,
    window: &mut Window,
    cx: &mut App,
) -> Option<RemovalOutcome> {
    let lang = strings::lang(cx);
    match result {
        Ok(outcome) => {
            let summary = summarize(lang, outcome);
            let note = if outcome.moved.is_empty() {
                Notification::warning(summary).title(ui(lang, Key::NothingMoved))
            } else {
                Notification::success(format!("{summary} {}", ui(lang, Key::EmptyTrashHint)))
                    .title(ui(lang, Key::ResultTitle))
            };
            window.push_notification(note, cx);
            Some(outcome.clone())
        }
        // 东西已经在废纸篓里了，只是 History 没记全：当结果看，另外说清楚。
        Err(ExecuteFailure::Partial { error, partial }) => {
            window.push_notification(
                Notification::warning(format!(
                    "{} {}",
                    summarize(lang, partial),
                    strings::partial_note(lang, error)
                ))
                .title(ui(lang, Key::PartialTitle)),
                cx,
            );
            Some(partial.as_ref().clone())
        }
        Err(failure) => {
            window.push_notification(
                Notification::error(strings::describe_failure(lang, failure))
                    .title(ui(lang, Key::NothingMoved)),
                cx,
            );
            None
        }
    }
}

pub fn summarize(lang: Lang, outcome: &RemovalOutcome) -> String {
    strings::result_summary(
        lang,
        outcome.moved.len(),
        &bytes(lang, outcome.freed),
        outcome.refused.len(),
        outcome.failed.len(),
    )
}

/// 页面底部的操作栏：左边说明已选多少，右边是主按钮。
pub fn footer_bar(cx: &App) -> Div {
    h_flex()
        .flex_shrink_0()
        .gap_3()
        .px_6()
        .py_3()
        .border_t_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().sidebar)
}

/// "已选 N 项 · 可释放 X"，大小未知的另说。
pub fn picked_summary(lang: Lang, picked: Picked, cx: &App) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .child(div().text_sm().font_semibold().child(strings::picked_line(
            lang,
            picked.count,
            &bytes(lang, picked.bytes),
        )))
        .child(
            div()
                .text_xs()
                .text_color(muted(cx))
                .truncate()
                .child(if picked.unknown > 0 {
                    strings::confirm_unknown(lang, picked.unknown)
                } else {
                    ui(lang, Key::EmptyTrashHint).to_string()
                }),
        )
}
