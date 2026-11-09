# 里程碑

状态只有三种：**已完成**、**进行中**、**候选**（还没定，写在这里是为了有个地方讨论）。
发布计划跟着[语义化版本](https://semver.org/lang/zh-CN/)走，改动记在 [CHANGELOG.md](../CHANGELOG.md)。

## M0 · 调研（2026-11-06，已完成）

把同类工具读了一遍，决定 Rind 要吸收什么、不要什么：

- [Peel](https://github.com/TuguiDragos/Peel)：逐文件给证据与置信度、保护区、最小权限的特权助手。
- [Mole](https://github.com/tw93/Mole)：CLI 优先、`--dry-run` 当默认习惯、对 agent 友好。
- [腾讯柠檬](https://github.com/Tencent/lemon-cleaner)：数据驱动的清理规则库（尤其国内软件）。
- [Pearcleaner](https://github.com/alienator88/Pearcleaner)：二进制瘦身、Finder 集成。

结论：**先做卸载主线**，把「可解释 + 可撤销」做扎实，其余功能按同样的规矩往上加。

## M1 · 卸载主线（2026-11-06，已完成）

提交 `66f5d77`。App 发现 → 残留扫描与证据打分 → 计划（certain / likely / possible）→ 移入废纸篓 →
History 放回。crate 拆成 `rind-core` / `rind-i18n` / `rind-cli` / `rind-app`。

## M2 · 四个新功能 + 界面重做（2026-11-09，已完成）

提交 `403a78f`。孤儿残留、缓存与开发产物清理、空间分析、启动项；界面加总览首页并重做导航；
CLI 补 `status` / `orphans` / `clean` / `analyze` / `trash` / `launch`。
两轮独立审查（正确性 + 对抗性安全）发现的问题全部修完。

## M3 · 开源化（2026-11-09，已完成）

- [x] LICENSE（GPL-3.0-or-later）、CHANGELOG、CONTRIBUTING、SECURITY、行为准则、issue / PR 模板
- [x] 设计与使用文档：`docs/DESIGN.md`、`docs/USAGE.md`、`docs/SAFETY.md`
- [x] 本地里程碑（本文件）
- [x] 推送到 GitHub
- [x] 持续集成：`fmt` + `clippy` + 测试（macOS runner）
- [x] 转为公开仓库（2026-11-09）
- [x] 打 tag `v0.1.0` 并写 Release notes（[releases/tag/v0.1.0](https://github.com/ahjdzx/rind/releases/tag/v0.1.0)），首个 GitHub Actions 运行通过

## M4 · 能给别人装（进行中）

目标是「别人不用装 Rust 也能用上」，顺序按依赖关系排：

1. [x] `.dmg` 打包：tag（`v*`）由 GitHub Actions 自动构建并发布（`.github/workflows/release.yml`），
   本地用 `scripts/package-dmg.sh`（2026-11-09）。
2. [ ] Developer ID 签名 + 公证 —— 没有公证，用户第一次打开要右键「打开」；这一步才是体验的关键。
3. [ ] `Homebrew cask`（依赖签名与公证）。
4. [ ] 首次启动引导「完全磁盘访问」（没有它，沙盒容器读不了，卸载体验是残的）。
5. [x] README 截图与动图预览，按语言分目录（`docs/images/{en,zh}/`，2026-11-09）。

顺带候选：universal（arm64 + x86_64）构建——现在 DMG 只有 Apple Silicon 产物。

## M5 · 把清理做得更准（候选）

- 规则库扩充：现在 11 个 App（国内常用软件优先），候选是补到 50 个左右，并且每条都要在本机核对过。
- 「重建代价高」的判定再细分（现在是一张固定清单）。
- 清理结果里显示「上次清过是什么时候、清了多少」（History 已经有数据，缺的是聚合视图）。
- 空间分析：增量扫描 / 缓存上一棵树，避免每次都全盘重扫。
- 启动项：显示每个项最近一次运行时间。

## 明确不做

- 实时防护 / 杀毒 / 常驻监控。
- 需要联网的功能（云规则、账号、遥测）。
- 自动清理（没有你点确认就不动手）。
- 提权搬运 `/Library` 下的系统级内容。
- 重复文件与相似照片：这两类误判率高，而且删除不可逆的风险大，不在 Rind 的定位里。

## 怎么参与

想认领候选里的某一条：开一个 issue 说清你打算怎么做（尤其是涉及移动文件的部分），
再看 [CONTRIBUTING.md](../CONTRIBUTING.md) 的「不可破坏的不变量」。
