# 参与开发

谢谢愿意帮忙。Rind 是会移动别人文件的工具，所以这里对「正确」的要求比对「多」的要求高。

## 开发环境

- macOS（开发与测试都在 macOS 26 / Apple Silicon 上；更老的系统没验证过）
- Rust **1.85+**（`rust-toolchain.toml` 跟 stable 走）
- 不需要完整 Xcode：Command Line Tools + Rust 就够（界面用运行时编译的 Metal shader）

```sh
git clone git@github.com:ahjdzx/rind.git
cd rind
cargo build -p rind-cli          # 命令行
cargo build -p rind-app          # 桌面应用（首次编译依赖较多，慢是正常的）
cargo test --workspace --exclude rind-app
```

小提示：给界面单独用一个 target 目录，能避免和测试抢编译锁——`CARGO_TARGET_DIR=target/app cargo build -p rind-app`。

## 提交之前

这三条 CI 也会跑，本地先过一遍能省时间：

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude rind-app --all-targets -- -D warnings
cargo test --workspace --exclude rind-app
```

界面改动也请自己跑一次 `cargo clippy -p rind-app --all-targets -- -D warnings`，并且至少手动打开看一眼——
界面没有自动化测试，布局错了测试不会告诉你。

## 布局

```
crates/rind-core    扫描、匹配、保护区、废纸篓、History、卸载/清理/孤儿/空间/启动项
crates/rind-i18n    中英文案（覆盖全部枚举，两边都不许缺）
crates/rind-cli     命令行 `rind`
crates/rind-app     GPUI 桌面应用
rules/*.toml        具体 App 的清理规则，编译时内嵌
docs/               架构、设计、安全、使用、里程碑
```

## 不可破坏的不变量

改动只要碰这四条中的任何一条，都请在 PR 里单独说明为什么：

1. **唯一的移动出口。** 所有会移动文件的功能都走 `remove::trash_items`（或它带回调的变体）。
   不要新开一条绕过它的路径。
2. **预演等于真跑。** `--dry-run` 必须和真跑做同样的检查、给同样的判断（`moved` / `refused` / `freed` 逐项一致）。
3. **fail closed。** 拿不准时拒绝或标成不可选，不要把不确定的东西当成安全。保护区判定按盘上真实路径，
   不做字符串比较。
4. **测试不碰真实系统。** 测试里的路径一律来自 `Env::for_test`；`launchctl` 走注入的假实现；
   需要真实废纸篓的测试把临时件放进 `~/Library/Caches/rind-tests`（同卷），跑完自己放回。

另外两条习惯，请尽量保持：

- **文案要有中英两份**，并在 `rind-i18n` 里加测试；不要硬编码字符串。
- **代码注释是中文**，语气平实；面向用户的文案不要用术语。

## 加一个清理规则

`rules/<bundle-id>.toml`，参考 `rules/com.tencent.xinWeChat.toml`：

```toml
bundle_id = "com.example.app"
name = "示例"
aliases = ["Example"]

[[paths]]
path = "~/Library/Containers/com.example.app"
confidence = "certain"
note = "沙盒容器"
```

规则只能**补充证据**，不能覆盖保护区。请写清路径是从哪来的（cask 的 zap 段？本机实测？），
最好在 PR 里贴一下 `rind leftovers <App> --json` 里对应的那一条。

## 提交信息

中文，第一行说「为什么」，正文说清影响面。仓库的提交里带了几个结构化 trailer，沿用它们会让历史更好读：

```text
fix: 预演会漏报含照片库的目录

真实移动前会往下 6 层找媒体库，预演只看了扫描结果，于是 --dry-run 说会移动、真跑却拒绝。

Constraint: 预演与真跑必须逐项一致
Rejected: 只在 CLI 层补检查 | 界面里点确认也会遇到同一个问题
Confidence: high
Scope-risk: narrow
Not-tested: 跨卷放回的真实路径
```

可用 trailer：`Constraint:`、`Rejected:`、`Directive:`、`Confidence:`（high/medium/low）、
`Scope-risk:`（narrow/moderate/broad）、`Not-tested:`。

## Pull Request

- 一个 PR 做一件事。
- 说明「改了什么、为什么、怎么验证的」，验证要有真实输出（命令 + 结果），不要只写「已测试」。
- 涉及移动文件、保护区、History 的改动，请把新增的测试名贴出来。
- 行为变化请同步更新 `CHANGELOG.md`；设计上的取舍写进 `docs/`。

## 安全问题

不要开公开 issue，按 [SECURITY.md](SECURITY.md) 私下报。
