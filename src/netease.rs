//! 网易云音源客户端。
//!
//! 这里只做 HTTP 调用与响应归一化，不掺业务逻辑。
//!
//! 两种接入模式走同一套方法，只在两处有差异：
//! - 取播放链接的路径不同（legacy 的 `enhance/player/url` vs 自建服务的 `song/url/v1`）
//! - 自建服务可以带 cookie 拿到无损/解灰结果
//!
//! 未登录时接口对会员曲目返回 `code = -110`，这是正常现象而不是程序错误，
//! 所以这里把「取不到链接」当成一种结果而非异常返回，由上层决定怎么呈现。

use std::io::Read;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::config::{ApiMode, Config, Quality};
use crate::model::{AudioInfo, Playlist, Track, now_secs};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
/// 曲目元数据批量查询时每批的数量。
const DETAIL_BATCH: usize = 50;

pub struct NeteaseClient {
    agent: ureq::Agent,
    mode: ApiMode,
    cookie: Option<String>,
    /// 上一次请求的时刻，用于限速
    last_call: Mutex<Instant>,
    request_count: Mutex<u64>,
}

/// 播放链接的探测结果。拿不到不是错误，所以不放进 Result 的错误侧。
#[derive(Debug, Clone)]
pub struct UrlOutcome {
    pub info: Option<AudioInfo>,
    pub code: i32,
    pub reason: String,
}

impl UrlOutcome {
    pub fn ok(&self) -> bool {
        self.info.is_some()
    }

    /// 这类结果值得提示用户去配 cookie。
    pub fn needs_login(&self) -> bool {
        matches!(self.code, -110 | 200) && self.info.is_none()
    }
}

impl NeteaseClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        Ok(NeteaseClient {
            agent,
            mode: cfg.api.clone(),
            cookie: cfg.cookie_header(),
            last_call: Mutex::new(Instant::now() - Duration::from_secs(1)),
            request_count: Mutex::new(0),
        })
    }

    pub fn logged_in(&self) -> bool {
        self.cookie
            .as_ref()
            .map(|c| c.contains("MUSIC_U="))
            .unwrap_or(false)
    }

    pub fn requests_made(&self) -> u64 {
        *self.request_count.lock().unwrap()
    }

    /// 接口没做速率限制，但打太快会被风控。这里压到最低 300ms 一次。
    fn throttle(&self) {
        let mut last = self.last_call.lock().unwrap();
        let gap = Duration::from_millis(300);
        let elapsed = last.elapsed();
        if elapsed < gap {
            thread::sleep(gap - elapsed);
        }
        *last = Instant::now();
        drop(last);
        if let Ok(mut n) = self.request_count.lock() {
            *n += 1;
        }
    }

    fn url(&self, path: &str, query: &str) -> String {
        let base = self.mode.prefix();
        if query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query}")
        }
    }

    /// 发一次 GET 并读出文本体。
    /// 用 `as_reader` 而不是 `read_to_string`：后者有 10MB 上限，
    /// 大歌单的 JSON 会直接爆掉。
    fn get_text(&self, url: &str) -> Result<String> {
        self.throttle();
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Referer", "https://music.163.com/");
        if let Some(cookie) = &self.cookie {
            req = req.header("Cookie", cookie);
        }
        let mut resp = req
            .call()
            .with_context(|| format!("请求失败: {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("接口返回 HTTP {}", status.as_u16()));
        }
        let mut body = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .context("读取响应体失败")?;
        Ok(body)
    }

    /// 下载任意 URL 的原始字节，用于音频与封面。
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.throttle();
        let mut req = self.agent.get(url).header("User-Agent", UA);
        if url.contains("music.126.net") || url.contains("music.163.com") {
            req = req.header("Referer", "https://music.163.com/");
        }
        let mut resp = req.call().with_context(|| format!("下载失败: {url}"))?;
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut buf)
            .context("读取下载流失败")?;
        if buf.is_empty() {
            return Err(anyhow!("下载内容为空: {url}"));
        }
        Ok(buf)
    }

    /// 把音频下载到指定文件（流式，不占内存）。
    pub fn download_to(&self, url: &str, dest: &std::path::Path) -> Result<u64> {
        self.throttle();
        let mut req = self.agent.get(url).header("User-Agent", UA);
        if url.contains("music.126.net") || url.contains("music.163.com") {
            req = req.header("Referer", "https://music.163.com/");
        }
        let mut resp = req.call().with_context(|| format!("下载失败: {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("下载返回 HTTP {}", resp.status().as_u16()));
        }
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("创建文件失败: {}", dest.display()))?;
        let written = std::io::copy(&mut resp.body_mut().as_reader(), &mut file)
            .context("写入音频数据失败")?;
        Ok(written)
    }

    // ---------- 歌单 ----------

    pub fn playlist_detail(&self, id: u64) -> Result<(Playlist, Vec<Track>)> {
        let url = self.url("/playlist/detail", &format!("id={id}"));
        let text = self.get_text(&url)?;
        let env: PlaylistEnvelope =
            serde_json::from_str(&text).context("歌单响应解析失败，接口结构可能变了")?;
        let raw = env
            .pick()
            .ok_or_else(|| anyhow!("歌单 {id} 不存在或不可见（响应里没有 result/playlist）"))?;

        let mut tracks: Vec<Track> = raw
            .tracks
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter(|t| t.id > 0)
            .map(RawTrack::into_track)
            .collect();

        // 有些歌单只给 trackIds 不给曲目详情，需要补一次批量查询
        let mut declared_ids: Vec<u64> = Vec::new();
        if let Some(ids) = &raw.track_ids {
            declared_ids = ids.iter().filter(|t| t.id > 0).map(|t| t.id).collect();
        }
        if tracks.is_empty() && !declared_ids.is_empty() {
            tracks = self.songs_detail(&declared_ids)?;
        }

        // 顺序以 trackIds 为准，接口给详情的顺序不一定可靠
        let order: Vec<u64> = if declared_ids.is_empty() {
            tracks.iter().map(|t| t.id).collect()
        } else {
            declared_ids
        };
        let mut by_id: std::collections::HashMap<u64, Track> =
            tracks.into_iter().map(|t| (t.id, t)).collect();

        // 详情里缺的曲目补回来
        let missing: Vec<u64> = order
            .iter()
            .copied()
            .filter(|id| !by_id.contains_key(id))
            .collect();
        if !missing.is_empty() {
            for t in self.songs_detail(&missing)? {
                by_id.insert(t.id, t);
            }
        }

        let ordered: Vec<Track> = order
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect();

        let declared_count = if raw.track_count > 0 {
            raw.track_count
        } else {
            ordered.len()
        };

        let playlist = Playlist {
            source: "netease".to_string(),
            id: raw.id.max(id),
            name: if raw.name.is_empty() {
                format!("歌单 {id}")
            } else {
                raw.name
            },
            creator: raw
                .creator
                .map(|c| c.nickname)
                .unwrap_or_else(|| "未知".to_string()),
            cover_url: raw.cover_img_url,
            declared_count,
            scanned_at: now_secs(),
            track_ids: ordered.iter().map(|t| t.id).collect(),
        };

        Ok((playlist, ordered))
    }

    /// 批量取曲目元数据。
    pub fn songs_detail(&self, ids: &[u64]) -> Result<Vec<Track>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(DETAIL_BATCH) {
            let list = chunk
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let url = self.url("/song/detail", &format!("ids=%5B{list}%5D"));
            let text = self.get_text(&url)?;
            let env: SongsEnvelope =
                serde_json::from_str(&text).context("曲目详情响应解析失败")?;
            out.extend(
                env.songs
                    .into_iter()
                    .flatten()
                    .filter(|t| t.id > 0)
                    .map(RawTrack::into_track),
            );
        }
        Ok(out)
    }

    // ---------- 播放链接 ----------

    /// 取播放链接。单个档位一次请求，拿不到由上层走降级链。
    pub fn song_url(&self, id: u64, quality: Quality) -> Result<UrlOutcome> {
        let query = format!(
            "ids=%5B{id}%5D&br={}&level={}&encodeType={}",
            quality.br(),
            quality.as_str(),
            if quality == Quality::Lossless {
                "flac"
            } else {
                "mp3"
            }
        );
        let url = self.url(self.mode.url_path(), &query);
        let text = self.get_text(&url)?;
        let env: UrlEnvelope = serde_json::from_str(&text).context("播放链接响应解析失败")?;

        let entry = env.data.into_iter().flatten().next();
        let Some(entry) = entry else {
            return Ok(UrlOutcome {
                info: None,
                code: env.code,
                reason: "接口未返回该曲目的链接信息".to_string(),
            });
        };

        let raw_url = entry.url.unwrap_or_default();
        if raw_url.is_empty() {
            let reason = match entry.code {
                -110 => "需要登录态（会员或数字专辑曲目）".to_string(),
                -447 => "需要登录态：接口触发了风控，请降低频率或配置 cookie".to_string(),
                0 | 200 => "暂无版权或已下架".to_string(),
                other => format!("接口返回 code={other}"),
            };
            return Ok(UrlOutcome {
                info: None,
                code: entry.code,
                reason,
            });
        }

        let ext = if entry.file_type.is_empty() {
            if raw_url.ends_with(".flac") {
                "flac".to_string()
            } else {
                "mp3".to_string()
            }
        } else {
            entry.file_type.clone()
        };

        Ok(UrlOutcome {
            code: entry.code,
            reason: String::new(),
            info: Some(AudioInfo {
                quality: quality.as_str().to_string(),
                level: entry.level.unwrap_or_default(),
                br: entry.br,
                size: entry.size,
                ext,
                url: raw_url,
                md5: entry.md5.unwrap_or_default(),
            }),
        })
    }

    /// 按降级链依次尝试，返回第一个可用的链接。
    /// 返回值第二项是每次尝试的失败原因，便于提示用户。
    pub fn resolve_url(&self, id: u64, quality: Quality) -> Result<(UrlOutcome, Vec<String>)> {
        let mut notes = Vec::new();
        let mut last_code = 0;
        for q in quality.ladder_from() {
            let outcome = self.song_url(id, q)?;
            if outcome.ok() {
                return Ok((outcome, notes));
            }
            last_code = outcome.code;
            notes.push(format!("{}: {}", q.as_str(), outcome.reason));
        }
        Ok((
            UrlOutcome {
                info: None,
                code: last_code,
                reason: notes
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "所有档位都拿不到链接".to_string()),
            },
            notes,
        ))
    }

    // ---------- 歌词 ----------

    pub fn lyric(&self, id: u64) -> Result<Option<String>> {
        let url = self.url("/song/lyric", &format!("id={id}&lv=-1&kv=-1&tv=-1"));
        let text = self.get_text(&url)?;
        let env: LyricEnvelope = serde_json::from_str(&text).context("歌词响应解析失败")?;
        let lyric = env.lrc.and_then(|l| l.lyric).unwrap_or_default();
        let trimmed = lyric.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(lyric))
    }
}

// ---------- 原始响应结构 ----------

/// 网易云同一个字段的类型并不稳定：`no` 有时是数字 3，有时是字符串 "03"，
/// `duration` / `fee` 也会漂移。这里统一做宽松解析，
/// 否则一个字段的类型变化就能让整张歌单扫描失败。
mod flex {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    fn to_u64(value: Value) -> u64 {
        match value {
            Value::Number(n) => n
                .as_u64()
                .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
                .unwrap_or(0),
            Value::String(s) => {
                let t = s.trim();
                t.parse::<u64>()
                    .or_else(|_| t.parse::<f64>().map(|f| f.max(0.0) as u64))
                    .unwrap_or(0)
            }
            _ => 0,
        }
    }

    pub fn as_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(to_u64(Value::deserialize(deserializer)?))
    }

    pub fn as_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(to_u64(Value::deserialize(deserializer)?).min(u32::MAX as u64) as u32)
    }

    pub fn as_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(to_u64(Value::deserialize(deserializer)?) as usize)
    }

    pub fn as_i32<'de, D>(deserializer: D) -> Result<i32, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = match Value::deserialize(deserializer)? {
            Value::Number(n) => n.as_i64().unwrap_or(0),
            Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        Ok(raw as i32)
    }

    /// 文本字段也可能给 null（会员曲目的 `type` 就是 null），
    /// 统一归一成空串，别让一个 null 把整个响应判成解析失败。
    pub fn as_string<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => String::new(),
        })
    }

    /// 数组字段可能整个是 null，归一成空数组。
    pub fn as_vec_opt<'de, D, T>(deserializer: D) -> Result<Vec<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Option::<Vec<Option<T>>>::deserialize(deserializer)?.unwrap_or_default())
    }
}

#[derive(Debug, Deserialize)]
struct PlaylistEnvelope {
    #[serde(default)]
    result: Option<RawPlaylist>,
    #[serde(default)]
    playlist: Option<RawPlaylist>,
}

impl PlaylistEnvelope {
    fn pick(self) -> Option<RawPlaylist> {
        self.result.or(self.playlist)
    }
}

#[derive(Debug, Deserialize)]
struct SongsEnvelope {
    #[serde(default, deserialize_with = "flex::as_vec_opt")]
    songs: Vec<Option<RawTrack>>,
}

#[derive(Debug, Deserialize)]
struct UrlEnvelope {
    #[serde(default, deserialize_with = "flex::as_vec_opt")]
    data: Vec<Option<RawUrlEntry>>,
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
}

#[derive(Debug, Deserialize)]
struct LyricEnvelope {
    #[serde(default)]
    lrc: Option<RawLyric>,
}

#[derive(Debug, Deserialize)]
struct RawLyric {
    #[serde(default)]
    lyric: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUrlEntry {
    #[serde(default)]
    url: Option<String>,
    #[serde(default, deserialize_with = "flex::as_u64")]
    br: u64,
    #[serde(default, deserialize_with = "flex::as_u64")]
    size: u64,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "flex::as_string")]
    file_type: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlaylist {
    #[serde(default, deserialize_with = "flex::as_u64")]
    id: u64,
    #[serde(default, deserialize_with = "flex::as_string")]
    name: String,
    #[serde(default)]
    creator: Option<RawCreator>,
    #[serde(default, rename = "coverImgUrl")]
    cover_img_url: Option<String>,
    #[serde(default, rename = "trackCount", deserialize_with = "flex::as_usize")]
    track_count: usize,
    #[serde(default)]
    tracks: Option<Vec<Option<RawTrack>>>,
    #[serde(default, rename = "trackIds")]
    track_ids: Option<Vec<RawTrackId>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTrackId {
    #[serde(default, deserialize_with = "flex::as_u64")]
    id: u64,
}

#[derive(Debug, Default, Deserialize)]
struct RawCreator {
    #[serde(default, deserialize_with = "flex::as_string")]
    nickname: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawTrack {
    #[serde(default, deserialize_with = "flex::as_u64")]
    id: u64,
    #[serde(default, deserialize_with = "flex::as_string")]
    name: String,
    #[serde(default)]
    artists: Vec<Option<RawArtist>>,
    #[serde(default)]
    album: Option<RawAlbum>,
    #[serde(default, deserialize_with = "flex::as_u64")]
    duration: u64,
    #[serde(default, deserialize_with = "flex::as_u32")]
    no: u32,
    #[serde(default, deserialize_with = "flex::as_u32")]
    disc: u32,
    #[serde(default, deserialize_with = "flex::as_i32")]
    fee: i32,
}

#[derive(Debug, Default, Deserialize)]
struct RawArtist {
    #[serde(default, deserialize_with = "flex::as_string")]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawAlbum {
    #[serde(default, deserialize_with = "flex::as_u64")]
    id: u64,
    #[serde(default, deserialize_with = "flex::as_string")]
    name: String,
    #[serde(default, rename = "picUrl")]
    pic_url: Option<String>,
}

impl RawTrack {
    fn into_track(self) -> Track {
        let album = self.album.unwrap_or_default();
        Track {
            source: "netease".to_string(),
            id: self.id,
            name: if self.name.is_empty() {
                format!("曲目 {}", self.id)
            } else {
                self.name
            },
            artists: self
                .artists
                .into_iter()
                .flatten()
                .map(|a| a.name)
                .filter(|n| !n.is_empty())
                .collect(),
            album: album.name,
            album_id: album.id,
            cover_url: album.pic_url,
            duration_ms: self.duration,
            track_no: self.no,
            disc: self.disc,
            fee: self.fee,
            playable: None,
        }
    }
}
