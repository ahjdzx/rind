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
   （如 `/Library/LaunchDaemons`、`/Library/Caches`），记为 `NeedsAdmin`，列出但不移动（Rind 不申请提权，见 ROADMAP「明确不做」）。
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

## 统一的移动出口（`remove`）

卸载、孤儿、清理、空间分析、启动项**只有一个**把东西移进废纸篓的地方：`remove::trash_items`（卸载用同一个 `Mover`，只是多了"App 先走"的顺序）。
它保证第 7 步的全部规则：只认扫描结果里有的路径（`NotInPlan`）、真要动必须有 History（`LedgerRequired`）、每项移动前紧挨着
`resolve` + `protect::check`、先记 `pending` 意图再移动、写不了 History 就停并带回 `partial`、一批一条记录、什么都没移成不留记录。
同一批里一个路径在另一个下面时只移外层的。History 记录带 `kind`（`RecordKind`：卸载 / 孤儿 / 清理 / 文件 / 启动项），旧记录按卸载读。

`HoldBack::InUse { app }`：所属程序正在运行（比如开着的浏览器的缓存），不能选，退出后重新扫描。

## 孤儿（`orphans`）

App 已经删了、数据还在。候选与卸载用同一批搜索位置；"主人"全集 = 已安装 App + Apple 自带 App + Spotlight 能找到的所有 App 包
（含内嵌组件 id、App Group）。任何主人能拿到证据的候选都不是孤儿。剩下的只按强信号报告（`OrphanSignal`）：

- `BundleIdGone`：名字是 bundle id、没有主人用它。厂商前缀也没人用 → likely；同厂商还有 App → possible。
- `ProgramMissing`：launchd plist 要运行的程序不在了、label 不属于任何主人 → likely。
- `KnownAppGone`：规则库点名的 App 没装 → likely；Homebrew cask 点名 → possible（cask 数据只是线索）。
- 普通名字的陌生文件夹**不报**：误报比漏报糟糕得多。

`com.apple.*`、系统目录、保护区一律不报；按推测的主人分组，勾选规则沿用 `LeftoverMatch::suggested()`。

## 清理（`cleanup`）

按 `CleanupCategory` 分类：应用缓存、日志、浏览器缓存（只碰缓存目录，不碰 profile 数据 / Cookies / Safari）、Xcode、包管理器缓存、
项目构建产物、需要人判断的（Xcode 归档、iOS 设备支持文件、重新下载代价高的缓存）。构建产物必须有旁证文件（`target` 旁边有
`Cargo.toml`、`node_modules` 旁边有 `package.json`……），项目超过 N 天没动过才建议。"需要人判断的"从不建议。所属程序在运行 → `InUse`。

## 空间（`space`）

对一个目录并行统计实际占用（`st_blocks`），不跟随符号链接、不跨卷、硬链接只算一次；树只留到 `max_depth` 层、每层前 `max_children`
个，其余折叠成 `Other`，往深处看就对子目录再分析一次。同时收集最大的 N 个大文件。超时或取消时返回**部分结果**并标 `incomplete`，
读不了的目录列在 `unreadable`、相关节点标 `partial`。根先解析成盘上的真实路径（符号链接、大小写都还原），输出里每个 `path` 都是
真实路径——移动出口拿它再解析一次，两边必须完全相等。分析 `/` 时把数据卷（firmlink、`/System/Volumes/Data`）当同一个卷，跳过
`/System/Volumes/Data` 本身，别的卷不进。移动一个目录前紧挨着再做一次「子孙探测」：往下 6 层、最多 20 万个条目（广度优先，
不跟符号链接），看到照片 / 音乐库或带钱包的浏览器 profile 就整个拒绝；额度用完时放行（浅处的库一定看过，超限的几乎都是
`node_modules` / `target` 这类）。符号链接只是链接：移走它不会带走它指向的东西，所以保护区里的**裸链接**本身可以移。每个节点都有 `hold_back`，受保护的不能选；**子树里有不能选的东西时，它的每个祖先
（含根）都标 `Contains { path, why }`**，`path` 指到最近的那个真正挡路者，界面据此禁止勾选整个外层。树里看不到的部分（超过
`max_depth`、并进 `Other` 的）由走树时的便宜检查补上（照片 / 音乐库、固定保护位置、`com.apple.*`、带钱包的浏览器 profile）。
没走完就要停（超时 / 取消）时，没来得及核实的节点按 `Protected { Unresolvable }` 处理——宁可多拦，不让没核实过的东西变成可选。

## 启动项（`launch`）

列出 `~/Library/LaunchAgents`、`/Library/LaunchAgents`、`/Library/LaunchDaemons`：label、程序、是否存在、RunAtLoad / KeepAlive、
所属 App、是否孤儿、运行状态（`/bin/launchctl` 批量查询）。只有当前用户域的项能操作：停用/启用用 `launchctl disable|enable`
（可逆、不动文件）；移除 = `bootout` 后把 plist 走 `remove`，**卸下来了却没移走（被拒 / 失败 / History 出错）的重新 `bootstrap`
装回去**，装不回去就在原因里注明 `UNLOADED_NOT_MOVED`。`/Library` 下的是 `NeedsAdmin`，`com.apple.*` 是 `SystemOwned`。
不收符号链接的 plist；`~/Library/LaunchAgents` 本身或它到家目录之间哪一级是符号链接时整个用户域都不列（免得指向
`/Library/LaunchAgents` 的链接让全局项冒充成可管理的）。label 按原样读，只有 `[A-Za-z0-9._-]` 组成的才可管理（它会拼进
`gui/<uid>/<label>`）。动手前（停用/启用、移除的 `bootout` 之前）用 `O_NOFOLLOW` 重新打开 plist 核对：普通文件、≤ 1 MB、
属主是当前用户、组和其他人不可写、不是深嵌套的炸弹（与 Info.plist 共用 `apps::plist_is_tame`）、盘上的 `Label` 与扫描时相同、
程序也没换；任何一点不符就报错，不调用 launchctl。核对时记下这份文件的身份（设备 + inode + 大小 + mtime / ctime + 内容哈希），
调用 launchctl 前、`bootout` 之后、以及真正 `rename` 之前各比一次，变了就不移动（`CHANGED_AT_MOVE`）并把卸下来的装回去。程序在不在：
读不了（EACCES / EPERM）的按在，`/Volumes/...` 下的按在。launchctl 抽象成可注入的接口，测试从不调用真实的 launchctl。

## 磁盘（`disk`）

`disk::usage(path)` 用 `statfs` 取所在卷的总量与可用量，给总览页的容量条。

## 线程与界面

core 是同步 API + rayon；CLI 直接调用；GPUI 应用在 `background_executor` 里跑扫描，用 `ScanProgress` 回调刷新界面。core 的类型都 `Send + Sync + Clone + Serialize`，CLI 的 `--json` 和 GUI 都直接用。

## 预演与真跑一致（`--dry-run`）

预演不是"只看扫描结果"：它和真跑一样解析真实路径、一样做子孙探测，外层被拒时内层同样重新单独排一遍（`nest` 之后），
所以 `moved` 和 `freed` 和真跑对得上。
`--dry-run --json` 里 `moved` 会移动的、`refused` 会拒绝的，真跑给出的就是同一份——给 agent 按预演做决定用。
唯一的差别是预演不写 History、不碰 launchctl、不动任何文件。

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
