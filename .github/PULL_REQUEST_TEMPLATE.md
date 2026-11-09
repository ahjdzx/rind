## 改了什么

<!-- 一两句说清。第一行请说明「为什么」，不是「改了什么文件」。 -->

## 怎么验证的

<!-- 贴真实命令与结果，不要只写「已测试」。界面改动请说清手动看了哪一页。 -->

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude rind-app --all-targets -- -D warnings
cargo test --workspace --exclude rind-app
```

## 检查清单

- [ ] 移动文件走的是唯一出口 `remove::trash_items`（没有新开路径）
- [ ] `--dry-run` 与真跑判断一致（`moved` / `refused` / `freed`）
- [ ] 拿不准的情况是「拒绝 / 不勾选」，不是「当成安全」
- [ ] 新文案有中英两份，并在 `rind-i18n` 有测试
- [ ] 测试不碰真实系统（路径来自 `Env::for_test`，`launchctl` 用注入的假实现）
- [ ] 行为变化更新了 `CHANGELOG.md`；设计取舍写进了 `docs/`

## 涉及这些文件请单独说明

<!-- 保护区 (protect.rs)、移动出口 (remove.rs)、History (history.rs)、启动项 (launch.rs) -->
