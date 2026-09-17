//! 交叉编译检查用的壳子 —— 用来验证 Linux 专属代码能不能编过。
//!
//! # 为什么需要它
//!
//! 开发机是 Windows，`cargo build` 编译不到 `fuse_fs.rs`（`fuser` 是 Unix-only）。
//! 也就是说这个项目里最难、最容易写错的一段代码，在开发机上**完全得不到编译器的
//! 保护**——API 名字写错、方法签名对不上、缺 `Send + Sync`，全都要等到拷到
//! 飞牛上 `cargo build` 才会暴露。
//!
//! 这里用 `#[path]` 直接指向项目的**真实源文件**（不是副本，所以永远不会过期），
//! 再让它对着 Linux target 编一遍。`cargo check` 不做链接，因此不需要交叉链接器；
//! 依赖也刻意只留纯 Rust 的，这样 `rustup target add` 之后就真的能跑起来。
//!
//! # 用法
//!
//! ```text
//! rustup target add aarch64-unknown-linux-gnu   # 只需一次
//! cd tools/linuxcheck
//! cargo check --target aarch64-unknown-linux-gnu
//! ```
//!
//! 用 aarch64 是因为飞牛 fnOS 就是这个架构：既检查了「Unix 平台能不能编」，
//! 又顺带确认了目标架构上没有问题。
//!
//! # 注意
//!
//! 它只能覆盖「平台无关层 + fuse_fs」这一组。`netease` / `fetch` / `tag`
//! 因为依赖 ring 没法参与交叉检查，但它们本身是跨平台的，日常 `cargo build`
//! 就能覆盖到。两边合起来，项目里所有代码都在某个平台上被真正编译过。

#[path = "../../../src/model.rs"]
pub mod model;

#[path = "../../../src/config.rs"]
pub mod config;

#[path = "../../../src/naming.rs"]
pub mod naming;

#[path = "../../../src/store.rs"]
pub mod store;

#[path = "../../../src/vfs.rs"]
pub mod vfs;

#[path = "../../../src/fuse_fs.rs"]
pub mod fuse_fs;
