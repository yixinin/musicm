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
//! # 覆盖范围（以及为什么必须守住它）
//!
//! 这里列进来的模块 = 「Windows 编不到、或只有 Linux 那一支编得到」的东西：
//! `fuse_fs`（`fuser` 是 Unix-only）和 `mount`（内部按平台分叉）。
//! 其余模块依赖 `ring`，没法参与交叉编译，但它们本身跨平台，日常 `cargo build`
//! 就覆盖了。两边合起来，项目里所有代码都在某个平台上被真正编译过。
//!
//! 这个不变式很容易被破坏，代价也很大——`mount` 就是因此补进来的：它原来长在
//! `main.rs` 里，用 `#[cfg]` 分成两个 `mount_now`，结果 Windows 看不到 Linux 那一支、
//! 这里也没引用它，于是 `println!("已卸载 {mountpoint}")`（`PathBuf` 没实现 `Display`）
//! 在开发机上 21 个测试全绿、零警告，一直漏到 NAS 上编译才炸。
//!
//! 所以规矩是：**新增平台专属代码时，先把它放进一个「总是被编译的叶子模块」，
//! 再把那个模块加到下面这个列表里。**

#[path = "../../../src/model.rs"]
pub mod model;

// 纯 std + anyhow，没有 C 依赖，所以可以（也应该）参与交叉检查：
// 里面的 Unix 权限分支只有在这里才编得到。
#[path = "../../../src/auth.rs"]
pub mod auth;

#[path = "../../../src/qr.rs"]
pub mod qr;

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

// `mount` 里装着平台分叉：Linux 那一支只有在这里才会被编译。
// 曾经它长在 main.rs 里，于是两边都编不到，`PathBuf` 的格式化错误一直漏到 NAS 上。
#[path = "../../../src/mount.rs"]
pub mod mount;
