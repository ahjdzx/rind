# Rind

卸载 Mac 上的 App，连同它留下的每个文件，并**说明每个文件为什么属于它、有多大把握**。
只移入废纸篓，从不直接删除；History 一键放回。Rust 编写，界面基于 GPUI。

吸收了 [Peel](https://github.com/TuguiDragos/Peel)（逐文件证据与保护区）、[Mole](https://github.com/tw93/Mole)（CLI 优先、`--dry-run`）、
[腾讯柠檬](https://github.com/Tencent/lemon-cleaner)（数据驱动的规则库）和
[Pearcleaner](https://github.com/alienator88/Pearcleaner) 的长处。设计见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 安全原则

- **只进废纸篓。** 每一项都用 `NSFileManager.trashItem` 移动，History 记录它在废纸篓里的实际位置，随时放回。
- **保护区任何选择都不能覆盖。** iCloud Drive、钥匙串、SSH/GPG 密钥、Mail、Messages、Safari、照片和音乐库、iPhone 备份、
  桌面/文稿/下载等文件夹本身、系统路径、带加密钱包扩展的浏览器 profile。`rind protected` 列出全部。
- **不替你选拿不准的。** 只有 certain / likely、没有别的 App 共用、大小已知的项才会被预先勾上；
  读不了的目录显示"未知"，绝不显示 0。
- **移动前再查一次。** 每一项在移动前都重新过一遍保护区检查。
- **先记账，再移动。** 没有写进 History 的东西不会进废纸篓。

## 构建

需要 Rust 1.85+。不需要完整 Xcode（GPUI 用运行时编译的 Metal shader）。

```bash
cargo build --release -p rind-cli      # 命令行 `rind`
cargo build --release -p rind-app      # 桌面应用
cargo test --workspace --exclude rind-app
```

## `rind` 命令

```
rind apps                      列出已安装的 App
rind leftovers <app>           看一个 App 留下了什么，以及每个文件为什么属于它（只看不动）
rind uninstall <app>           移到废纸篓；先列出计划，再问一次
    --dry-run                  只说会做什么
    --yes                      不问
    --all                      连没有预先勾上的也一起（保护区仍然不动）
    --keep-app                 只移残留
    --quit / --force-quit      App 在运行时先退出它
rind history                   Rind 移走过什么
rind restore <id>              放回去
rind protected                 Rind 永远不动的地方
--json                         给脚本和 AI agent 用
--lang zh|en
```

## 规则库

`rules/*.toml` 为具体 App 补充清理路径（国内常用软件优先），编译时内嵌。规则只能**补充**证据，不能绕过保护区。

## 许可证

GPL-3.0-or-later。
