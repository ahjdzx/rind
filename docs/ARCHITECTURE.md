# Rind 架构

Rind 是一个 macOS 卸载与清理工具：找出一个 App 散落在各处的残留文件，**说明每个文件为什么属于它、有多大把握**，然后把用户选中的部分**移入废纸篓**（永不直接删除），并能从 History 一键放回。

设计上吸收了四个项目的长处：

| 来源 | 吸收的做法 |
|---|---|
| Peel | 逐文件证据与置信度（certain / likely / possible）、硬编码保护区、共用文件不自动勾选、未知大小不当作 0、只进废纸篓 + History 恢复、移动前再校验一次 |
| Mole | CLI 优先、`--dry-run`、`--json`、history 子命令、对 AI agent 友好 |
| 腾讯柠檬 | 数据驱动的规则库（`rules/*.toml`），针对国内常用软件补充清理路径 |
| Pearcleaner | Finder/拖放式的交互目标（后续）、App Lipo（后续） |

## 工作区

```
rind/
├── crates/
│   ├── rind-core/   # 全部逻辑：扫描、匹配、度量、保护、废纸篓、历史。无 UI。
│   ├── rind-i18n/   # 把 core 的结构化结果翻成人话；zh-CN 默认，en 备用。
│   ├── rind-cli/    # `rind` 命令行（clap）。
│   └── rind-app/    # GPUI 桌面应用（gpui-kit = gpui-pre 0.3.11 + gpui-component 0.9）。
├── rules/           # 每个 App 一个 TOML 规则文件，编译时内嵌进 rind-core。
└── docs/
```

依赖方向：`rind-app` / `rind-cli` → `rind-i18n` → `rind-core`。core 不依赖 i18n，所有面向人的文案都在 i18n 或上层。

## 一次卸载怎么走

1. **发现 App**（`apps`）：扫描 `/Applications`、`~/Applications`，读 `Info.plist` 得到名称、bundle id、版本、可执行文件；收集内嵌 bundle（Frameworks / PlugIns / XPCServices / Helpers / LoginItems）的 id；用 `codesign` 取 Team ID 和 App Group；判断来源（App Store 收据 / Homebrew Caskroom / 其它）。Apple 自带的 App（`com.apple.*`，或在 `/System/Applications` 下）不列入可卸载。
2. **扫描候选**（`leftovers`）：在每个 `SearchLocation`（`~/Library/Application Support`、`Caches`、`Preferences`、`Containers`、`Group Containers`、`LaunchAgents`、`Saved Application State`、`Logs`、`HTTPStorages`、`WebKit`、`Cookies`、`Application Scripts`，以及 `/Library/{Application Support,Caches,Preferences,LaunchAgents,LaunchDaemons}`、`/private/var/db/receipts`、Homebrew Caskroom）列出条目作为候选。默认只看一层；在 `Application Support`、`Caches`、`Logs` 等位置，若一层目录是厂商目录（名字匹配 Team / bundle id 前缀 / 规则声明），再看一层。
3. **匹配**（`matcher`）：对每个候选给出 `Evidence { kind, confidence, matched }`：
   - 名字含完整 bundle id → `BundleId`，certain
   - 含内嵌组件的 id → `EmbeddedBundleId`，certain（组件属于该 App 或同厂）/ likely
   - 含 App Group id → `AppGroup`，certain
   - launchd plist 的 `Program`/`ProgramArguments[0]` 指向 App 包内 → `LaunchdProgram`，certain
   - 规则库命中 → `Rule`，按规则声明
   - Homebrew cask 的 `zap`/`uninstall` 段的 `trash`/`delete` 命中 → `CaskZap`：路径名含本 App 的 bundle id / 名称时 likely，
     否则（`Google`、`JetBrains` 这类厂商目录）只是 possible。`rmdir` 段只在目录已空时才算；cask 路径只认 `~/Library`、
     `/Library` 和 Caskroom 下的，`~/Documents`、`~/Music` 之类一律不收。cask 数据来自用户可写的缓存，所以它只是线索，
     从不单独构成 certain。
   - 只有 bundle id 前缀（如 `com.foo.` 下的另一个 id）→ `BundleIdPrefix`，likely
   - 只有名称完整相等（归一化后）→ `Name`，likely；名称作为整词出现 → possible
   - `com.apple.*` 与系统目录（`SystemFolders`）对任何非 Apple App 都不匹配。
   同一个候选再拿**其它已安装 App** 跑一遍：谁也拿到 likely 以上，就记为 `rivals`（共用），不会被自动勾选。
   候选是目录时也看它的直接子项：有子项被别的 App 认领，整个目录就算共用。同一 bundle id 的另一份拷贝也算 rival。
   兄弟 App 的 id（`com.foo.app.ce`、`com.google.Chrome.canary`）不算本 App 的 certain，只是 likely。
4. **度量**（`size`）：每个匹配项用线程池按时间预算统计 **实际占用块数**（`st_blocks * 512`，APFS 克隆/稀疏文件按真实占用），超时、无权限读 → `Size::Unknown`，界面显示"未知"，绝不显示 0，也不自动勾选。
5. **计划**（`uninstall::plan`）：`UninstallPlan` 列出 App 本体和全部匹配项。`LeftoverMatch::suggested()` 仅当：confidence ≥ likely、无 rivals、无 hold_back、大小已知。
6. **保护**（`protect`）：匹配到也不能动的东西记为 `hold_back`：iCloud Drive、钥匙串、`~/.ssh`、`~/.gnupg`、Mail、Messages、Safari、照片/音乐库、iPhone 备份、Desktop/Documents/Downloads 本身、家目录和 Library 本身、系统路径、带钱包扩展的浏览器 profile、`~/.Trash`、Rind 自己。**任何选择都不能覆盖**。
   判断按**盘上的真实身份**而不是字符串：路径先 `canonicalize`（APFS 不区分大小写，`~/library/keychains` 和 `~/Library/Keychains`
   是同一处），叶子是符号链接时不跟随叶子、只解析父目录；相对路径、含 `..`、解析不了的一律按保护处理（`Unresolvable`）。
   钱包检测找不到定论时也按有钱包处理（fail closed）。root 所有的文件，或父目录带 sticky 位且不属于当前用户的
   （如 `/Library/LaunchDaemons`、`/Library/Caches`），在 MVP 里记为 `NeedsAdmin`，列出但不移动。
7. **移动**（`uninstall::execute` → `trash`）：
   - 冻结 `Selection`，只移动计划里有的路径。
   - 不是预演就必须有 History 可写（`LedgerRequired`），否则一个都不动。
   - 只要会移动东西、App 又在运行，就拒绝（`AppRunning`），由上层提示退出/强制退出；预演不受影响。
   - 每一项在 `trashItem` **之前紧挨着**再跑一次 `protect::check`，并移动它解析出来的真实路径。
   - 先移 App 本体，再移残留；App 本体移动失败则残留一个都不动。
   - 用 `NSFileManager.trashItem(at:resultingItemURL:)` 得到废纸篓里的实际路径。
   - 先写 History 记录（`history::Ledger`，JSON Lines，`~/Library/Application Support/Rind/history.jsonl`）：每一项移动前先记
     `pending` 的意图，移动后回填它在废纸篓里的路径。写不了记录就停下，并把已经发生的事随 `ExecuteError::Ledger { partial }`
     一起返回。文件用 `flock` 加锁，重写用唯一临时文件 + `fsync` + rename。
   - `dry_run` 时只返回将做什么。
8. **恢复**（`history::restore`）：记录文件是用户可写的，所以每一项先校验——`trashed` 真的在废纸篓里、`original` 是绝对路径、
   不在保护区、父目录解析后没逃出家目录或 `/Library`、`/Applications`；再用 `renamex_np(RENAME_EXCL)` 放回（原位置已被占用就跳过并说明）；
   跨卷时复制并核对后才删源。逐项记录 `restored`，全部放回后记录才算 restored。

## 线程与界面

core 是同步 API + rayon；CLI 直接调用；GPUI 应用在 `background_executor` 里跑扫描，用 `ScanProgress` 回调刷新界面。core 的类型都 `Send + Sync + Clone + Serialize`，CLI 的 `--json` 和 GUI 都直接用。

## 测试

core 的所有路径都来自 `Env`（可注入的家目录等），测试用 `tempfile` 搭一个假的 `~/Library` 跑完整流程，不碰真实系统。`cargo test --workspace --exclude rind-app` 在 CI 跑；GPUI 应用需要本机窗口，手动验证。

## 规则文件

```toml
# rules/com.tencent.xinWeChat.toml
bundle_id = "com.tencent.xinWeChat"
name = "微信"
aliases = ["WeChat"]

[[paths]]
path = "~/Library/Containers/com.tencent.xinWeChat"
confidence = "certain"
note = "沙盒容器，含聊天记录与缓存"

[[paths]]
path = "~/Library/Group Containers/*.com.tencent.xinWeChat"
confidence = "certain"
```

路径支持 `~` 和 glob。规则只**补充**证据，不能绕过保护区。
