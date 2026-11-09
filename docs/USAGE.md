# Rind 使用手册

面向使用者：怎么装、怎么用、每个命令做什么、东西放在哪、出问题怎么办。
想了解它为什么这么设计，看 [DESIGN.md](DESIGN.md)；想知道它保证什么、不保证什么，看 [SAFETY.md](SAFETY.md)。

## 1. 系统要求

- macOS。开发与验证都在 macOS 26 (Tahoe) / Apple Silicon 上完成，更老的系统与 Intel 机器没有验证过。
- 运行不需要完整 Xcode（界面用运行时编译的 Metal shader）。
- 从源码构建需要 Rust **1.85+**（本机用 stable 1.99 验证）。

## 2. 安装

目前**没有预编译产物**，也没有 Homebrew / cask，只有一个办法：自己构建。

```sh
git clone git@github.com:ahjdzx/rind.git
cd rind

cargo build --release -p rind-cli      # 命令行，产物在 target/release/rind
cargo build --release -p rind-app      # 桌面应用，产物在 target/release/rind-app
```

想让它出现在「应用程序」里，把 `rind-app` 拷进 `Rind.app/Contents/MacOS/` 也行，但仓库里没有做打包脚本，
也没有签名与公证，第一次打开可能需要右键 →「打开」。这两个都在 [ROADMAP](ROADMAP.md) 里。

## 3. 第一次打开

桌面应用启动后停在**总览**页：

1. 最上面一条是磁盘容量条；下面是「智能扫描」按钮——它会扫一遍孤儿残留和可清理的东西，算出能释放多少。
2. 左边侧栏是全部页面：**总览 / App / 孤儿 / 清理 / 空间 / 启动项**，底部是 **History / 设置**。
3. 想先看看它怎么判断，用示例模式打开，里面的数据是假的，不碰系统：

   ```sh
   RIND_DEMO=1 cargo run -p rind-app
   ```

命令行那边，建议先跑只读命令：

```sh
rind status            # 磁盘容量 + 可卸载的 App 数（加 --scan 再汇总孤儿与可清理空间）
rind apps              # 列出已安装的 App（--all 连 macOS 自带的也列）
rind leftovers 微信     # 微信留下了什么、每个文件为什么算它的（只看不动）
rind protected         # Rind 永远不动的地方
```

## 4. 桌面应用

| 页面 | 做什么 | 要点 |
| --- | --- | --- |
| **总览** | 磁盘容量、智能扫描、各页入口 | 扫描可以中途取消；结果只读，不会自动勾选 |
| **App** | 卸载 App 与它的残留 | 逐个文件给出理由与置信度；保护区里的项不显示勾选框 |
| **孤儿** | 已删除 App 留下的残留 | 默认只勾「确定主人不在了」的那部分；Spotlight 不可用时会降级为「只列不勾」 |
| **清理** | 缓存、日志、包管理器缓存、项目构建产物 | 按类别分组；重建代价高的只列不勾；项目太久没动才建议清构建产物 |
| **空间** | 哪个目录最占地方、大文件在哪 | 只看不动；里面带媒体库 / 钱包的目录不可选 |
| **启动项** | 登录启动项与后台任务 | 只有当前用户自己的项能停用 / 启用 / 移除 |
| **History** | Rind 移走过什么 | 一次一批，可整批或逐项放回 |
| **设置** | 语言、清理偏好、保护清单 | 保护清单是只读的，列出来的东西任何设置都不能覆盖 |

任何一次移动，确认之前都会先列出「会移走什么、能释放多少、哪些不动」，确认后才动手；
动完会告诉你放回了哪里（废纸篓），并给一个 `rind restore <id>` 的提示。

## 5. 命令行

所有命令都接受两个全局参数：

```
--json          给脚本和 AI agent 用的 JSON 输出
--lang zh|en    语言（默认看 RIND_LANG，再看 LANG）
```

### 5.1 查看

| 命令 | 说明 |
| --- | --- |
| `rind apps [--all]` | 列出已安装的 App |
| `rind leftovers <APP> [--no-size]` | 一个 App 留下了什么，以及每个文件的理由与置信度（只看） |
| `rind protected` | 永远不动的地方 |
| `rind history` | Rind 移走过什么 |
| `rind status [--scan]` | 总览；`--scan` 会再扫一遍孤儿与可清理项（慢一些） |
| `rind analyze [PATH] [--depth N] [--top N] [--large-min SIZE]` | 目录占用与大文件清单（只看） |
| `rind launch list` | 全部启动项 |

`<APP>` 可以是名字、bundle id 或路径，不分大小写，可以只给一部分。有人看着（交互模式）时，
模糊的名字会列出所有匹配让你确认；没人看着（`--yes` / `--json`）时只接受完整匹配。

### 5.2 卸载

```sh
rind uninstall 微信                 # 先列出计划，再问一次
rind uninstall 微信 --dry-run       # 只说会做什么，什么都不动
rind uninstall 微信 --yes           # 不问
rind uninstall 微信 --keep-app      # 只移残留，App 本体留下
rind uninstall 微信 --quit          # App 在运行就先让它退出（等 5 秒）
rind uninstall 微信 --force-quit    # 不肯退出就强制退出
rind uninstall 微信 --all           # 连 Rind 没替你选的也一起（保护区仍然不动）
```

### 5.3 清理

```sh
rind orphans                       # 孤儿残留（只看）
rind orphans --trash --dry-run     # 预演：会移走哪些、拒绝哪些、释放多少
rind orphans --trash --yes         # 真清（默认只清 Rind 建议的那些）
rind orphans --trash --path ~/Library/Caches/com.foo --yes   # 只清指定路径

rind clean                         # 缓存 / 日志 / 构建产物（只看）
rind clean --category dev_artifacts --min-age 60
rind clean --trash --dry-run
rind clean --trash --yes
```

`--category` 可选：`app_caches`、`logs`、`browser_caches`、`xcode`、`package_caches`、`dev_artifacts`、`needs_review`。
`--project-root` 指定去哪找项目（默认 `~/work`、`~/Projects`、`~/Developer` 等），`--min-age` 是「多少天没动过才建议清」。

### 5.4 移走任意文件

```sh
rind trash ./build --dry-run       # 预演
rind trash ./build ~/old-notes --yes
```

保护区里的路径、目录里的照片 / 音乐库与带钱包的浏览器 profile 会被拒绝（预演和真跑一致）。
注意：显式点名一个目录时，Rind 只兜底媒体库与钱包，目录里「有人在用 / 要管理员」的项不会拦——
搬走的是这个目录，它照样能整体放回。

### 5.5 放回

```sh
rind history                       # 找到那次的 id（可只给开头几位）
rind restore 3f9a                  # 放回原处
```

放回是一批一批的：某一项已经不在废纸篓、或原处已经有别的文件，它会被跳过并说明原因，其余照常放回。

### 5.6 启动项

```sh
rind launch list
rind launch disable com.example.agent        # 停用（可逆，不动文件）
rind launch enable  com.example.agent        # 重新启用
rind launch remove  com.example.agent --dry-run
rind launch remove  com.example.agent --yes  # 先 bootout，再把 plist 移进废纸篓
```

label 要写完整。`/Library` 下和 `com.apple.*` 的项列出来但不可操作。

## 6. 给脚本和 AI agent

- **先预演**：所有会移动东西的命令都有 `--dry-run`，`--json` 的输出里 `moved` 是会移走的、`refused` 是拒绝的、
  `freed` 是能释放的字节数。预演和真跑给的是同一份判断。
- **没人看着时用完整名字**：`--yes` / `--json` 下模糊匹配不会替你猜。
- **`rind status --scan --json`** 适合当入口：一次拿到磁盘、App 数、孤儿与可清理空间的汇总。
- **不要用 `--all` 代替判断**：`--all` 会把「可能 / 共用 / 读不了」的也带上，那是给人看的兜底，不是默认值。
- 退出码：`0` 成功（包括「没有能移动的」这种空结果会被说明），非 0 表示出错了，原因在 stderr。

## 7. 文件放在哪

| 位置 | 是什么 |
| --- | --- |
| `~/Library/Application Support/Rind/history.jsonl` | History：每一次移动的清单（一行一条记录） |
| `~/Library/Application Support/Rind/` | App 图标缓存等 |
| 废纸篓（`~/.Trash`，外置卷是 `<卷>/.Trashes/<uid>`） | 被移走的东西真正待在哪儿 |
| `rules/*.toml`（源码里，编译时内嵌） | 具体 App 的清理规则 |

Rind 没有账号、没有遥测、没有联网请求；它唯一的网络行为是你自己 `git pull`。

## 8. 环境变量

| 变量 | 作用 |
| --- | --- |
| `RIND_LANG` | 界面与命令输出的语言（`zh` / `en`），优先于 `LANG` |
| `RIND_DEMO=1` | 桌面应用用示例数据启动，不碰系统 |
| `RIND_PAGE=space` | 开发辅助：启动时直接打开某一页 |
| `RIND_AUTOSCAN=1` | 开发辅助：启动即跑智能扫描 |
| `RIND_HOME` + `RIND_ROOT` | 把家目录和系统根指到别处（集成测试用），两个都给才生效 |

## 9. 常见问题

**为什么有些项没有预先勾上？**
只有「确定 / 很可能属于这个 App」、没有别的 App 共用、大小已知的才会预先勾上。拿不准的列出来让你自己判断；
需要管理员权限的只列不搬。

**为什么显示「读不了」？**
多半是 macOS 的隐私保护（TCC）挡住了沙盒容器之类的位置。给终端或 Rind 授予「完全磁盘访问」后重新扫描即可。
Rind 不会把读不了当成 0 字节。

**为什么 App 在运行时不让我卸载？**
用 `--quit`（等 5 秒）或 `--force-quit`，或在界面里先退出它。运行中卸载可能让 App 自己再写回一堆文件。

**「能释放 12 GB」是真的吗？**
那是按实际占用（APFS clone 与硬链接去重后）算的，但只有在**清空废纸篓之后**磁盘空间才真的回来。

**怎么确认它到底干了什么？**
`rind history --json` 有每一次移动的完整清单（含废纸篓里的实际路径）。想先看看再说，就一律加 `--dry-run`。

**会不会删掉我的聊天记录 / 照片？**
保护区里的东西任何选择都不能覆盖；照片库、音乐库、钱包 profile 这类埋在目录深处的也会被拦（往下 6 层）。
但请记住：Rind 只把它们移进废纸篓，清空废纸篓才是真正的删除——那之后谁也救不回来。

**怎么把 Rind 自己删干净？**
`rm -rf` 别用在这儿；用 Rind 自己：

```sh
rind uninstall Rind --yes       # 如果它在 /Applications 里
rind trash ~/Library/Application\ Support/Rind --yes   # History 与缓存
```

## 10. 出问题时

- 先看 `rind history`：它记录了 Rind 做过什么，放回也从这里来。
- 安全问题（能绕过保护区、把不该动的移走）请按 [SECURITY.md](../SECURITY.md) 私下报，不要开公开 issue。
- 其他问题：开 issue 时带上 macOS 版本、芯片、`rind --version`、复现步骤，以及（脱敏后的）`rind status --json`。
