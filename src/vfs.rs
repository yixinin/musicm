//! 虚拟文件树。
//!
//! 这棵树是 FUSE / WebDAV 出口共用的内核，刻意**不依赖网络、不依赖平台**。
//! 真正容易出错的部分——inode 稳定性、路径拼接、重名、未落地文件的属性——
//! 全在这里，也全都能在任意平台上跑单元测试。
//!
//! 树的内容 = 已落地的真实文件 ∪ 索引里有但还没下载的曲目：
//!
//! - 真实文件走目录遍历，顺带把 `.lrc`、`cover.jpg` 这类伴随文件带出来
//! - 未落地的曲目按命名规则算出一个「占位节点」，内核 `open` 到它时才真正去取音频
//!
//! 有一条约束必须守住：**inode 跨刷新保持稳定**。内核拿 inode 做 dentry 缓存，
//! 同一个路径一旦换了 inode，正在播放的文件会突然读到别的东西。
//! 所以这里不在变更时重建整棵树，只在原地更新节点。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Quality;
use crate::naming;
use crate::store::Index;

/// 根目录 inode。FUSE 规定 1 是根。
pub const ROOT_INO: u64 = 1;

/// 目录遍历的最大深度。布局只需要 3 层，留点余量防止有人手动塞了深目录。
const MAX_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub ino: u64,
    pub parent: u64,
    /// 名字（只是路径的最后一段，不含目录）
    pub name: String,
    pub kind: NodeKind,
    /// 相对音乐根目录的路径，用 `/` 分隔；根节点是空串
    pub rel: String,
    pub size: u64,
    pub mtime: u64,
    /// 音频文件对应的曲目 key，只有曲目文件才可能有
    pub track: Option<String>,
    /// 是否已经真实落盘。false = 还在云端，`open` 时才取
    pub materialized: bool,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    /// 所在目录的相对路径（`网易云/热歌榜`），根目录下的文件返回空串。
    pub fn dir_rel(&self) -> String {
        match self.rel.rfind('/') {
            Some(i) => self.rel[..i].to_string(),
            None => String::new(),
        }
    }

    /// 文件名主干（不含扩展名）。落盘时要用它拼实际路径。
    ///
    /// 刻意不用 `Path::file_stem`：歌名里带点号的情况太常见（`Mr.Children`），
    /// 这里只需要砍掉最后一个点号之后的部分，语义更明确。
    pub fn stem(&self) -> String {
        let base = match self.rel.rfind('/') {
            Some(i) => &self.rel[i + 1..],
            None => self.rel.as_str(),
        };
        match base.rfind('.') {
            Some(i) => base[..i].to_string(),
            None => base.to_string(),
        }
    }
}

/// 把虚拟节点变成磁盘上真实文件的那一层。
///
/// 抽成 trait 是为了让文件系统内核完全不认识「网易云」这回事：
/// 树、路径、inode 这些逻辑可以脱离网络单测，实现也可以换成 WebDAV 版或离线版。
pub trait Materializer: Send + Sync {
    /// 在 `dir` 下用 `stem` 落一个文件，返回实际落地的信息。
    ///
    /// 扩展名不在这里决定——它取决于服务端返回的容器格式，
    /// 所以返回值里的 `path` 才是最终路径，可能和调用方的预期不同。
    fn materialize(&self, track_key: &str, dir: &Path, stem: &str) -> Result<Materialized, String>;
}

#[derive(Debug, Clone)]
pub struct Materialized {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug)]
pub struct Vfs {
    nodes: BTreeMap<u64, Node>,
    /// 父 inode -> (名字 -> 子 inode)。用 BTreeMap 保证 readdir 顺序稳定。
    children: BTreeMap<u64, BTreeMap<String, u64>>,
    /// 相对路径 -> inode，用于「这个路径是否已存在」
    by_rel: BTreeMap<String, u64>,
    next_ino: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VfsStats {
    pub dirs: usize,
    pub files: usize,
    /// 已列出但还没落盘的曲目数
    pub pending: usize,
    /// 已落盘文件的字节数
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
struct Counts {
    files: usize,
    /// 待取回节点的 inode 集合（重扫前后对比用）
    pending: BTreeSet<u64>,
}

#[derive(Debug, Clone)]
struct Plan {
    track: String,
    size: u64,
    mtime: u64,
}

/// 一次重扫的结果。只用于日志与测试断言，树本身的语义不依赖它。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// 新出现的文件节点（之前树里没有）
    pub added: usize,
    /// 从「待取回」**原位**变成「已落地」的，inode 不变
    pub materialized: usize,
}

impl Vfs {
    /// 构建虚拟树。
    ///
    /// `include_pending = false` 只暴露已经落盘的文件，对应「零网络」模式：
    /// 扫描最快，代价是没下载的曲目看不见。
    pub fn build(out_root: &Path, index: &Index, quality: Quality, include_pending: bool) -> Vfs {
        let mut vfs = Vfs::empty();
        vfs.load(out_root, index, quality, include_pending);
        vfs
    }

    /// 增量装载：索引期望的文件 ∪ 磁盘上已有的文件。
    ///
    /// **已有的节点一律复用**（`by_rel` 命中就直接返回原 inode），不做任何清理，
    /// 所以 `build` 与 [`Vfs::refresh`] 共用这一条路径时 inode 语义是一致的。
    fn load(&mut self, out_root: &Path, index: &Index, quality: Quality, include_pending: bool) {
        let planned = plan(index, quality);

        // 未落地的曲目先占位，文件名必须在下载之前就定下来——
        // 这正是 `expected_ext` 那个猜测存在的原因。
        if include_pending {
            for (rel, p) in &planned {
                self.insert_chain(rel, Some(p.track.clone()), p.size, p.mtime, false);
            }
        }

        // 再扫真实目录：已存在的文件会被标成 materialized 并覆盖成真实大小/时间。
        if out_root.is_dir() {
            self.scan_dir(out_root, out_root, ROOT_INO, 0, &planned);
        }

        // 不在任何歌单里的曲目不走计划（无法参与同名去重），
        // 但只要落过盘就会在上面这一步被带出来，够用了。
    }

    /// 磁盘上又多出文件之后，在原树上增量重扫。
    ///
    /// 存在的理由很具体：「每日推荐」天天变，`daily --fetch` 落地的文件是在**挂载之后**
    /// 才出现的，而树是挂载那一刻算出来的快照。没有这一步，用户每天都要重新挂载一次
    /// 才看得到今天的日推。
    ///
    /// **为什么不重建整棵树**：内核拿 inode 做 dentry 缓存，重建会把已有路径的 inode
    /// 全换掉，正在播放的文件会突然读到别的东西（见文件头那条约束）。所以这里只做
    /// 「复用旧 inode、给新路径新分配」，绝不重新编号。
    ///
    /// **只增不删**，这一点也是刻意的：删除节点会和 [`Vfs::adopt`] 竞争。
    /// 取回音频的流程是「按猜测的扩展名先占位 → 落地 → adopt 改名」，在改名完成之前
    /// 猜测的那个路径在磁盘上确实不存在；一次重扫要是把「磁盘上找不到」当成删除，
    /// 就会把正在取回的节点删掉。代价是手动删掉的文件会留在树里直到重新挂载——
    /// 比误删要好得多。
    ///
    /// `index` 应当是**重新从磁盘读出来的**索引（而不是挂载时的快照），这样挂载期间
    /// 新扫的歌单、新落地的曲目也能一并进来。
    pub fn refresh(
        &mut self,
        out_root: &Path,
        index: &Index,
        quality: Quality,
        include_pending: bool,
    ) -> RefreshStats {
        let before = self.snapshot_counts();
        self.load(out_root, index, quality, include_pending);

        // 「升级」要按**重扫前就是待取回**的那批节点来数。直接用「已落地节点数变多了多少」
        // 会把新落盘的文件也算进去——那些文件本来就不是占位节点，两种事实混在一起，
        // 日志就会撒谎（新落地 1 首却报「1 个转为已落地」，听起来像有曲目被取回了）。
        let materialized = before
            .pending
            .iter()
            .filter(|ino| self.nodes.get(ino).is_some_and(|n| n.materialized))
            .count();
        let after_files = self.snapshot_counts().files;

        RefreshStats {
            added: after_files.saturating_sub(before.files),
            materialized,
        }
    }

    fn snapshot_counts(&self) -> Counts {
        let mut c = Counts::default();
        for node in self.nodes.values() {
            if node.kind != NodeKind::File {
                continue;
            }
            c.files += 1;
            if !node.materialized {
                c.pending.insert(node.ino);
            }
        }
        c
    }

    fn empty() -> Vfs {
        let mut vfs = Vfs {
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            by_rel: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
        };
        vfs.nodes.insert(
            ROOT_INO,
            Node {
                ino: ROOT_INO,
                parent: ROOT_INO,
                name: String::new(),
                kind: NodeKind::Dir,
                rel: String::new(),
                size: 0,
                mtime: 0,
                track: None,
                materialized: true,
            },
        );
        vfs.children.insert(ROOT_INO, BTreeMap::new());
        vfs.by_rel.insert(String::new(), ROOT_INO);
        vfs
    }

    pub fn get(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        let ino = *self.children.get(&parent)?.get(name)?;
        self.nodes.get(&ino)
    }

    /// 子节点列表，已按名字排序。返回克隆是为了让调用方不用一直握着锁。
    pub fn children(&self, ino: u64) -> Vec<Node> {
        let Some(kids) = self.children.get(&ino) else {
            return Vec::new();
        };
        kids.values()
            .filter_map(|i| self.nodes.get(i).cloned())
            .collect()
    }

    /// 节点对应的真实路径。
    pub fn abs(&self, out_root: &Path, ino: u64) -> Option<PathBuf> {
        let node = self.nodes.get(&ino)?;
        if node.rel.is_empty() {
            return Some(out_root.to_path_buf());
        }
        Some(out_root.join(node.rel.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }

    /// 落盘完成后把节点更新成真实状态。
    ///
    /// 服务端实际返回的容器格式可能和我们猜的扩展名不一样（实测请求无损经常
    /// 拿到 320k mp3），这时必须改名——否则一个真正的 mp3 挂着 `.flac` 后缀，
    /// 播放器会直接报错。返回新名字（没改名则返回 `None`）。
    pub fn adopt(
        &mut self,
        ino: u64,
        out_root: &Path,
        actual: &Path,
        size: u64,
        mtime: u64,
    ) -> Option<String> {
        let rel = rel_string(out_root, actual)?;
        let old = self.nodes.get(&ino)?.clone();

        if rel != old.rel {
            let new_name = actual.file_name()?.to_string_lossy().to_string();
            self.by_rel.remove(&old.rel);
            self.by_rel.insert(rel.clone(), ino);
            if let Some(kids) = self.children.get_mut(&old.parent) {
                kids.remove(&old.name);
                kids.insert(new_name.clone(), ino);
            }
            if let Some(node) = self.nodes.get_mut(&ino) {
                node.rel = rel;
                node.name = new_name.clone();
                node.size = size;
                node.mtime = mtime;
                node.materialized = true;
            }
            return Some(new_name);
        }

        if let Some(node) = self.nodes.get_mut(&ino) {
            node.size = size;
            node.mtime = mtime;
            node.materialized = true;
        }
        None
    }

    pub fn stats(&self) -> VfsStats {
        let mut s = VfsStats::default();
        for node in self.nodes.values() {
            match node.kind {
                NodeKind::Dir => s.dirs += 1,
                NodeKind::File => {
                    s.files += 1;
                    if node.materialized {
                        s.bytes += node.size;
                    } else {
                        s.pending += 1;
                    }
                }
            }
        }
        s
    }

    // ---------- 内部：建树 ----------

    fn alloc(
        &mut self,
        parent: u64,
        name: &str,
        kind: NodeKind,
        rel: &str,
        size: u64,
        mtime: u64,
        track: Option<String>,
        materialized: bool,
    ) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(
            ino,
            Node {
                ino,
                parent,
                name: name.to_string(),
                kind,
                rel: rel.to_string(),
                size,
                mtime,
                track,
                materialized,
            },
        );
        self.by_rel.insert(rel.to_string(), ino);
        self.children.entry(parent).or_default().insert(name.to_string(), ino);
        self.children.entry(ino).or_default();
        ino
    }

    /// 沿路径把缺失的目录和最后那个文件补齐，已存在的直接复用。
    fn insert_chain(
        &mut self,
        rel: &str,
        track: Option<String>,
        size: u64,
        mtime: u64,
        materialized: bool,
    ) -> u64 {
        let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        let mut parent = ROOT_INO;
        let mut acc = String::new();

        for (i, part) in parts.iter().enumerate() {
            acc = if acc.is_empty() {
                (*part).to_string()
            } else {
                format!("{acc}/{part}")
            };
            let last = i + 1 == parts.len();

            if last {
                if let Some(&ino) = self.by_rel.get(&acc) {
                    return ino;
                }
                return self.alloc(
                    parent,
                    part,
                    NodeKind::File,
                    &acc,
                    size,
                    mtime,
                    track,
                    materialized,
                );
            }
            parent = match self.by_rel.get(&acc) {
                Some(&ino) => ino,
                None => self.alloc(parent, part, NodeKind::Dir, &acc, 0, mtime, None, true),
            };
        }
        ROOT_INO
    }

    fn scan_dir(
        &mut self,
        root: &Path,
        dir: &Path,
        dir_ino: u64,
        depth: usize,
        planned: &BTreeMap<String, Plan>,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        let mut files: Vec<String> = Vec::new();
        let mut dirs: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let raw = entry.file_name();
            let Some(name) = raw.to_str() else { continue };
            // 跳过隐藏文件：下载中的 `.xxx.part.mp3` 和索引临时文件都在这一档
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                dirs.push(name.to_string());
            } else if kind.is_file() {
                files.push(name.to_string());
            }
        }
        // 排序保证 readdir 顺序稳定（read_dir 本身不保证顺序）
        files.sort();
        dirs.sort();

        for name in files {
            let abs = dir.join(&name);
            let Some(rel) = rel_string(root, &abs) else { continue };
            let meta = fs::metadata(&abs).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(to_secs)
                .unwrap_or(0);
            let track = planned.get(&rel).map(|p| p.track.clone());

            match self.by_rel.get(&rel).copied() {
                // 计划里已经有占位节点，升级成已落盘
                Some(ino) => {
                    if let Some(node) = self.nodes.get_mut(&ino) {
                        node.size = size;
                        node.mtime = mtime;
                        node.materialized = true;
                        if track.is_some() {
                            node.track = track;
                        }
                    }
                }
                None => {
                    self.alloc(dir_ino, &name, NodeKind::File, &rel, size, mtime, track, true);
                }
            }
        }

        for name in dirs {
            let abs = dir.join(&name);
            let Some(rel) = rel_string(root, &abs) else { continue };
            let mtime = fs::metadata(&abs)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(to_secs)
                .unwrap_or(0);
            let ino = match self.by_rel.get(&rel).copied() {
                Some(ino) => ino,
                None => self.alloc(dir_ino, &name, NodeKind::Dir, &rel, 0, mtime, None, true),
            };
            self.scan_dir(root, &abs, ino, depth + 1, planned);
        }
    }
}

/// 索引里期望存在的文件：相对路径 -> 曲目。
///
/// 命名规则必须和下载流程用的是同一套（`naming::unique_stems` + `group_rel`），
/// 否则虚拟树里看到的路径和磁盘上的路径对不上，缓存永远命不中。
fn plan(index: &Index, quality: Quality) -> BTreeMap<String, Plan> {
    let mut out = BTreeMap::new();

    for playlist in index.playlists() {
        let tracks = index.playlist_tracks(&playlist.key());
        if tracks.is_empty() {
            continue;
        }
        let stems = naming::unique_stems(&tracks);
        let dir = naming::group_rel(&playlist.source, Some(&playlist.name));

        for (track, stem) in tracks.iter().zip(stems) {
            let cached = index.file_of(&track.key());
            // 下过一次就知道真实容器是什么了，比按档位猜准得多
            let ext = match cached {
                Some(f) => naming::sanitize_ext(&f.ext),
                None => quality.expected_ext().to_string(),
            };
            let rel = join_rel(&dir, &format!("{stem}.{ext}"));            let (size, mtime) = match cached {
                Some(f) => (f.bytes, f.fetched_at),
                None => (estimate_size(track, quality), index.updated_at),
            };
            out.insert(
                rel,
                Plan {
                    track: track.key(),
                    size,
                    mtime,
                },
            );
        }
    }
    out
}

/// 未落地文件的占位大小。
/// 播放器与刮削器会看这个值，报 0 容易让它们认为文件损坏。
fn estimate_size(track: &crate::model::Track, quality: Quality) -> u64 {
    quality.br() / 8 * track.duration_secs()
}

fn join_rel(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    }
}

fn rel_string(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    Some(s.trim_start_matches('/').to_string())
}

fn to_secs(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CachedFile, Playlist, Track};
    use std::path::PathBuf;

    fn track(id: u64, no: u32, name: &str) -> Track {
        Track {
            source: "netease".to_string(),
            id,
            name: name.to_string(),
            artists: vec!["歌手".to_string()],
            album: "专辑".to_string(),
            album_id: 1,
            cover_url: None,
            duration_ms: 240_000,
            track_no: no,
            disc: 1,
            fee: 0,
            playable: None,
        }
    }

    /// 造一个「一张歌单、两首曲目」的索引；第一首已在磁盘上。
    fn fixture(root: &Path) -> Index {
        let tracks = vec![track(1, 1, "已下载"), track(2, 2, "没下载")];
        let pl = Playlist {
            source: "netease".to_string(),
            id: 100,
            name: "热歌榜".to_string(),
            creator: "我".to_string(),
            cover_url: None,
            declared_count: 2,
            scanned_at: 1_700_000_000,
            track_ids: vec![1, 2],
        };
        let mut index = Index::default();
        index.upsert_playlist(pl, tracks);
        index.updated_at = 1_700_000_500;

        // 磁盘上只放第一首
        let dir = root.join("网易云").join("热歌榜");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("01 已下载 - 歌手.mp3"), vec![0u8; 1234]).unwrap();
        fs::write(dir.join("cover.jpg"), vec![0u8; 10]).unwrap();

        index.record_file(CachedFile {
            track: "netease:1".to_string(),
            path: dir.join("01 已下载 - 歌手.mp3").to_string_lossy().to_string(),
            bytes: 1234,
            quality: "exhigh".to_string(),
            ext: "mp3".to_string(),
            fetched_at: 1_700_000_400,
            cover_embedded: true,
            lyrics_embedded: true,
            lrc_path: None,
        });
        index
    }

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "musicm_vfs_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn builds_expected_tree() {
        let root = temp_root("tree");
        let index = fixture(&root);
        let vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        // 根 -> 网易云 -> 热歌榜
        let source = vfs.lookup(ROOT_INO, "网易云").expect("音源目录");
        assert!(source.is_dir());
        let pl = vfs.lookup(source.ino, "热歌榜").expect("歌单目录");

        let names: Vec<String> = vfs.children(pl.ino).into_iter().map(|n| n.name).collect();
        assert_eq!(
            names,
            vec![
                "01 已下载 - 歌手.mp3".to_string(),
                "02 没下载 - 歌手.mp3".to_string(),
                "cover.jpg".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cached_file_is_materialized_pending_is_not() {
        let root = temp_root("mat");
        let index = fixture(&root);
        let vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        let done = vfs.lookup(pl, "01 已下载 - 歌手.mp3").unwrap();
        let todo = vfs.lookup(pl, "02 没下载 - 歌手.mp3").unwrap();

        assert!(done.materialized);
        assert_eq!(done.size, 1234, "应当采用磁盘上的真实大小");
        assert!(!todo.materialized);
        // 320kbps × 240s / 8 = 9.6MB，只要求是个非零的合理估算
        assert!(todo.size > 5_000_000 && todo.size < 15_000_000, "估算 {}", todo.size);
        assert_eq!(todo.track.as_deref(), Some("netease:2"));

        let stats = vfs.stats();
        assert_eq!(stats.pending, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cached_mode_hides_pending() {
        let root = temp_root("cachedonly");
        let index = fixture(&root);
        let vfs = Vfs::build(&root, &index, Quality::Exhigh, false);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        assert!(vfs.lookup(pl, "01 已下载 - 歌手.mp3").is_some());
        assert!(vfs.lookup(pl, "02 没下载 - 歌手.mp3").is_none());
        assert_eq!(vfs.stats().pending, 0);
        let _ = fs::remove_dir_all(&root);
    }

    /// 挂载之后才落地的文件（`daily --fetch` 就是这个场景）必须能被重扫进来，
    /// 而且**已有路径的 inode 一个都不能变**——内核的 dentry 缓存认这个。
    #[test]
    fn refresh_picks_up_new_files_without_renumbering() {
        let root = temp_root("refresh");
        let index = fixture(&root);
        let mut vfs = Vfs::build(&root, &index, Quality::Exhigh, false);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        let before = vfs.lookup(pl, "01 已下载 - 歌手.mp3").unwrap().ino;

        // 模拟日推落地：一个新目录 + 一个文件
        let daily = root.join("网易云").join("每日推荐");
        fs::create_dir_all(&daily).unwrap();
        fs::write(daily.join("新歌 - 歌手.mp3"), vec![0u8; 10]).unwrap();

        let stats = vfs.refresh(&root, &index, Quality::Exhigh, false);
        assert_eq!(stats.added, 1, "只该多出那一个新文件");
        assert_eq!(stats.materialized, 0);

        assert_eq!(
            vfs.lookup(pl, "01 已下载 - 歌手.mp3").unwrap().ino,
            before,
            "旧文件的 inode 必须原样保留"
        );
        // 日推目录不能被误判成歌单曲目，它就是一块普通磁盘内容
        let daily_ino = vfs.lookup(source, "每日推荐").expect("日推目录").ino;
        let node = vfs.lookup(daily_ino, "新歌 - 歌手.mp3").expect("新文件");
        assert!(node.materialized);
        assert!(node.track.is_none(), "不属于任何歌单就不该挂曲目 key");
        let _ = fs::remove_dir_all(&root);
    }

    /// 占位节点等到文件真的落地时，要原位升级而不是多出一个条目——
    /// 否则 readdir 会同时报出「没下载」和「已下载」两个同名文件。
    #[test]
    fn refresh_upgrades_pending_in_place() {
        let root = temp_root("refresh_mat");
        let index = fixture(&root);
        let mut vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        let ino = vfs.lookup(pl, "02 没下载 - 歌手.mp3").unwrap().ino;
        assert!(!vfs.get(ino).unwrap().materialized);

        let dir = root.join("网易云").join("热歌榜");
        fs::write(dir.join("02 没下载 - 歌手.mp3"), vec![0u8; 999]).unwrap();

        let stats = vfs.refresh(&root, &index, Quality::Exhigh, true);
        assert_eq!(stats.materialized, 1);
        assert_eq!(stats.added, 0, "升级不该新增条目");

        let node = vfs.lookup(pl, "02 没下载 - 歌手.mp3").unwrap();
        assert_eq!(node.ino, ino, "inode 必须保持不变");
        assert!(node.materialized);
        assert_eq!(node.size, 999, "要换成磁盘上的真实大小");
        assert_eq!(vfs.children(pl).len(), 3, "条目数不该变: {:?}", vfs.children(pl));
        assert_eq!(vfs.stats().pending, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_tracks_get_distinct_names() {
        let root = temp_root("dup");
        let tracks = vec![track(1, 1, "同名"), track(1, 1, "同名")];
        let pl = Playlist {
            source: "netease".to_string(),
            id: 7,
            name: "重复".to_string(),
            creator: "我".to_string(),
            cover_url: None,
            declared_count: 2,
            scanned_at: 1,
            track_ids: vec![1, 1],
        };
        let mut index = Index::default();
        index.upsert_playlist(pl, tracks);
        let vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "重复").unwrap().ino;
        let names: Vec<String> = vfs.children(pl).into_iter().map(|n| n.name).collect();
        assert_eq!(names.len(), 2, "同名曲目必须各占一个条目: {names:?}");
        assert_ne!(names[0], names[1]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn adopt_renames_when_real_container_differs() {
        let root = temp_root("adopt");
        let index = fixture(&root);
        let mut vfs = Vfs::build(&root, &index, Quality::Lossless, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        // 按 lossless 档位猜的是 .flac
        let node = vfs.lookup(pl, "02 没下载 - 歌手.flac").unwrap();
        let ino = node.ino;

        // 服务端实际给的是 mp3
        let actual = root
            .join("网易云")
            .join("热歌榜")
            .join("02 没下载 - 歌手.mp3");
        fs::write(&actual, vec![0u8; 999]).unwrap();
        let renamed = vfs.adopt(ino, &root, &actual, 999, 42);

        assert_eq!(renamed.as_deref(), Some("02 没下载 - 歌手.mp3"));
        let node = vfs.get(ino).unwrap();
        assert_eq!(node.size, 999);
        assert!(node.materialized);
        // 旧名字必须消失，新名字必须能查到——否则 readdir 会同时报出两个
        assert!(vfs.lookup(pl, "02 没下载 - 歌手.flac").is_none());
        assert!(vfs.lookup(pl, "02 没下载 - 歌手.mp3").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn inodes_are_stable_across_adopt() {
        let root = temp_root("stable");
        let index = fixture(&root);
        let mut vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        let ino = vfs.lookup(pl, "02 没下载 - 歌手.mp3").unwrap().ino;

        let actual = root.join("网易云").join("热歌榜").join("02 没下载 - 歌手.mp3");
        fs::write(&actual, vec![0u8; 5]).unwrap();
        vfs.adopt(ino, &root, &actual, 5, 7);

        assert_eq!(vfs.lookup(pl, "02 没下载 - 歌手.mp3").unwrap().ino, ino);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hidden_files_are_skipped() {
        let root = temp_root("hidden");
        let index = fixture(&root);
        // 中断下载残留的分片
        let dir = root.join("网易云").join("热歌榜");
        fs::write(dir.join(".01 x - y.part.mp3"), b"junk").unwrap();
        let vfs = Vfs::build(&root, &index, Quality::Exhigh, true);

        let source = vfs.lookup(ROOT_INO, "网易云").unwrap().ino;
        let pl = vfs.lookup(source, "热歌榜").unwrap().ino;
        let names: Vec<String> = vfs.children(pl).into_iter().map(|n| n.name).collect();
        assert!(
            !names.iter().any(|n| n.starts_with('.')),
            "临时分片不该出现在文件树里: {names:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stem_and_dir_split_rel_path() {
        let node = Node {
            ino: 1,
            parent: 1,
            name: "01 Mr.Children - 歌手.mp3".to_string(),
            kind: NodeKind::File,
            rel: "网易云/热歌榜/01 Mr.Children - 歌手.mp3".to_string(),
            size: 0,
            mtime: 0,
            track: None,
            materialized: true,
        };
        assert_eq!(node.dir_rel(), "网易云/热歌榜");
        // 只砍掉最后一个点号之后的部分，歌名里的点号要保住
        assert_eq!(node.stem(), "01 Mr.Children - 歌手");
    }
}
