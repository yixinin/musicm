//! 索引层。
//!
//! 刻意用 JSON 而不是 SQLite：`rusqlite` 的 bundled 模式需要 C 工具链，
//! 在飞牛 aarch64 上编译要额外装 build-essential，这个代价现在不值得。
//! 目标是几千到几万首，JSON 全量读写完全够用；真到十万级再换 `redb`（纯 Rust）即可。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{now_secs, CachedFile, Playlist, Track};

pub const INDEX_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub updated_at: u64,
    pub playlists: BTreeMap<String, Playlist>,
    pub tracks: BTreeMap<String, Track>,
    /// 已落地的文件，key 与 tracks 相同
    pub files: BTreeMap<String, CachedFile>,
}

impl Default for Index {
    fn default() -> Self {
        Index {
            version: INDEX_VERSION,
            updated_at: 0,
            playlists: BTreeMap::new(),
            tracks: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UpsertOutcome {
    pub added: usize,
    pub updated: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub playlists: usize,
    pub tracks: usize,
    pub files: usize,
    pub cached_bytes: u64,
    pub vip_only: usize,
}

impl Index {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Index::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("读取索引失败: {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Index::default());
        }
        let idx: Index = serde_json::from_str(&raw).with_context(|| {
            format!(
                "索引 JSON 解析失败，文件可能被中断写入: {}",
                path.display()
            )
        })?;
        Ok(idx)
    }

    /// 先写临时文件再改名，保证任何时刻磁盘上的索引都是完整的。
    /// 掉电或进程被杀也不会留下半截 JSON。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建索引目录失败: {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(self)?;
        fs::write(&tmp, &body).with_context(|| format!("写入索引失败: {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("替换索引失败: {}", path.display()))?;
        Ok(())
    }

    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }

    /// 把一次扫描的结果并入索引。同一首歌出现在多张歌单时只存一份。
    pub fn upsert_playlist(&mut self, playlist: Playlist, tracks: Vec<Track>) -> UpsertOutcome {
        let key = playlist.key();
        let total = tracks.len();
        let mut added = 0usize;
        let mut updated = 0usize;

        for track in tracks {
            let tk = track.key();
            match self.tracks.get(&tk) {
                Some(prev) => {
                    // 名称/专辑变了说明源站改过信息，算一次更新
                    if prev.name != track.name || prev.album != track.album {
                        updated += 1;
                    }
                    // 保留上一次探测到的可播放状态，避免每次扫描都重新付出探测成本
                    let mut merged = track;
                    merged.playable = prev.playable;
                    self.tracks.insert(tk, merged);
                }
                None => {
                    added += 1;
                    self.tracks.insert(tk, track);
                }
            }
        }

        self.playlists.insert(key, playlist);
        self.touch();
        UpsertOutcome {
            added,
            updated,
            total,
        }
    }

    pub fn playlist(&self, key: &str) -> Option<&Playlist> {
        self.playlists.get(key)
    }

    /// 按完整 key（`netease:123`）取曲目。
    /// 目前只有 Linux 出口的按需取回会用到，但索引层本身是跨平台的。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn track(&self, key: &str) -> Option<&Track> {
        self.tracks.get(key)
    }

    pub fn playlists(&self) -> impl Iterator<Item = &Playlist> {
        self.playlists.values()
    }

    /// 按歌单内的原始顺序取出曲目，索引里缺的跳过而不是报错。
    pub fn playlist_tracks(&self, key: &str) -> Vec<Track> {
        let Some(pl) = self.playlists.get(key) else {
            return Vec::new();
        };
        pl.track_ids
            .iter()
            .filter_map(|id| self.tracks.get(&format!("{}:{}", pl.source, id)).cloned())
            .collect()
    }

    /// 这首歌所属的第一张歌单，用来决定它在音乐库里落在哪个目录。
    ///
    /// 收进索引层而不是留在调用方，是因为命令行和 Web 界面都要拿它算目录。
    /// 两边各写一份 `find(|p| p.track_ids.contains(&id))`，迟早会有一边漏掉
    /// 「同一首歌在两张歌单里」这种情形，然后同一个文件在磁盘上出现两份。
    pub fn owning_playlist(&self, track_id: u64) -> Option<&Playlist> {
        self.playlists
            .values()
            .find(|p| p.track_ids.contains(&track_id))
    }

    /// 记下一首**不属于任何歌单**的曲目的元数据（搜索结果、歌手热门那一类）。
    ///
    /// 只在索引里还没有它时写入——已经属于某张歌单的曲目不该被覆盖。
    /// 没有这一步，点播一首搜到的歌之后它不会出现在 `info` 的统计里，
    /// FUSE 的虚拟树里也看不到它。
    pub fn remember_track(&mut self, track: Track) {
        self.tracks.entry(track.key()).or_insert(track);
        self.touch();
    }

    /// 命令行里允许直接写数字 id，或写 `netease:123` 这样的完整 key。
    pub fn resolve_track(&self, query: &str) -> Option<&Track> {
        let q = query.trim();
        if self.tracks.contains_key(q) {
            return self.tracks.get(q);
        }
        for source in ["netease", "qq"] {
            let key = format!("{source}:{q}");
            if let Some(t) = self.tracks.get(&key) {
                return Some(t);
            }
        }
        // 退一步：按名字精确匹配，方便手动点播
        self.tracks.values().find(|t| t.name == q)
    }

    pub fn record_file(&mut self, file: CachedFile) {
        self.tracks.entry(file.track.clone()).and_modify(|t| {
            t.playable = Some(true);
        });
        self.files.insert(file.track.clone(), file);
        self.touch();
    }

    pub fn file_of(&self, track_key: &str) -> Option<&CachedFile> {
        self.files.get(track_key)
    }

    pub fn stats(&self) -> Stats {
        Stats {
            playlists: self.playlists.len(),
            tracks: self.tracks.len(),
            files: self.files.len(),
            cached_bytes: self.files.values().map(|f| f.bytes).sum(),
            vip_only: self.tracks.values().filter(|t| t.vip_only()).count(),
        }
    }
}

/// 歌单目录名与索引 key 都靠它，集中在这里免得两处写法漂移。
pub fn playlist_key_for(source: &str, id: u64) -> String {
    format!("{source}:{id}")
}
