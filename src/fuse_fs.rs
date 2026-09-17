//! FUSE 出口（仅 Linux 编译）。
//!
//! 这一层刻意做得很薄：所有路径、inode、重名的逻辑都在 [`crate::vfs`] 里，
//! 这里只负责把内核的调用翻译过去。原因是 FUSE 的适配代码在 Windows 开发机上
//! 根本编译不了，能放到平台无关层的东西就要放过去。
//!
//! 三条设计要点：
//!
//! 1. **只读**。飞牛音乐只会读，所以整个文件系统挂成 `ro`，
//!    写操作由内核直接返回 EROFS，我们不用实现。
//! 2. **扫描零网络**。`lookup` / `getattr` / `readdir` 只查内存里的树，
//!    一个请求都不发。只有 `open` 才可能去取音频。
//! 3. **按需取回放在 `open` 而不是 `read`**。取回来是整文件落地，
//!    读的时候只是从本地文件 pread，这样播放器随便 seek 都不会触发网络。
//!
//! 内核拿 inode 做 dentry 缓存，所以 `n_threads > 1` 是必须的：
//! 同时有多个进程在扫描目录时，单线程事件循环会被一个慢请求（取回音频）
//! 整个卡住。
//!
//! 还有一条：树是挂载那一刻算出来的快照，而文件会在挂载**之后**才出现
//! （`daily --fetch` 是最典型的例子，它每天都产生新文件），所以 `readdir`
//! 上挂了一层到期重扫——见 [`MusicFs::maybe_refresh`]。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fuser::{
    Config as FuseOptions, Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request, Session, SessionACL,
};

use crate::config::Quality;
use crate::mount::RefreshGate;
use crate::store::Index;
use crate::vfs::{Materializer, Vfs};

/// 属性缓存时间。
///
/// 刻意压到 1 秒：刚落盘的文件要尽快以真实大小出现，而负向结果
/// （"还没下载"）也不能缓存太久，否则取回之后内核仍然认为文件不存在。
const TTL: Duration = Duration::from_secs(1);
const BLOCK: u32 = 4096;

/// 重扫磁盘的最小间隔（取值与理由见 [`crate::mount::REFRESH_SECS`]）。
///
/// 间隔是必须的：重扫要遍历一遍音乐目录，而飞牛音乐扫描时会连发成百上千次
/// `readdir`。30 秒对内网存储足够快（新文件最多晚这么久出现），也不会让机械盘一直转。
const REFRESH_INTERVAL: Duration = Duration::from_secs(crate::mount::REFRESH_SECS);

pub struct MountRequest {
    pub mountpoint: PathBuf,
    pub vfs: Vfs,
    /// 音乐文件真实落地目录，虚拟路径都相对于它解析
    pub out_root: PathBuf,
    pub materializer: Box<dyn Materializer>,
    /// true = 读到没落地的曲目时去取；false = 树里只出现已落地的文件
    pub ondemand: bool,
    /// 允许其他用户访问。飞牛音乐以别的用户跑，开这个才扫得到。
    pub allow_other: bool,
    pub threads: usize,
    /// 重扫时重新读它：挂载期间可能刚扫了新歌单、刚落了新曲目
    pub index_path: PathBuf,
    pub quality: Quality,
}

/// 挂载并进入事件循环，直到被卸载才返回。
pub fn run(req: MountRequest) -> Result<()> {
    let MountRequest {
        mountpoint,
        vfs,
        out_root,
        materializer,
        ondemand,
        allow_other,
        threads,
        index_path,
        quality,
    } = req;

    if !mountpoint.exists() {
        std::fs::create_dir_all(&mountpoint)
            .with_context(|| format!("创建挂载点失败: {}", mountpoint.display()))?;
    }
    if !mountpoint.is_dir() {
        bail!("挂载点不是目录: {}", mountpoint.display());
    }

    let mut options = FuseOptions::default();
    options.mount_options = vec![
        MountOption::FSName("musicm".to_string()),
        MountOption::Subtype("musicm".to_string()),
        MountOption::RO,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::NoAtime,
        // 让内核按我们上报的权限位做检查。用户的 uid 挂在挂载进程名下，
        // 文件是 0444、目录是 0555，所以 allow_other 之后其他用户也能读。
        MountOption::DefaultPermissions,
    ];

    if allow_other {
        options.acl = SessionACL::All;
        // 进程被 kill 时让 fusermount 顺手卸载，免得留下一个卡死的挂载点。
        // fuser 要求 auto_unmount 必须搭配 allow_other / allow_root，所以放在这个分支里。
        options.mount_options.push(MountOption::AutoUnmount);
    }

    // 多线程事件循环：取回音频是秒级阻塞操作，单线程会把整个文件系统卡住。
    options.n_threads = Some(threads.max(1));
    // clone_fd 需要内核 4.5+ 且只在多线程下有意义，收益不大，先不开。
    options.clone_fd = false;

    let fs = MusicFs::new(vfs, out_root, materializer, ondemand, index_path, quality);
    let session = Session::new(fs, &mountpoint, &options)
        .with_context(|| format!("挂载到 {} 失败", mountpoint.display()))?;

    session.run().context("FUSE 事件循环异常退出")?;
    Ok(())
}

struct MusicFs {
    vfs: Mutex<Vfs>,
    out_root: PathBuf,
    materializer: Box<dyn Materializer>,
    ondemand: bool,
    /// 已打开的文件句柄。key 是 FUSE 的 fh，不是 inode——同一个文件可以开多次。
    open_files: Mutex<HashMap<u64, File>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
    /// 重扫要用（见 [`MusicFs::maybe_refresh`]）
    index_path: PathBuf,
    quality: Quality,
    /// 「到期才重扫」的闸门（语义与测试都在 `mount.rs`：那边两个平台都编译）
    refresh: RefreshGate,
}

impl MusicFs {
    fn new(
        vfs: Vfs,
        out_root: PathBuf,
        materializer: Box<dyn Materializer>,
        ondemand: bool,
        index_path: PathBuf,
        quality: Quality,
    ) -> Self {
        MusicFs {
            vfs: Mutex::new(vfs),
            out_root,
            materializer,
            ondemand,
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            index_path,
            quality,
            refresh: RefreshGate::new(REFRESH_INTERVAL),
        }
    }

    /// 到期就在原树上增量重扫一次磁盘（索引也重新读）。
    ///
    /// 挂在这里而不是开一个后台线程：只有真的有人读目录时才需要新的文件，
    /// 而且重扫用的是同一把锁，放在 `readdir` 里天然不会和它自己的遍历打架。
    ///
    /// 重扫失败一律只跳过这一轮——一个只读文件系统不该因为索引的瞬时写坏
    /// 就把 `readdir` 变成 EIO。
    fn maybe_refresh(&self) {
        if !self.refresh.due() {
            return;
        }

        let index = match Index::load(&self.index_path) {
            Ok(index) => index,
            Err(e) => {
                eprintln!("[musicm] 重扫跳过：索引读取失败 {e:#}");
                return;
            }
        };

        let Ok(mut vfs) = self.vfs.lock() else {
            return;
        };
        let stats = vfs.refresh(&self.out_root, &index, self.quality, self.ondemand);
        drop(vfs);

        if stats.added > 0 || stats.materialized > 0 {
            eprintln!(
                "[musicm] 重扫磁盘：新增 {} 个文件 / {} 个转为已落地",
                stats.added, stats.materialized
            );
        }
    }

    fn attr(&self, node: &crate::vfs::Node) -> FileAttr {
        let t = to_systime(node.mtime);
        FileAttr {
            ino: INodeNo(node.ino),
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if node.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if node.is_dir() { 0o555 } else { 0o444 },
            nlink: if node.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK,
            flags: 0,
        }
    }

    /// 把还没落地的节点真正取回来，返回它当前的相对路径。
    ///
    /// 返回路径而不是直接改节点，是因为服务端返回的容器格式可能和我们猜的不一样，
    /// 那样文件名会变（`.flac` → `.mp3`），调用方必须用新路径去开文件。
    fn materialize(&self, ino: u64, track: Option<&str>) -> std::result::Result<String, String> {
        let track = track.ok_or_else(|| "这不是曲目文件，无法按需取回".to_string())?;

        // 只在锁里读元信息，真正的下载在锁外做——否则会把整个文件系统堵住
        let (dir_rel, stem) = {
            let vfs = self.vfs.lock().map_err(|_| "文件树锁已损坏".to_string())?;
            let node = vfs.get(ino).ok_or_else(|| "节点不存在".to_string())?;
            (node.dir_rel(), node.stem())
        };
        let dir = if dir_rel.is_empty() {
            self.out_root.clone()
        } else {
            self.out_root.join(&dir_rel)
        };

        eprintln!("[musicm] 取回 {dir_rel}/{stem}");
        let done = self.materializer.materialize(track, &dir, &stem)?;
        eprintln!(
            "[musicm] 完成 {}（{:.1} MB）",
            done.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            done.size as f64 / 1024.0 / 1024.0
        );

        let mut vfs = self.vfs.lock().map_err(|_| "文件树锁已损坏".to_string())?;
        vfs.adopt(ino, &self.out_root, &done.path, done.size, done.mtime);
        Ok(vfs
            .get(ino)
            .map(|n| n.rel.clone())
            .unwrap_or_else(|| rel_hint(&self.out_root, &done.path)))
    }
}

impl fuser::Filesystem for MusicFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_string_lossy();
        let Ok(vfs) = self.vfs.lock() else {
            reply.error(Errno::EIO);
            return;
        };
        match vfs.lookup(parent.0, &name) {
            Some(node) => {
                let attr = self.attr(node);
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Ok(vfs) = self.vfs.lock() else {
            reply.error(Errno::EIO);
            return;
        };
        match vfs.get(ino.0) {
            Some(node) => {
                let attr = self.attr(node);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // 树是挂载那一刻的快照，而文件会在之后才落地（每日推荐每天都变）。
        // 有人来读目录时顺便看一眼要不要重扫——读到的就是新内容，不需要重新挂载。
        self.maybe_refresh();

        let Ok(vfs) = self.vfs.lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(node) = vfs.get(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if !node.is_dir() {
            reply.error(Errno::ENOTDIR);
            return;
        }

        let parent = node.parent;
        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        entries.push((ino.0, FileType::Directory, ".".to_string()));
        entries.push((parent, FileType::Directory, "..".to_string()));
        for child in vfs.children(ino.0) {
            let kind = if child.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child.ino, kind, child.name));
        }

        // offset 是「下一个条目的序号」，必须比下标大 1，
        // 因为 0 要留作「从头开始」的哨兵值。
        for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(entry_ino), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }

        let Ok((rel, materialized, track)) = self.snapshot(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let rel = if materialized {
            rel
        } else {
            if !self.ondemand {
                reply.error(Errno::ENOENT);
                return;
            }
            match self.materialize(ino.0, track.as_deref()) {
                Ok(new_rel) => new_rel,
                Err(e) => {
                    eprintln!("[musicm] 取回失败 {rel}: {e}");
                    reply.error(Errno::EIO);
                    return;
                }
            }
        };

        match File::open(self.out_root.join(&rel)) {
            Ok(file) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut map) = self.open_files.lock() {
                    map.insert(fh, file);
                }
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => {
                eprintln!("[musicm] 打开 {rel} 失败: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Ok(map) = self.open_files.lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(file) = map.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };

        // pread：偏移量显式给，文件指针不共享，多个读者不会互相干扰
        let mut buf = vec![0u8; size as usize];
        match file.read_at(&mut buf, offset) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => {
                eprintln!("[musicm] 读取失败 offset={offset}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Ok(mut map) = self.open_files.lock() {
            map.remove(&fh.0);
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let stats = self.vfs.lock().map(|v| v.stats()).unwrap_or_default();
        let blocks = (stats.bytes / BLOCK as u64).max(1);
        // 只读的虚拟视图，没有真实容量概念：报 bfree/bavail = 0 是诚实的。
        reply.statfs(
            blocks,
            0,
            0,
            (stats.files + stats.dirs) as u64,
            0,
            BLOCK,
            255,
            BLOCK,
        );
    }
}

impl MusicFs {
    /// 取出 open() 需要的那几个字段，避免在持锁状态下做网络请求。
    fn snapshot(
        &self,
        ino: u64,
    ) -> Result<(String, bool, Option<String>), ()> {
        let vfs = self.vfs.lock().map_err(|_| ())?;
        let node = vfs.get(ino).ok_or(())?;
        if node.is_dir() {
            return Err(());
        }
        Ok((node.rel.clone(), node.materialized, node.track.clone()))
    }
}

fn to_systime(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// 兜底：`adopt` 之后理论上一定查得到，这里只是不给 panic 留口子。
fn rel_hint(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}
