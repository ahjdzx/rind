# Rind

卸载 Mac 上的 App，连同它留下的每个文件，并**说明每个文件为什么属于它、有多大把握**。
只移入废纸篓，从不直接删除；History 一键放回。Rust 编写，界面基于 GPUI。

吸收 [Peel](https://github.com/TuguiDragos/Peel)（逐文件证据与保护区）、
[Mole](https://github.com/tw93/Mole)（CLI 优先、`--dry-run`）、
[腾讯柠檬](https://github.com/Tencent/lemon-cleaner)（数据驱动的规则库）与
[Pearcleaner](https://github.com/alienator88/Pearcleaner) 的长处，然后按自己的规矩重做：
**可解释、可撤销、保守默认、预演等于真跑**。

> 状态：**0.1.0**，源码构建，还没有预编译包。要装得自己编（见下）。

## 能做什么

- **卸载**：App 与它的残留一起，逐项给出理由（bundle id、内嵌组件、App Group、签名 Team、Homebrew cask、
  pkg 回执）与置信度（certain / likely / possible）。
- **孤儿残留**：已删除 App 留下的文件——刚拖进废纸篓的也算。
- **清理**：缓存、日志、浏览器缓存、Xcode、包管理器缓存、项目构建产物；重建代价高的只列不勾。
- **空间**：谁最占地方、大文件在哪（按实际占用算，APFS clone 与硬链接去重）。
- **启动项**：LaunchAgents / LaunchDaemons 的查看、停用、移除。
- **命令行与界面同一套判断**：`rind` 有 12 个子命令，全部支持 `--json`，会移动东西的支持 `--dry-run`。

## 安全原则

- **只进废纸篓。** 每一项都用 `NSFileManager.trashItem` 移动，History 记录它在废纸篓里的实际位置，随时放回。
  没有记录的东西不会进废纸篓。
- **保护区任何选择都不能覆盖。** iCloud Drive、钥匙串、SSH / GPG 密钥、Mail、Messages、Safari、照片与音乐库、
  iPhone 备份、桌面 / 文稿 / 下载等文件夹本身、系统路径、带加密钱包扩展的浏览器 profile。`rind protected` 列出全部。
- **不替你选拿不准的。** 只有 certain / likely、没有别的 App 共用、大小已知的项才会被预先勾上；
  读不了的目录显示「未知」，绝不显示 0。
- **移动前再查一次。** 解析成盘上的真实路径，和扫描时不一致就拒绝；外层目录里有不能动的东西时整个外层拒绝。
- 没有账号、没有遥测、没有常驻进程——你不按按钮，它什么都不做。

细节（含写明的边界）在 [docs/SAFETY.md](docs/SAFETY.md)。

## 构建

需要 macOS 与 Rust 1.85+。**不需要完整 Xcode**（GPUI 用运行时编译的 Metal shader）。

```sh
git clone git@github.com:ahjdzx/rind.git
cd rind
cargo build --release -p rind-cli      # 命令行 `rind`
cargo build --release -p rind-app      # 桌面应用
cargo test --workspace --exclude rind-app
```

## 先用一下（只读）

```sh
./target/release/rind status             # 磁盘 + 可卸载的 App 数
./target/release/rind leftovers 微信      # 微信留下了什么，每个文件为什么算它的
./target/release/rind protected          # 它永远不动的地方

RIND_DEMO=1 cargo run -p rind-app        # 桌面应用，示例数据，不碰系统
```

`rind status --scan` 会再扫一遍孤儿与可清理项；`--help` 有全部参数。

## `rind` 命令一览

```
查看      apps · leftovers · protected · history · status · analyze · launch list
卸载      uninstall <app>          [--dry-run] [--yes] [--all] [--keep-app] [--quit] [--force-quit]
孤儿      orphans                  [--trash] [--all] [--path P] [--dry-run] [--yes]
清理      clean                    [--trash] [--category C] [--project-root D] [--min-age N] [--dry-run] [--yes]
任意文件  trash <path>...          [--dry-run] [--yes]
放回      restore <id>
启动项    launch list|disable|enable|remove <label>...

全局：--json（给脚本和 agent 用）· --lang zh|en
```

完整说明与示例见 [docs/USAGE.md](docs/USAGE.md)。

## 规则库

`rules/*.toml` 为具体 App 补充清理路径（国内常用软件优先），编译时内嵌。规则只能**补充**证据，不能绕过保护区。
想加规则见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/USAGE.md](docs/USAGE.md) | 使用手册：安装、界面、全部命令、文件位置、常见问题 |
| [docs/DESIGN.md](docs/DESIGN.md) | 设计：目标与非目标、信息架构、交互与视觉、扩展点 |
| [docs/SAFETY.md](docs/SAFETY.md) | 安全模型：唯一出口、保护区、History、已知边界、怎么自己验证 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 技术架构：一次卸载怎么走、各模块契约 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑与候选计划 |
| [CHANGELOG.md](CHANGELOG.md) | 每个版本的改动 |

## 参与

- 贡献流程与「不可破坏的不变量」：[CONTRIBUTING.md](CONTRIBUTING.md)
- 安全问题请私下报：[SECURITY.md](SECURITY.md)
- 行为准则：[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)（Contributor Covenant 2.1）

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE)。

## 致谢

Peel、Mole、腾讯柠檬、Pearcleaner 把这条路走通过一遍，Rind 站在它们的经验上；
GPUI 与 [gpui-component](https://github.com/longbridge/gpui-component) 提供了界面层。
