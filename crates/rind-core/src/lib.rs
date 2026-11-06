//! Rind 的全部逻辑：找 App、找残留、说明理由、度量大小、挡住保护区、移入废纸篓、放回来。
//! 没有界面，没有文案；面向人的话都在 `rind-i18n` 和上层。
//!
//! 模块之间的契约见 `types.rs` 和 docs/ARCHITECTURE.md。

pub mod apps;
pub mod env;
pub mod history;
pub mod leftovers;
pub mod matcher;
pub mod protect;
pub mod rules;
pub mod running;
pub mod size;
pub mod trash;
pub mod types;
pub mod uninstall;

pub use env::Env;
pub use types::*;
