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

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Config;
use crate::vfs;

/// 挂载后重扫磁盘的最小间隔（秒）。
///
/// 放在这个「总是被编译的文件」里，是为了让 CLI 的提示语和 FUSE 的实际行为用同一个
/// 数字——写成两处，改一处就会有一句骗人的提示（`fuse_fs` 里的常量指向它）。
///
/// 为什么需要重扫：树是挂载那一刻的快照，而「每日推荐」每天都产生新文件、
/// 按需取回也在挂载之后落地。没有它，用户每天都要重新挂载一次。
pub const REFRESH_SECS: u64 = 30;

/// 「到期才重扫」的闸门。
///
/// 逻辑本身很小，放在这里的原因是**用它的人在 Windows 上根本不参与编译**
/// （`fuse_fs` 只是 Linux 模块），留在那边就永远没有测试——这正是本文件存在的
/// 理由，见文件头。
///
/// 记的是「尝试」时刻而不是「成功」时刻：索引临时读不出来时也要等满一个间隔，
/// 否则每一次 `readdir` 都会去读一次磁盘。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct RefreshGate {
    interval: Duration,
    last: Mutex<Instant>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl RefreshGate {
    pub fn new(interval: Duration) -> Self {
        RefreshGate {
            interval,
            // 刚挂载时树是刚算出来的，没必要立刻再扫一遍
            last: Mutex::new(Instant::now()),
        }
    }

    /// 到间隔了返回 true，并把此刻记为「已尝试」。间隔为 0 等于每次都放行。
    pub fn due(&self) -> bool {
        let Ok(mut last) = self.last.lock() else {
            // 锁坏了就不重扫。这是一项优化，坏掉时最保守的行为是「当作没到点」：
            // 拿不到锁还去读磁盘，只会让别的请求一起卡住。
            return false;
        };
        if last.elapsed() < self.interval {
            return false;
        }
        *last = Instant::now();
        true
    }

    /// 只给测试用：把「上次尝试」拨回到 `by` 之前。
    #[cfg(test)]
    fn rewind(&self, by: Duration) {
        if let Ok(mut last) = self.last.lock() {
            *last = Instant::now().checked_sub(by).unwrap_or_else(Instant::now);
        }
    }
}

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
        // 重扫用：索引路径 + 档位，让挂载期间新落地的文件（尤其是每日推荐）
        // 不用重新挂载就能出现。
        index_path: cfg.index_path(),
        quality: cfg.quality,
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

// ---------- 给 Web 管理界面用的挂载控制 ----------
//
// 这三个函数同样按平台分叉，但都放在这个「总是被编译的文件」里，
// 所以 Windows 的 `cargo build` 编非 Linux 那一支、`tools/linuxcheck` 编 Linux 那一支，
// 合起来每一行都被真正编译过（理由见文件头的模块文档）。
//
// 用 `cfg!` 而不是把整个函数 `#[cfg]` 掉的那个例外是 `fuse_supported`：
// 它要返回一个**运行时**布尔值给界面决定按钮亮不亮，两个平台都得有自己的实现。

/// 当前平台能不能真的挂载 FUSE。
pub const fn fuse_supported() -> bool {
    cfg!(target_os = "linux")
}

/// 卸载一个挂载点。
///
/// 走外部命令而不是 `libc::umount`：挂载本身也是 `fusermount3` 干的
/// （见文件头关于 `default-features = false` 的取舍），保持同一套依赖。
#[cfg(target_os = "linux")]
pub fn unmount(mountpoint: &Path) -> Result<()> {
    use anyhow::bail;

    // fusermount3 是 fuse3 包的新名字，老发行版里还叫 fusermount。
    // 两个都试，但要把「没装」和「装了但卸载失败」分开报——
    // 前者要去装包，后者要看错误信息，提示完全不同。
    let mut attempts: Vec<String> = Vec::new();
    for bin in ["fusermount3", "fusermount"] {
        let out = match std::process::Command::new(bin)
            .arg("-u")
            .arg(mountpoint)
            .output()
        {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                attempts.push(format!("{bin}（未安装）"));
                continue;
            }
            Err(e) => bail!("执行 {bin} 失败: {e}"),
        };
        if out.status.success() {
            return Ok(());
        }
        attempts.push(format!(
            "{bin}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    bail!(
        "卸载 {} 失败。\n尝试记录：{}\n\
         挂载点本来就没挂上（或已被别的进程卸载）也会是这个样子，\
         可以用 `mount | grep musicm` 确认。",
        mountpoint.display(),
        attempts.join("；")
    )
}

#[cfg(not(target_os = "linux"))]
pub fn unmount(_mountpoint: &Path) -> Result<()> {
    anyhow::bail!(
        "FUSE 只在 Linux 上可用，当前是 {}，没有可卸载的挂载点。",
        std::env::consts::OS
    )
}

/// 挂载点上现在是什么文件系统。没挂载时返回 `None`。
///
/// 存在的理由：进程重启之后界面上那个「已挂载」的标记就丢了，
/// 但内核里的挂载其实还在。问一次 `/proc/mounts` 才能说实话。
#[cfg(target_os = "linux")]
pub fn mounted_fstype(mountpoint: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    let want = escape_mount_field(&mountpoint.to_string_lossy());
    // 每行形如：musicm /vol1/1000/music fuse.musicm ro,nosuid,...
    // 第 2 列是挂载点，第 3 列是文件系统类型。
    text.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let _source = it.next()?;
        if it.next()? != want {
            return None;
        }
        it.next().map(|s| s.to_string())
    })
}

#[cfg(not(target_os = "linux"))]
pub fn mounted_fstype(_mountpoint: &Path) -> Option<String> {
    None
}

/// `/proc/mounts` 把路径里的空格转义成 `\040`，比较前要转过来。
#[cfg(target_os = "linux")]
fn escape_mount_field(path: &str) -> String {
    path.replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 平台判定要和 `cfg!` 说的一致——界面靠它决定挂载按钮亮不亮。
    #[test]
    fn fuse_support_matches_the_platform() {
        assert_eq!(fuse_supported(), cfg!(target_os = "linux"));
    }

    /// 非 Linux 上卸载必须明确失败而不是静默成功：
    /// 静默成功会让界面显示「已卸载」，而实际上什么都没发生。
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn unmount_is_rejected_off_linux() {
        let err = unmount(Path::new("/tmp/musicm-not-here")).unwrap_err();
        assert!(err.to_string().contains("Linux"), "实际: {err}");
    }

    /// 挂载点的空格转义。比错了会导致「明明挂上了却显示未挂载」。
    #[test]
    #[cfg(target_os = "linux")]
    fn mount_fields_are_escaped_like_the_kernel_does() {
        assert_eq!(escape_mount_field("/vol1/音乐"), "/vol1/音乐");
        assert_eq!(escape_mount_field("/vol1/my music"), "/vol1/my\\040music");
    }

    /// 间隔没到就不该放行：`readdir` 在一次目录扫描里会被调用成百上千次，
    /// 每次都重扫磁盘就等于把音乐库目录walk 了成百上千遍。
    #[test]
    fn refresh_gate_stays_closed_within_the_interval() {
        let gate = RefreshGate::new(Duration::from_secs(30));
        assert!(!gate.due(), "刚挂载时不该到点");
        assert!(!gate.due(), "连问两次都不该放行：第一次没动过时刻");
    }

    /// 放行过一次之后必须重新等满间隔——否则「到期」这个概念就没意义了。
    #[test]
    fn refresh_gate_consumes_its_turn() {
        let gate = RefreshGate::new(Duration::from_secs(30));
        gate.rewind(Duration::from_secs(60));
        assert!(gate.due(), "拨回到 60 秒前应当放行");
        assert!(!gate.due(), "放过一次之后要重新计时");
    }
}
