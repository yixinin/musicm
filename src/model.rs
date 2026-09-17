//! 跨音源统一模型。
//!
//! 网易云和 QQ 音乐的原始响应结构差别很大，扫描层只负责把各自的响应塞进这几个
//! 结构体，之后索引、命名、下载、打标全部只认这套模型。

use serde::{Deserialize, Serialize};

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一首曲目。字段命名与虚拟文件树的生成规则一一对应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Track {
    /// 音源标识：netease / qq
    pub source: String,
    pub id: u64,
    pub name: String,
    /// 可能有多位歌手（合唱、feat.）
    pub artists: Vec<String>,
    pub album: String,
    pub album_id: u64,
    #[serde(default)]
    pub cover_url: Option<String>,
    /// 时长，毫秒
    pub duration_ms: u64,
    /// 专辑内序号，0 表示接口没给
    pub track_no: u32,
    pub disc: u32,
    /// 网易云 fee：0 免费 / 1 会员 / 4 数字专辑 / 8 免费但高音质需会员
    pub fee: i32,
    /// 最近一次解析播放链接的结果。None = 还没探测过。
    #[serde(default)]
    pub playable: Option<bool>,
}

impl Track {
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }

    pub fn artist_line(&self) -> String {
        if self.artists.is_empty() {
            "未知歌手".to_string()
        } else {
            self.artists.join("/")
        }
    }

    pub fn duration_secs(&self) -> u64 {
        self.duration_ms / 1000
    }

    /// 会员 / 数字专辑曲目：匿名状态下取链接必然失败，扫描时就要能区分出来，
    /// 免得等到播放时才发现整张歌单一半不可用。
    pub fn vip_only(&self) -> bool {
        matches!(self.fee, 1 | 4)
    }

    pub fn duration_label(&self) -> String {
        let total = self.duration_secs();
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// 一张歌单的元信息 + 曲目顺序。曲目本身存在 `Index::tracks` 里，这里只存 id，
/// 避免同一首歌出现在多张歌单时被复制多份。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub source: String,
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub cover_url: Option<String>,
    /// 接口自报的曲目总数（可能大于本次实际拿到的数量）
    pub declared_count: usize,
    pub scanned_at: u64,
    pub track_ids: Vec<u64>,
}

impl Playlist {
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }
}

/// 一次成功的播放链接解析结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    /// 我们请求的档位名（lossless / exhigh / ...）
    pub quality: String,
    /// 服务端实际给的档位名。接口经常「请求无损、返回 320k mp3」，
    /// 所以这两个值必须分开记，否则日志会骗人。
    #[serde(default)]
    pub level: String,
    pub br: u64,
    pub size: u64,
    pub ext: String,
    pub url: String,
    #[serde(default)]
    pub md5: String,
}

impl AudioInfo {
    /// 服务端实际给的档位。
    pub fn actual_level(&self) -> String {
        if self.level.is_empty() {
            self.quality.clone()
        } else {
            self.level.clone()
        }
    }

    /// 给人看的档位描述，实际与请求不一致时会标出来。
    pub fn quality_label(&self) -> String {
        let actual = self.actual_level();
        if actual == self.quality {
            actual
        } else {
            format!("{actual}（请求 {}）", self.quality)
        }
    }

    pub fn kbps(&self) -> u64 {
        self.br / 1000
    }

    pub fn size_mb(&self) -> f64 {
        self.size as f64 / 1024.0 / 1024.0
    }
}

/// 已经落地到磁盘的曲目文件记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    pub track: String,
    pub path: String,
    pub bytes: u64,
    pub quality: String,
    pub ext: String,
    pub fetched_at: u64,
    pub cover_embedded: bool,
    pub lyrics_embedded: bool,
    #[serde(default)]
    pub lrc_path: Option<String>,
}

impl CachedFile {
    pub fn size_label(&self) -> String {
        let mb = self.bytes as f64 / 1024.0 / 1024.0;
        format!("{mb:.1} MB")
    }
}
