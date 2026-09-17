//! 「把虚拟文件树交给内核」这一步，以及它在各平台上的分叉。
//!
//! # 为什么单独一个文件
//!
//! 这个文件的内容是平台相关的：Linux 下真的去挂 FUSE，其它平台上只把树骨架打印出来
//! 就当预览。但**文件本身在所有平台上都会被编译**——这是刻意安排的。
//!
//! 这段代码原先写在 `main.rs` 里，用两个 `#[cfg]` 的 `mount_now` 分开，结果是：
//!
//! - Windows 上的 `cargo build` 完全看不到 Linux 那一支；
//! - `tools/linuxcheck` 只引用了 `fuse_fs.rs`，也看不到它；
//! - 于是 `println!("已卸载 {mountpoint}")` 这种错误（`PathBuf` 没实现 `Display`）
//!   在开发机上 21 个测试全绿、零警告，一直到 NAS 上编译才炸出来。
//!
//! 现在的覆盖是：Windows 的 `cargo build` 编「非 Linux」那一支，
//! `tools/linuxcheck` 以 aarch64-linux 为目标编「Linux」那一支。
//! 两边合起来，这个文件里的每一行都至少被真正编译过一次。
//!
//! 结论适用于以后：**平台专属的代码要放在这种「总是被编译的叶子模块」里**，
//! 而不是用 `#[cfg]` 从 `main.rs` 里挖掉一块。

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;
use crate::vfs;

/// 挂载参数。平台之间只有 `threads` 的用处不同（FUSE 事件循环线程数），
/// 但参数两边都要拼，所以集中放在这里。
pub struct MountArgs {
    pub mountpoint: PathBuf,
    /// true = 列出全部曲目，读到没落地的就去取；false = 只列出已落盘的
    pub ondemand: bool,
    pub allow_other: bool,
    /// 只有 Linux 下的 FUSE 事件循环用得到
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub threads: usize,
}

/// 真正挂载。这个函数只在被卸载时返回。
#[cfg(target_os = "linux")]
pub fn mount_now(
    cfg: &Config,
    args: MountArgs,
    tree: vfs::Vfs,
    materializer: Box<dyn vfs::Materializer>,
) -> Result<()> {
    use crate::fuse_fs;

    let mountpoint = args.mountpoint.clone();
    let req = fuse_fs::MountRequest {
        mountpoint: mountpoint.clone(),
        vfs: tree,
        out_root: cfg.out_root.clone(),
        materializer,
        ondemand: args.ondemand,
        allow_other: args.allow_other,
        threads: args.threads,
    };

    match fuse_fs::run(req) {
        Ok(()) => {
            // `PathBuf` 没有实现 `Display`，必须走 `display()`。
            println!("已卸载 {}", mountpoint.display());
            Ok(())
        }
        Err(e) if args.allow_other => Err(anyhow::anyhow!(
            "{e:#}\n\n\
             提示：如果错误里出现 allow_other / user_allow_other，说明 /etc/fuse.conf \
             还没放行。取消 `user_allow_other` 那一行的注释（需要 root）再挂一次；\
             或者先用 --allow-other=false 验证功能本身是否正常。"
        )),
        Err(e) => Err(e),
    }
}

/// 非 Linux 平台没有 FUSE。但文件树本身是平台无关的，把它算出来打印一下，
/// 至少能确认路径生成和曲目识别是否符合预期。
#[cfg(not(target_os = "linux"))]
pub fn mount_now(
    _cfg: &Config,
    args: MountArgs,
    tree: vfs::Vfs,
    _materializer: Box<dyn vfs::Materializer>,
) -> Result<()> {
    println!("（当前平台不是 Linux，无法真正挂载，下面是本地算出的文件树骨架）");
    for source in tree.children(vfs::ROOT_INO) {
        println!("  {}/", source.name);
        for group in tree.children(source.ino) {
            println!("    {}/   {} 项", group.name, tree.children(group.ino).len());
        }
    }
    Err(anyhow::anyhow!(
        "FUSE 只在 Linux 上可用，当前是 {}，因此没有挂载 {}。\n\
         飞牛 fnOS 就是 Debian/Linux：把源码拷过去 `cargo build --release` 即可。",
        std::env::consts::OS,
        args.mountpoint.display()
    ))
}
