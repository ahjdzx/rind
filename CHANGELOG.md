# 更新日志

本文件记录 Rind 每个版本的改动，格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

## [0.2.1] - 2026-11-10

### 界面

- 应用有了自己的图标：`Rind.app` 里带上 `AppIcon.icns`（黄底圆角方格 + R，按 macOS 图标网格留边、带投影），
  Dock、Finder、启动台不再是空白图标。打包脚本用 `sips` + `iconutil` 从 `assets/AppIcon-1024.png` 生成。
- 侧栏顶部的标志换成同一个图标（原来是通用的 Lucide 图标）；设置页底部新增「关于」：图标、版本号和一句介绍。

### 文档

- 新增英文文档 [docs/en/USAGE.md](docs/en/USAGE.md)、[docs/en/SAFETY.md](docs/en/SAFETY.md)、
  [docs/en/DESIGN.md](docs/en/DESIGN.md)；英文 README 的文档索引改指英文版
  （架构与路线图仍为中文，已在表中标注）。
- 三组文档（使用手册 / 安全模型 / 设计文档）的中英文版本顶部互相加了语言切换链接。

### 其他

- 新增品牌图：`docs/images/icon-{256,512}.png`（列表与头像用）、
  `docs/images/social-preview.png`（1280×640，GitHub 社交预览用）；生成脚本
  `scripts/make-brand-images.py`；两个 README 顶部加了应用图标。

## [0.2.0] - 2026-11-09

### 新增

- 深色模式完整支持：外观可选「跟随系统 / 浅色 / 深色」，选择会记住（`<data-dir>/appearance`），
  下次启动沿用；强制浅色 / 深色时，原生窗口边框与标题栏一起切换。
- `RIND_APPEARANCE=system|light|dark` 环境变量：启动时指定外观，优先于记住的选择；示例模式也认。
- 语言选择会记住：设置页选中 / 英后落盘（`<data-dir>/language`），下次启动沿用；
  `RIND_LANG` 显式指定时优先级更高。

### 打包与安装

- 每个 tag（`v*`）由 GitHub Actions 构建并把 `Rind-v<版本>.dmg` 挂到 Release
  （`.github/workflows/release.yml`，手动触发时只出构建产物）；本地打包用 `scripts/package-dmg.sh`。
- DMG 里是 `Rind.app` 与 `/Applications` 快捷方式，ad-hoc 签名；没有 Developer ID 与公证，
  第一次打开需要右键 →「打开」。目前只有 Apple Silicon（arm64）产物。

### 文档

- 英文 README 成为仓库首页（[README.md](README.md)），中文版移到
  [README.zh-CN.md](README.zh-CN.md)，两版互相链接。
- 新增界面截图与动图预览，按语言分目录：英文版配英文界面截图（`docs/images/en/`），
  中文版配中文截图（`docs/images/zh/`），各自带一份 `demo.gif`。
- 示例数据的清理批次标题跟随界面语言（和真实 GUI 写进 History 的文案一致），
  英文界面的历史页不再出现中文条目名。

## 计划中

见 [docs/ROADMAP.md](docs/ROADMAP.md)。当前候选：Developer ID 签名与公证、Homebrew cask、
universal（arm64 + Intel）构建、首次运行的「完全磁盘访问」引导、规则库扩充。

## [0.1.0] - 2026-11-09

第一个可用版本。没有预编译产物，从源码构建。

### 新增

- **卸载主线**：App 列表 → 残留扫描（bundle id、内嵌组件、App Group、签名 Team、名称打分、Homebrew cask 的
  `zap` 段、pkg 回执）→ 每个文件给出理由与置信度（certain / likely / possible）→ 移入废纸篓 → History 一键放回。
- **孤儿残留**：已删除 App 留下的文件；Spotlight、已装 App、插件包与规则库共同认领，找不到主人的才列。
- **缓存与开发产物清理**：应用缓存、日志、浏览器缓存（只碰缓存目录）、Xcode、包管理器缓存、项目构建产物；
  构建产物要有旁证文件、项目久未改动才建议；重建代价高的只列不勾。
- **空间分析**：按实际占用统计（APFS clone 与硬链接去重）、目录树、大文件清单，可中途取消。
- **启动项**：列出 `~/Library/LaunchAgents`、`/Library/LaunchAgents`、`/Library/LaunchDaemons`；
  只有当前用户域能停用 / 启用 / 移除。
- **桌面应用**（GPUI）：总览首页（磁盘容量、智能扫描）、App、孤儿、清理、空间、启动项、History、设置；
  原生侧栏；中英文界面；示例模式 `RIND_DEMO=1`。
- **命令行 `rind`**：`apps`、`leftovers`、`uninstall`、`history`、`restore`、`protected`、`status`、`orphans`、
  `clean`、`analyze`、`trash`、`launch`；全部支持 `--json`，会移动的命令支持 `--dry-run`。
- **规则库**：`rules/*.toml` 为具体 App 补充清理路径（国内常用软件优先），编译时内嵌，只能补充证据。
- **双语文案**：`rind-i18n` 覆盖全部枚举，中英文都有测试兜底。

### 安全

- 所有删除只到废纸篓，History 记录它在废纸篓里的实际位置，可逐项放回；没有记录的东西不会进废纸篓。
- 保护区（iCloud Drive、钥匙串、SSH / GPG 密钥、Mail、Messages、Safari、照片与音乐库、iPhone 备份、
  桌面 / 文稿 / 下载等文件夹本身、系统路径、含加密钱包扩展的浏览器 profile）任何选择都不能覆盖，
  `rind protected` 可查。
- 唯一的移动出口 `remove::trash_items`：移动前把路径解析成盘上的真实身份，与扫描时不一致就拒绝。
- 外层目录里有不能动的东西（受保护 / macOS 自己的 / 要管理员 / 在用）时，整个外层拒绝移走。
- 启动项：`launchctl` 动手前用 `O_NOFOLLOW` 重新核对 plist（属主、权限、大小、非深嵌套炸弹、`Label`、程序），
  并记下设备 + inode + 大小 + mtime / ctime + 内容哈希，在 `bootout` 前后与 `rename` 之前各比一次，变了就装回去。
- `--dry-run` 与真跑逐项一致：会移走的、会拒绝的、能释放多少，两边对得上。

### 已知限制

- 只支持 macOS；本机在 macOS 26 / Apple Silicon 上验证，更老的系统与 Intel 机器未验证。
- 需要管理员权限（`/Library` 下、系统域启动项等）的项只列出，不搬运。
- 没有「完全磁盘访问」时读不了沙盒容器，界面会标成「读不了」，不会显示为 0。
- 目录里的媒体库 / 钱包探测往下 6 层、最多 20 万个条目；更深处的东西不会拦，见 [docs/SAFETY.md](docs/SAFETY.md)。
- 没有预编译产物，也没有更新器：得自己 `git pull` 后重新构建。

[未发布]: https://github.com/ahjdzx/rind/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/ahjdzx/rind/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ahjdzx/rind/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ahjdzx/rind/releases/tag/v0.1.0
