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
use crate::model::{AudioInfo, Playlist, Track, now_millis, now_secs};

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

/// 每日推荐里曲目来自哪个字段。
///
/// 存在的理由是「空结果也得能解释」：这个接口未登录时**不报错**，只回一个空列表，
/// 所以「今天没有推荐」和「接口结构变了」在 `Vec::is_empty()` 上长得一模一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyShape {
    /// 明文 `/api/` 路径的 `recommend` 字段（实测就是这个）
    Recommend,
    /// 加密 weapi / 新版自建服务的 `data.dailySongs` 字段
    DailySongs,
    /// 两个字段都不在响应里——接口结构变了，而不是「今天没有推荐」
    Missing,
}

impl DailyShape {
    pub fn label(self) -> &'static str {
        match self {
            DailyShape::Recommend => "recommend",
            DailyShape::DailySongs => "data.dailySongs",
            DailyShape::Missing => "没有 recommend / dailySongs 字段",
        }
    }
}

/// 每日推荐的取回结果。
#[derive(Debug, Clone)]
pub struct DailyOutcome {
    pub tracks: Vec<Track>,
    /// 接口的 `code`。**未登录时也是 200**，所以它只能配合「本地有没有存过 cookie」
    /// 才能解释成「未登录」还是「凭据失效」（同 `account` 的 `profile: null`）。
    pub code: i32,
    pub shape: DailyShape,
}

/// 账号信息。只取展示需要的字段。
#[derive(Debug, Clone)]
pub struct Account {
    pub uid: u64,
    pub nickname: String,
    /// 0 = 非会员；11 = 黑胶 VIP。取值随版本漂移，所以只原样展示不解读。
    pub vip_type: i32,
}

/// 列表类响应的「空」是哪一种。
///
/// 存在的理由和 [`DailyShape`] 一样：空结果也得能解释。实测（2026-09 直连 `/api/`）：
/// 搜索无命中时**数组字段整个消失**，只留一个 `xxxCount: 0`
/// （`{"result":{"playlistCount":0},"code":200}`）。所以「数组字段不在」不能直接
/// 判成接口改版——那会把「真没搜到」误报成结构变化，用户就会去改代码而不是换个关键词。
/// 判据是两样都缺才算改版。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListShape {
    /// 有内容
    Items,
    /// 真的没有（数组字段不在，但计数字段在）
    Empty,
    /// 数组字段和计数字段都不在：这个接口的解析已经不成立了
    Missing,
}

impl ListShape {
    pub fn label(self) -> &'static str {
        match self {
            ListShape::Items => "有内容",
            ListShape::Empty => "没有匹配",
            ListShape::Missing => "响应里没有可识别的结果字段",
        }
    }
}

/// 搜索对象。网易云用数字 `type` 区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Song,
    Playlist,
    Artist,
}

impl SearchKind {
    pub const ALL: [SearchKind; 3] = [SearchKind::Song, SearchKind::Playlist, SearchKind::Artist];

    pub fn type_code(self) -> u32 {
        match self {
            SearchKind::Song => 1,
            SearchKind::Playlist => 1000,
            SearchKind::Artist => 100,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SearchKind::Song => "单曲",
            SearchKind::Playlist => "歌单",
            SearchKind::Artist => "歌手",
        }
    }
}

/// 一次搜索的结果。`total` 是接口自报的命中总数，通常远大于本次取回的条数。
#[derive(Debug, Clone)]
pub struct SearchOutcome<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub code: i32,
    pub shape: ListShape,
}

/// 列表里的一条歌单。曲目要 `scan` 完才有，所以这里只有元信息。
#[derive(Debug, Clone)]
pub struct PlaylistBrief {
    pub id: u64,
    pub name: String,
    pub creator: String,
    pub track_count: usize,
    /// 5 = 我喜欢的音乐（每个账号里都有这么一个特殊歌单）
    pub special_type: i32,
    /// 0 公开 / 10 私密
    pub privacy: i32,
    /// 我是否收藏了它。**匿名时是 `None`**——接口给的是 `null`（不知道），
    /// 用 `bool` 接就会把「不知道」说成「没收藏」。
    pub subscribed: Option<bool>,
}

impl PlaylistBrief {
    /// 与 `store::playlist_key_for` 同构。这里不调它，是为了让音源层只依赖
    /// config / model，不需要知道索引的存在。
    pub fn key(&self) -> String {
        format!("netease:{}", self.id)
    }

    pub fn is_favorite(&self) -> bool {
        self.special_type == 5
    }

    pub fn is_private(&self) -> bool {
        self.privacy != 0
    }
}

/// 搜索结果里的一位歌手。
#[derive(Debug, Clone)]
pub struct ArtistBrief {
    pub id: u64,
    pub name: String,
    /// 别名（周杰伦 → Jay Chou / 周董）
    pub alias: Vec<String>,
    pub song_count: usize,
    pub album_count: usize,
}

/// 某个账号的歌单列表。
#[derive(Debug, Clone)]
pub struct UserPlaylists {
    pub uid: u64,
    pub playlists: Vec<PlaylistBrief>,
    /// 接口是否还有下一页（`limit` 给小了就会是 true）
    pub more: bool,
    pub shape: ListShape,
    pub code: i32,
}

/// 扫码状态。网易云用 800~803 表示，此外还有若干**终止性**的错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrState {
    /// 801：还没人扫
    Waiting,
    /// 802：扫了，等手机端确认
    Scanned,
    /// 803：确认成功，凭据已下发
    Confirmed,
    /// 800：二维码过期
    Expired,
    /// 8821：风控要求「行为验证码验证」。
    ///
    /// 这是**终止态**：网易云判定这次登录请求不像官方客户端（或 IP/频率可疑），
    /// 要人过滑块/点选验证码才放行。第三方客户端拿不到这个验证码，
    /// 所以继续轮询不会有任何变化——只会白等满超时。
    /// 首见症状就是「扫了，App 显示已确认，但终端一直说继续等待」。
    Blocked,
    /// 响应里根本没有 `code`（自建服务内部出错时会回一个空对象 `{}`）。
    /// 这是**瞬时**状态，继续轮询是对的。
    Empty,
    /// 其它没见过的码。仍然继续轮询，但必须把原始 message 亮给用户，
    /// 否则接口一变，用户就只能干等到超时。
    Unrecognized,
}

impl QrState {
    pub fn from_code(code: i32) -> Self {
        match code {
            801 => QrState::Waiting,
            802 => QrState::Scanned,
            803 => QrState::Confirmed,
            800 => QrState::Expired,
            8821 => QrState::Blocked,
            0 => QrState::Empty,
            _ => QrState::Unrecognized,
        }
    }

    /// 是否该停止轮询。`Empty` 和 `Unrecognized` 都不算——它们还可能好转。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            QrState::Confirmed | QrState::Expired | QrState::Blocked
        )
    }
}

/// 一次扫码状态查询的结果。
#[derive(Debug, Clone)]
pub struct QrPoll {
    pub state: QrState,
    pub code: i32,
    pub message: String,
    /// 只在 `Confirmed` 时有值。
    pub cookie: Option<String>,
}

impl QrPoll {
    fn new(code: i32, message: String, cookie: Option<String>) -> Self {
        QrPoll {
            state: QrState::from_code(code),
            code,
            message,
            cookie,
        }
    }
}

/// 扫码内容。自建服务的 `/login/qr/create` 拼的就是这个串，所以两边共用。
pub fn login_qr_url(key: &str) -> String {
    format!("https://music.163.com/login?codekey={key}")
}

/// `Set-Cookie` 形如 `MUSIC_U=xxx; Path=/; Domain=.music.163.com; HttpOnly`，
/// 只保留第一段键值对。注意不能按 `;` 全切——后面那些属性也带 `=`。
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 错误信息里放一小段响应正文，够定位问题又不至于刷屏。
fn snippet(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = one_line.chars().take(160).collect();
    if one_line.chars().count() > 160 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 由「数组字段在不在 + 实际条数 + 计数字段」推出（命中总数, 形态）。
///
/// 条件是「两样都缺」才算接口改版，判据见 [`ListShape`]。
fn classify(list_present: bool, len: usize, count: Option<u64>) -> (usize, ListShape) {
    let declared = count.map(|c| c as usize).unwrap_or(len);
    if len > 0 {
        return (declared, ListShape::Items);
    }
    if list_present || count.is_some() {
        (declared, ListShape::Empty)
    } else {
        (0, ListShape::Missing)
    }
}

/// 查询参数里的值按 RFC 3986 转义。
///
/// 关键词里有中文、空格、`&`、`#`，直接拼进 URL 会把查询串切碎——
/// `s=AC/DC & friends` 里那个 `&` 会凭空多出一个参数，`#` 更是把它后面全丢掉。
/// 只手写这几行而不引第三方 crate：目标机是 aarch64 NAS，为这点事多一个依赖不划算。
fn encode_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 3);
    for b in raw.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    /// 发一次 POST 表单，返回（响应体, 所有 Set-Cookie）。
    ///
    /// 登录相关接口都是 POST 表单。`Set-Cookie` 必须单独取出来：
    /// 扫码成功的凭据就在里面，不在响应体里。
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(String, Vec<String>)> {
        self.throttle();
        let mut req = self
            .agent
            .post(url)
            .header("User-Agent", UA)
            .header("Referer", "https://music.163.com/");
        if let Some(cookie) = &self.cookie {
            req = req.header("Cookie", cookie);
        }
        let mut resp = req
            .send_form(form.iter().copied())
            .with_context(|| format!("请求失败: {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("接口返回 HTTP {}", status.as_u16()));
        }
        // 先把响应头收完（不可变借用结束），再去拿 body（需要可变借用）
        let cookies: Vec<String> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .collect();

        let mut body = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .context("读取响应体失败")?;
        Ok((body, cookies))
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
        // 两种服务认的参数不一样：老接口认 br + encodeType，自建服务只认 level。
        // 参数名也不同（`ids` vs `id`），所以不能共用一套。
        let query = match &self.mode {
            ApiMode::Direct => format!(
                "ids=%5B{id}%5D&br={}&level={}&encodeType={}",
                quality.br(),
                quality.as_str(),
                if quality == Quality::Lossless {
                    "flac"
                } else {
                    "mp3"
                }
            ),
            ApiMode::Sidecar { .. } => format!("id={id}&level={}", quality.as_str()),
        };
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
                // 同一个 -110 有两种成因，提示要分开——否则用户会去重装 cookie 却发现没用
                -110 if self.logged_in() => {
                    "需要登录态：cookie 可能已失效（用 `musicm whoami` 确认），\
                     或该曲目需要更高等级会员 / 数字专辑"
                        .to_string()
                }
                -110 => "需要登录态（会员或数字专辑曲目）：用 `musicm login` 配置 cookie".to_string(),
                -447 => "接口触发风控：降低频率，或换一个有效的 cookie".to_string(),
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

    // ---------- 搜索 ----------

    /// 拼一次搜索请求。两种接入方式的参数名不一样，所以不能共用一套。
    fn search_raw(
        &self,
        kind: SearchKind,
        keyword: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String> {
        let kw = encode_query(keyword);
        let type_code = kind.type_code();
        let (path, query) = match &self.mode {
            ApiMode::Direct => (
                "/search/get/web".to_string(),
                format!("s={kw}&type={type_code}&offset={offset}&limit={limit}"),
            ),
            // 自建服务的 `/search` 认 `keywords`，老路径认 `s`；不同 fork 只实现其中一个，
            // 所以两个都带上——多给一个参数无害，少给一个就是永远搜不到。
            ApiMode::Sidecar { .. } => (
                "/search".to_string(),
                format!("s={kw}&keywords={kw}&type={type_code}&offset={offset}&limit={limit}"),
            ),
        };
        self.get_text(&self.url(&path, &query))
    }

    /// 搜索单曲。
    ///
    /// ⚠️ 搜索接口**不返回专辑封面**（专辑对象里只有 `picId`，没有 `picUrl`），
    /// 所以拿搜索结果直接落地会丢封面，而飞牛的刮削就靠它。
    /// 要落地请先用 [`NeteaseClient::songs_detail`] 补一次元数据。
    pub fn search_songs(
        &self,
        keyword: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SearchOutcome<Track>> {
        let text = self.search_raw(SearchKind::Song, keyword, offset, limit)?;
        let env: SearchEnvelope<SongSearchResult> =
            serde_json::from_str(&text).context("搜索响应解析失败，接口结构可能变了")?;
        let (code, result) = env.pick();
        let Some(result) = result else {
            return Ok(SearchOutcome {
                items: Vec::new(),
                total: 0,
                code,
                shape: ListShape::Missing,
            });
        };
        let present = result.songs.is_present();
        let items: Vec<Track> = result
            .songs
            .into_vec()
            .into_iter()
            .flatten()
            .filter(|t| t.id > 0)
            .map(RawTrack::into_track)
            .collect();
        let (total, shape) = classify(present, items.len(), result.song_count);
        Ok(SearchOutcome {
            items,
            total,
            code,
            shape,
        })
    }

    /// 搜索歌单。返回的 id 直接可以喂给 `musicm scan`。
    pub fn search_playlists(
        &self,
        keyword: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SearchOutcome<PlaylistBrief>> {
        let text = self.search_raw(SearchKind::Playlist, keyword, offset, limit)?;
        let env: SearchEnvelope<PlaylistSearchResult> =
            serde_json::from_str(&text).context("搜索响应解析失败，接口结构可能变了")?;
        let (code, result) = env.pick();
        let Some(result) = result else {
            return Ok(SearchOutcome {
                items: Vec::new(),
                total: 0,
                code,
                shape: ListShape::Missing,
            });
        };
        let present = result.playlists.is_present();
        let items: Vec<PlaylistBrief> = result
            .playlists
            .into_vec()
            .into_iter()
            .flatten()
            .filter(|p| p.id > 0)
            .map(RawPlaylist::into_brief)
            .collect();
        let (total, shape) = classify(present, items.len(), result.playlist_count);
        Ok(SearchOutcome {
            items,
            total,
            code,
            shape,
        })
    }

    /// 搜索歌手。
    pub fn search_artists(
        &self,
        keyword: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SearchOutcome<ArtistBrief>> {
        let text = self.search_raw(SearchKind::Artist, keyword, offset, limit)?;
        let env: SearchEnvelope<ArtistSearchResult> =
            serde_json::from_str(&text).context("搜索响应解析失败，接口结构可能变了")?;
        let (code, result) = env.pick();
        let Some(result) = result else {
            return Ok(SearchOutcome {
                items: Vec::new(),
                total: 0,
                code,
                shape: ListShape::Missing,
            });
        };
        let present = result.artists.is_present();
        let items: Vec<ArtistBrief> = result
            .artists
            .into_vec()
            .into_iter()
            .flatten()
            .filter(|a| a.id > 0)
            .map(RawSearchArtist::into_brief)
            .collect();
        let (total, shape) = classify(present, items.len(), result.artist_count);
        Ok(SearchOutcome {
            items,
            total,
            code,
            shape,
        })
    }

    // ---------- 账号歌单 / 歌手曲目 ----------

    /// 某个账号的歌单列表。
    ///
    /// 形状上有个坑：`playlist` 在**顶层**，不在 `result` 里——
    /// `{"code":200,"more":true,"playlist":[...]}`。照 `result.playlist` 写会永远读到空，
    /// 而且 `code` 还是 200，看起来一切正常。
    ///
    /// 匿名也能调（只看得到公开歌单），本人 + cookie 才拿得到私密歌单。
    /// 这个差别由上层提示，这里不猜。
    pub fn user_playlists(&self, uid: u64, limit: usize, offset: usize) -> Result<UserPlaylists> {
        let query = match &self.mode {
            ApiMode::Direct => format!("uid={uid}&limit={limit}&offset={offset}"),
            // 自建服务按 URL 做 2 分钟缓存，而歌单是会变的，得带时间戳打破
            ApiMode::Sidecar { .. } => format!(
                "uid={uid}&limit={limit}&offset={offset}&timestamp={}",
                now_millis()
            ),
        };
        let url = self.url("/user/playlist", &query);
        let text = self.get_text(&url)?;
        let env: UserPlaylistEnvelope =
            serde_json::from_str(&text).context("账号歌单响应解析失败，接口结构可能变了")?;

        let present = env.playlist.is_present();
        let playlists: Vec<PlaylistBrief> = env
            .playlist
            .into_vec()
            .into_iter()
            .flatten()
            .filter(|p| p.id > 0)
            .map(RawPlaylist::into_brief)
            .collect();
        // 这个接口没有计数字段，「空」的判据就只剩数组字段在不在
        let (_, shape) = classify(present, playlists.len(), None);
        Ok(UserPlaylists {
            uid,
            playlists,
            more: env.more,
            shape,
            code: env.code,
        })
    }

    /// 歌手的热门曲目。
    ///
    /// `limit` 参数**不生效**（实测 `limit=5` 照样返回 50 首），截断只能自己做。
    pub fn artist_top_songs(&self, artist_id: u64) -> Result<(i32, Vec<Track>)> {
        let url = self.url("/artist/top/song", &format!("id={artist_id}"));
        let text = self.get_text(&url)?;
        let env: TopSongEnvelope =
            serde_json::from_str(&text).context("歌手曲目响应解析失败，接口结构可能变了")?;
        Ok((
            env.code,
            env.songs
                .into_iter()
                .flatten()
                .filter(|t| t.id > 0)
                .map(RawTrack::into_track)
                .collect(),
        ))
    }

    // ---------- 登录 ----------

    /// 查账号信息。
    ///
    /// 返回 `None` 表示当前 cookie **不构成有效登录**。这个判断只能联网得到：
    /// 网易云对匿名访问和失效 cookie 都返回 `code:200` + `profile:null`，
    /// 不会给出「凭据过期」这种错误码，本地看不出来。
    pub fn account(&self) -> Result<Option<Account>> {
        let path = match &self.mode {
            ApiMode::Direct => "/nuser/account/get",
            ApiMode::Sidecar { .. } => "/login/status",
        };
        let url = self.url(path, &format!("timestamp={}", now_millis()));
        let text = self.get_text(&url)?;
        let env: AccountEnvelope =
            serde_json::from_str(&text).context("账号信息响应解析失败，接口结构可能变了")?;
        Ok(env.pick())
    }

    /// 申请一个扫码登录用的 key，返回（key, 二维码内容）。
    ///
    /// 二维码内容两端都是自己拼的：自建服务的 `/login/qr/create` 做的就是同一件事
    /// （`https://music.163.com/login?codekey=<key>`），少一次网络往返就少一个失败点。
    pub fn qr_key(&self) -> Result<(String, String)> {
        let text = match &self.mode {
            ApiMode::Direct => {
                let url = self.url("/login/qrcode/unikey", "");
                self.post_form(&url, &[("type", "3")])?.0
            }
            ApiMode::Sidecar { .. } => {
                // 自建服务会按 2 分钟粒度缓存响应，必须带 timestamp 打破缓存
                let url = self.url(
                    "/login/qr/key",
                    &format!("timestamp={}", now_millis()),
                );
                self.get_text(&url)?
            }
        };
        let env: QrKeyEnvelope =
            serde_json::from_str(&text).context("扫码 key 响应解析失败")?;
        let key = env.pick().ok_or_else(|| {
            anyhow!("接口没返回扫码 key，响应片段: {}", snippet(&text))
        })?;
        let url = login_qr_url(&key);
        Ok((key, url))
    }

    /// 轮询扫码状态。调用方自己控制节奏与超时。
    pub fn qr_poll(&self, key: &str) -> Result<QrPoll> {
        let stamp = now_millis();
        match &self.mode {
            ApiMode::Direct => {
                let url = self.url(
                    "/login/qrcode/client/login",
                    &format!("timestamp={stamp}"),
                );
                let (body, cookies) = self.post_form(&url, &[("key", key), ("type", "3")])?;
                let env: QrPollEnvelope =
                    serde_json::from_str(&body).context("扫码状态响应解析失败")?;
                // 成功时凭据在 Set-Cookie 里（不是响应体里）。把每条 cookie 的属性
                // 剥掉后拼起来——`__csrf` 也有用，不能只留 MUSIC_U。
                // 个别版本的接口会在 body 里再给一份，也认。
                let from_headers = cookies
                    .iter()
                    .map(|c| cookie_pair(c))
                    .filter(|c| !c.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                let cookie = if from_headers.contains("MUSIC_U=") {
                    Some(from_headers)
                } else {
                    env.cookie.clone()
                }
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
                Ok(QrPoll::new(env.code, env.message, cookie))
            }
            ApiMode::Sidecar { .. } => {
                let url = self.url(
                    "/login/qr/check",
                    &format!("key={key}&timestamp={stamp}"),
                );
                let body = self.get_text(&url)?;
                // 自建服务内部出错时会回一个空对象，当作「还没结果」继续轮询，
                // 而不是把一次瞬时失败当作登录失败。
                let env: QrPollEnvelope = serde_json::from_str(&body).unwrap_or_default();
                Ok(QrPoll::new(env.code, env.message, env.cookie))
            }
        }
    }

    // ---------- 推荐 ----------

    /// 取「每日推荐」曲目。
    ///
    /// ⚠️ 这个接口的失败方式是「静默空数组」：没登录和 cookie 已失效都返回
    /// `code = 200` + 空列表，**不会给任何错误码**。所以这里只负责把
    /// （曲目、code、命中字段）原样交出去，由上层决定怎么解释——
    /// 如果这里把空列表直接翻译成「今天没有推荐」，用户会以为是自己账号的问题。
    ///
    /// 另一个坑是响应形状有两套，见 [`DailyEnvelope::pick`]。
    pub fn daily_songs(&self) -> Result<DailyOutcome> {
        let (path, query) = match &self.mode {
            ApiMode::Direct => ("/v1/discovery/recommend/songs", String::new()),
            // 自建服务按 URL 做 2 分钟缓存，不带时间戳会一直读到同一份
            ApiMode::Sidecar { .. } => (
                "/recommend/songs",
                format!("timestamp={}", now_millis()),
            ),
        };
        let url = self.url(path, &query);
        let text = self.get_text(&url)?;
        let env: DailyEnvelope =
            serde_json::from_str(&text).context("每日推荐响应解析失败，接口结构可能变了")?;
        let (code, raw, shape) = env.pick();

        Ok(DailyOutcome {
            tracks: raw
                .into_iter()
                .filter(|t| t.id > 0)
                .map(RawTrack::into_track)
                .collect(),
            code,
            shape,
        })
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

    /// 计数类字段：**必须能区分「值是 0」和「字段不在」**。
    ///
    /// 搜索无命中时网易云只给一个 `playlistCount: 0`，数组字段整个消失
    /// （实测 `{"result":{"playlistCount":0},"code":200}`）。那个 `0` 正是
    /// 「真没搜到」的证据，用 `#[serde(default)]` 的 0 去顶替它就把证据弄丢了。
    pub fn as_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Value::deserialize(deserializer)? {
            Value::Null => None,
            other => Some(to_u64(other)),
        })
    }

    /// 布尔字段可能是 `true` / `"true"` / `1`，也可能是 `null`。
    /// `null` 是「接口也不知道」，与 `false` 不是一回事——歌单的收藏状态就是这个形状。
    pub fn as_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Value::deserialize(deserializer)? {
            Value::Bool(b) => Some(b),
            Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        })
    }

    pub fn as_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(as_opt_bool(deserializer)?.unwrap_or(false))
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

    /// 数组字段的三种状态。
    ///
    /// 别用 `Vec` + `#[serde(default)]` 代替它：那样「字段是空数组」和「字段整个不在」
    /// 会塌成同一个值，而每日推荐恰恰只有这个区别能说明问题——
    /// 前者是「未登录」，后者是「接口改版」。同 `QrState` 把 `8821` 和空响应
    /// 分开的理由一样：两种成因不同的失败共用一个状态，就只能给出错误的提示。
    #[derive(Debug, Default)]
    pub enum MaybeList<T> {
        /// 字段不在响应里
        #[default]
        Missing,
        /// 字段在，但是 `null` 或 `[]`
        Empty,
        /// 字段在，且有元素（元素本身仍可能是 `null`，那是下架曲目的占位）
        Items(Vec<Option<T>>),
    }

    pub fn as_maybe_list<'de, D, T>(deserializer: D) -> Result<MaybeList<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        // 键缺失时 serde 不会调用这里，直接落到 `Default::default()` 的 Missing 上
        Ok(match Option::<Vec<Option<T>>>::deserialize(deserializer)? {
            Some(items) if !items.is_empty() => MaybeList::Items(items),
            _ => MaybeList::Empty,
        })
    }

    impl<T> MaybeList<T> {
        /// 数组字段是否出现在响应里——空数组和 `null` 都算「在」。
        pub fn is_present(&self) -> bool {
            !matches!(self, MaybeList::Missing)
        }

        /// 取出元素。`Missing` 和 `Empty` 都给空向量，
        /// 要区分「是哪种空」请先问 [`MaybeList::is_present`]。
        pub fn into_vec(self) -> Vec<Option<T>> {
            match self {
                MaybeList::Items(items) => items,
                _ => Vec::new(),
            }
        }
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

/// 搜索响应。结果体可能在 `result` 里，也可能被 `data` 再包一层
/// （自建服务 / 新版接口就是后一种），所以往下一层层找。
#[derive(Debug, Deserialize)]
struct SearchEnvelope<T> {
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    data: Option<Box<SearchEnvelope<T>>>,
}

impl<T> SearchEnvelope<T> {
    /// 返回（接口 code, 结果体）。
    ///
    /// 结果体不在时也要把 `code` 带出去：「没有 result」和「code 不是 200」
    /// 是两种需要分开解释的情况，混成一句「没有结果」用户就没法排查。
    fn pick(self) -> (i32, Option<T>) {
        if let Some(result) = self.result {
            return (self.code, Some(result));
        }
        match self.data {
            Some(inner) => inner.pick(),
            None => (self.code, None),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct SongSearchResult {
    #[serde(default, deserialize_with = "flex::as_maybe_list")]
    songs: flex::MaybeList<RawTrack>,
    /// 用 `Option` 接：搜索为空时数组字段会整个消失，只剩这个 `0`——
    /// 它正是「真没搜到」的唯一证据（见 [`ListShape`]）。
    #[serde(
        default,
        rename = "songCount",
        deserialize_with = "flex::as_opt_u64"
    )]
    song_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct PlaylistSearchResult {
    #[serde(default, deserialize_with = "flex::as_maybe_list")]
    playlists: flex::MaybeList<RawPlaylist>,
    #[serde(
        default,
        rename = "playlistCount",
        deserialize_with = "flex::as_opt_u64"
    )]
    playlist_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ArtistSearchResult {
    #[serde(default, deserialize_with = "flex::as_maybe_list")]
    artists: flex::MaybeList<RawSearchArtist>,
    #[serde(
        default,
        rename = "artistCount",
        deserialize_with = "flex::as_opt_u64"
    )]
    artist_count: Option<u64>,
}

/// 账号歌单响应。**`playlist` 在顶层**，不在 `result` 里——
/// 照 `result.playlist` 写会永远读到空，而 `code` 还是 200，看不出任何异常。
#[derive(Debug, Default, Deserialize)]
struct UserPlaylistEnvelope {
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
    /// 是否还有下一页。小 `limit` 时会是 true。
    #[serde(default, deserialize_with = "flex::as_bool")]
    more: bool,
    #[serde(default, deserialize_with = "flex::as_maybe_list")]
    playlist: flex::MaybeList<RawPlaylist>,
}

/// `/artist/top/song` 的响应：`songs` 也在顶层，且是 App 风格字段名（`ar` / `al` / `dt`）。
#[derive(Debug, Default, Deserialize)]
struct TopSongEnvelope {
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
    #[serde(default, deserialize_with = "flex::as_vec_opt")]
    songs: Vec<Option<RawTrack>>,
}

/// 每日推荐的响应。**两套形状都要认**：
///
/// - 明文 `/api/` 路径：`{code, recommend: [...]}`（实测，直连模式走的就是这条）
/// - 加密 weapi / 新版自建服务：`{code, data: {dailySongs: [...]}}`
///
/// 只照一套写，换个接入方式就会静默变成「今日 0 首」——而这个接口本来就不会报错，
/// 所以那种失败连日志都看不出来。
#[derive(Debug, Default, Deserialize)]
struct DailyEnvelope {
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
    #[serde(default, deserialize_with = "flex::as_maybe_list")]
    recommend: flex::MaybeList<RawTrack>,
    #[serde(default, rename = "dailySongs", deserialize_with = "flex::as_maybe_list")]
    daily_songs: flex::MaybeList<RawTrack>,
    #[serde(default)]
    data: Option<Box<DailyEnvelope>>,
}

impl DailyEnvelope {
    /// 返回（code, 曲目, 命中字段）。
    ///
    /// 判据是「字段在不在」而不是「曲目多不多」：空数组是一个有效结果
    /// （未登录就是这个样子），字段整个缺失才说明接口结构变了。两者必须分得开，
    /// 否则「凭据失效」会被误报成「接口改版」，用户就会去改代码而不是重新登录。
    fn pick(self) -> (i32, Vec<RawTrack>, DailyShape) {
        let DailyEnvelope {
            code,
            recommend,
            daily_songs,
            data,
        } = self;

        let tracks = |items: Vec<Option<RawTrack>>| -> Vec<RawTrack> {
            items.into_iter().flatten().filter(|t| t.id > 0).collect()
        };

        match (daily_songs, recommend) {
            (flex::MaybeList::Items(items), _) => {
                (code, tracks(items), DailyShape::DailySongs)
            }
            (_, flex::MaybeList::Items(items)) => {
                (code, tracks(items), DailyShape::Recommend)
            }
            // 字段在但是空的（`null` 或 `[]`）：这才是「未登录 / 今天没有推荐」
            (flex::MaybeList::Empty, _) => (code, Vec::new(), DailyShape::DailySongs),
            (_, flex::MaybeList::Empty) => (code, Vec::new(), DailyShape::Recommend),
            // 两套字段都不在：自建服务会在外面再包一层 data
            (flex::MaybeList::Missing, flex::MaybeList::Missing) => match data {
                Some(inner) => inner.pick(),
                None => (code, Vec::new(), DailyShape::Missing),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct UrlEnvelope {
    #[serde(default, deserialize_with = "flex::as_vec_opt")]
    data: Vec<Option<RawUrlEntry>>,
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
}

/// 账号信息。两种模式的包裹层次不一样，都要认：
///
/// - 直连：`{code, account:{...}, profile:{...}}`
/// - 自建服务：`{code, data:{code, account:{...}, profile:{...}}}`
///
/// 另外 `profile` 为 `null` 是**正常返回**，表示当前 cookie 不构成登录
/// （匿名的和失效的 cookie 都长这样），所以这里不能当成错误。
#[derive(Debug, Default, Deserialize)]
struct AccountEnvelope {
    #[serde(default)]
    profile: Option<RawProfile>,
    #[serde(default)]
    data: Option<Box<AccountEnvelope>>,
}

impl AccountEnvelope {
    fn pick(self) -> Option<Account> {
        if let Some(profile) = self.profile {
            return Some(profile.into_account());
        }
        self.data.and_then(|inner| inner.pick())
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawProfile {
    #[serde(default, rename = "userId", deserialize_with = "flex::as_u64")]
    user_id: u64,
    #[serde(default, deserialize_with = "flex::as_string")]
    nickname: String,
    #[serde(default, rename = "vipType", deserialize_with = "flex::as_i32")]
    vip_type: i32,
}

impl RawProfile {
    fn into_account(self) -> Account {
        Account {
            uid: self.user_id,
            nickname: if self.nickname.is_empty() {
                format!("用户 {}", self.user_id)
            } else {
                self.nickname
            },
            vip_type: self.vip_type,
        }
    }
}

/// 扫码 key。`unikey` 可能出现在顶层，也可能在 `data` 里（自建服务再包一层）。
#[derive(Debug, Default, Deserialize)]
struct QrKeyEnvelope {
    #[serde(default, deserialize_with = "flex::as_string")]
    unikey: String,
    #[serde(default)]
    data: Option<Box<QrKeyEnvelope>>,
}

impl QrKeyEnvelope {
    fn pick(self) -> Option<String> {
        if !self.unikey.is_empty() {
            return Some(self.unikey);
        }
        self.data.and_then(|inner| inner.pick())
    }
}

/// 扫码状态。自建服务失败时会回一个空对象，`code` 缺省为 0，
/// 这时当作「继续轮询」而不是登录失败。
#[derive(Debug, Default, Deserialize)]
struct QrPollEnvelope {
    #[serde(default, deserialize_with = "flex::as_i32")]
    code: i32,
    #[serde(default, deserialize_with = "flex::as_string")]
    message: String,
    /// 自建服务会把 Set-Cookie 拼成这个字段
    #[serde(default)]
    cookie: Option<String>,
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
    /// 5 = 我喜欢的音乐；0 = 普通歌单
    #[serde(default, rename = "specialType", deserialize_with = "flex::as_i32")]
    special_type: i32,
    /// 0 公开 / 10 私密。搜索接口不给这个字段，那时是 0（= 当作公开）。
    #[serde(default, deserialize_with = "flex::as_i32")]
    privacy: i32,
    /// 我是否收藏了它。匿名时接口给 `null`（不知道），所以是 `Option` 而不是 `bool`：
    /// 用 `bool` 接会把「不知道」说成「没收藏」。
    #[serde(default, deserialize_with = "flex::as_opt_bool")]
    subscribed: Option<bool>,
}

impl RawPlaylist {
    /// 转成列表里的一行。曲目详情要 `scan` 才有，所以这里不带。
    fn into_brief(self) -> PlaylistBrief {
        PlaylistBrief {
            id: self.id,
            name: if self.name.is_empty() {
                format!("歌单 {}", self.id)
            } else {
                self.name
            },
            creator: self.creator.map(|c| c.nickname).unwrap_or_default(),
            track_count: self.track_count,
            special_type: self.special_type,
            privacy: self.privacy,
            subscribed: self.subscribed,
        }
    }
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
    /// App 风格的接口（每日推荐的新版 `dailySongs`）把歌手给在 `ar` 里，
    /// 歌单/详情接口给在 `artists` 里，是同一个东西的两种拼写。
    #[serde(default, alias = "ar")]
    artists: Vec<Option<RawArtist>>,
    #[serde(default, alias = "al")]
    album: Option<RawAlbum>,
    /// 同理：详情接口叫 `duration`，App 风格叫 `dt`。
    #[serde(default, alias = "dt", deserialize_with = "flex::as_u64")]
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
struct RawSearchArtist {
    #[serde(default, deserialize_with = "flex::as_u64")]
    id: u64,
    #[serde(default, deserialize_with = "flex::as_string")]
    name: String,
    /// 别名给在 `alias` 和 `alia` 两个键里（内容一样），取一个就够。
    /// 照旧按「数组可能是 null」处理。
    #[serde(default, deserialize_with = "flex::as_vec_opt")]
    alias: Vec<Option<String>>,
    /// 歌曲数。搜索接口叫 `musicSize`，**不是** `mvSize`（那是 MV 数），别拿错。
    #[serde(default, rename = "musicSize", deserialize_with = "flex::as_usize")]
    song_count: usize,
    #[serde(default, rename = "albumSize", deserialize_with = "flex::as_usize")]
    album_count: usize,
}

impl RawSearchArtist {
    fn into_brief(self) -> ArtistBrief {
        ArtistBrief {
            id: self.id,
            name: if self.name.is_empty() {
                format!("歌手 {}", self.id)
            } else {
                self.name
            },
            alias: self
                .alias
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect(),
            song_count: self.song_count,
            album_count: self.album_count,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 这四个是「正常流程」的码，必须稳定映射。
    #[test]
    fn known_codes_map_to_states() {
        assert_eq!(QrState::from_code(801), QrState::Waiting);
        assert_eq!(QrState::from_code(802), QrState::Scanned);
        assert_eq!(QrState::from_code(803), QrState::Confirmed);
        assert_eq!(QrState::from_code(800), QrState::Expired);
    }

    /// 8821 是风控拒绝，必须与「空响应」分开。
    ///
    /// 这两者以前共用一个 `Unknown`，后果是：用户扫完码、App 显示已确认，
    /// 终端却一直打印「继续等待」，直到把超时耗光——把一个立即可知的失败
    /// 拖成了 5 分钟的傻等。
    #[test]
    fn risk_control_is_not_confused_with_empty_response() {
        let blocked = QrState::from_code(8821);
        let empty = QrState::from_code(0);
        assert_eq!(blocked, QrState::Blocked);
        assert_eq!(empty, QrState::Empty);
        assert_ne!(blocked, empty, "风控拒绝和空响应是完全相反的两件事");
    }

    /// 终止态必须是「不能再等了」的那几个。
    #[test]
    fn only_final_states_are_terminal() {
        for terminal in [
            QrState::Confirmed,
            QrState::Expired,
            QrState::Blocked,
        ] {
            assert!(terminal.is_terminal(), "{terminal:?} 应当终止轮询");
        }
        // 这几个还得接着等
        for ongoing in [QrState::Waiting, QrState::Scanned, QrState::Empty] {
            assert!(!ongoing.is_terminal(), "{ongoing:?} 不该终止轮询");
        }
    }

    /// 没见过的码不能当成终止态：接口加了新状态时，
    /// 宁可多等一会儿，也不要误判成失败让用户白重来一次。
    #[test]
    fn unrecognized_codes_keep_polling() {
        let odd = QrState::from_code(-447);
        assert_eq!(odd, QrState::Unrecognized);
        assert!(!odd.is_terminal());
    }

    /// `QrPoll` 必须把 code 原样留住——报错文案里要打给用户看。
    #[test]
    fn poll_keeps_raw_code_and_message() {
        let poll = QrPoll::new(8821, "需要行为验证码验证".to_string(), None);
        assert_eq!(poll.state, QrState::Blocked);
        assert_eq!(poll.code, 8821);
        assert_eq!(poll.message, "需要行为验证码验证");
        assert!(poll.cookie.is_none(), "没成功就不该有凭据");
    }

    fn daily(body: &str) -> (i32, Vec<Track>, DailyShape) {
        let env: DailyEnvelope = serde_json::from_str(body).expect("解析失败");
        let (code, raw, shape) = env.pick();
        (code, raw.into_iter().map(RawTrack::into_track).collect(), shape)
    }

    /// 实测的匿名响应原文。空列表 + `code:200`——这个接口不报错。
    #[test]
    fn daily_silent_empty_is_not_an_error() {
        let (code, tracks, shape) = daily(r#"{"code":200,"recommend":[]}"#);
        assert_eq!(code, 200);
        assert!(tracks.is_empty());
        assert_eq!(shape, DailyShape::Recommend);
    }

    /// 关键区分：字段在但是空的 = 未登录；字段整个不在 = 接口改版。
    ///
    /// 两者都是「零首曲目」，混成一个状态的话，「凭据失效」会被误报成
    /// 「接口结构变了」，用户就会去改代码而不是重新登录。
    #[test]
    fn daily_empty_field_differs_from_missing_field() {
        let (_, _, empty) = daily(r#"{"code":200,"recommend":[]}"#);
        let (_, _, missing) = daily(r#"{"code":200}"#);
        assert_eq!(empty, DailyShape::Recommend);
        assert_eq!(missing, DailyShape::Missing);
        assert_ne!(empty, missing);
    }

    /// 字段是 `null` 也算「空」，不能算「改版」——网易云的数组字段经常是 null。
    #[test]
    fn daily_null_field_counts_as_empty() {
        let (_, tracks, shape) = daily(r#"{"code":200,"recommend":null}"#);
        assert!(tracks.is_empty());
        assert_eq!(shape, DailyShape::Recommend);
    }

    /// 新版形状（自建服务 / weapi 路径）给的是 `data.dailySongs`，
    /// 而且字段名是 App 风格（`ar` / `al` / `dt`）。两套都得认。
    #[test]
    fn daily_parses_daily_songs_shape_with_app_field_names() {
        let body = r#"{"code":200,"data":{"code":200,"dailySongs":[
            {"id":123,"name":"甲","ar":[{"name":"歌手A"},{"name":"歌手B"}],
             "al":{"id":9,"name":"专辑","picUrl":"http://cover"},"dt":241000,"fee":1}
        ]}}"#;
        let (code, tracks, shape) = daily(body);
        assert_eq!(code, 200);
        assert_eq!(shape, DailyShape::DailySongs);
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(t.id, 123);
        assert_eq!(t.name, "甲");
        assert_eq!(t.artists, vec!["歌手A", "歌手B"]);
        assert_eq!(t.album, "专辑");
        assert_eq!(t.album_id, 9);
        assert_eq!(t.duration_ms, 241_000, "`dt` 要能当 `duration` 用");
        assert_eq!(t.cover_url.as_deref(), Some("http://cover"));
        assert!(t.vip_only(), "fee=1 是会员曲目");
    }

    /// 空数组里夹 `null` 占位（已下架）是常态，不能让它变成一首 id=0 的幽灵曲目。
    #[test]
    fn daily_skips_null_placeholders() {
        let body = r#"{"code":200,"recommend":[null,{"id":7,"name":"乙","artists":[{"name":"C"}]}]}"#;
        let (_, tracks, _) = daily(body);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, 7);
        assert!(tracks.iter().all(|t| t.id > 0));
    }

    // ---------- 搜索 ----------

    fn search_playlists(body: &str) -> SearchOutcome<PlaylistBrief> {
        let env: SearchEnvelope<PlaylistSearchResult> =
            serde_json::from_str(body).expect("解析失败");
        let (code, result) = env.pick();
        let Some(result) = result else {
            return SearchOutcome {
                items: Vec::new(),
                total: 0,
                code,
                shape: ListShape::Missing,
            };
        };
        let present = result.playlists.is_present();
        let items: Vec<PlaylistBrief> = result
            .playlists
            .into_vec()
            .into_iter()
            .flatten()
            .map(RawPlaylist::into_brief)
            .collect();
        let (total, shape) = classify(present, items.len(), result.playlist_count);
        SearchOutcome {
            items,
            total,
            code,
            shape,
        }
    }

    /// 实测的空结果原文：数组字段**整个消失**，只剩一个 `playlistCount: 0`。
    ///
    /// 这条是最容易写错的地方：如果按「数组字段不在 = 接口改版」处理，
    /// 用户搜一个生僻词就会被告知「接口结构变了」，然后去改代码。
    #[test]
    fn empty_search_is_not_a_schema_change() {
        let out = search_playlists(r#"{"result":{"playlistCount":0},"code":200}"#);
        assert!(out.items.is_empty());
        assert_eq!(out.total, 0);
        assert_eq!(out.code, 200);
        assert_eq!(
            out.shape,
            ListShape::Empty,
            "数组不在但计数字段在 = 真没搜到，不是改版"
        );
    }

    /// 两样都缺才说明解析已经不成立了。那时必须和「没搜到」分开报。
    #[test]
    fn missing_count_field_means_schema_change() {
        let out = search_playlists(r#"{"code":200,"somethingElse":[]}"#);
        assert_eq!(out.shape, ListShape::Missing);
        assert_ne!(out.shape, ListShape::Empty);
    }

    /// `total` 是接口自报的命中总数，不是我取回来的条数——差得很远是常态（实测 336 vs 3）。
    #[test]
    fn total_comes_from_the_count_field() {
        let out = search_playlists(
            r#"{"result":{"playlistCount":301,"playlists":[
                {"id":6792103822,"name":"周杰伦-Jay","trackCount":143,
                 "creator":{"nickname":"Buradarrr"},"specialType":0,"subscribed":false}
            ]},"code":200}"#,
        );
        assert_eq!(out.shape, ListShape::Items);
        assert_eq!(out.total, 301, "显示的应该是命中总数，不是本页条数");
        assert_eq!(out.items.len(), 1);
        let pl = &out.items[0];
        assert_eq!(pl.id, 6792103822);
        assert_eq!(pl.name, "周杰伦-Jay");
        assert_eq!(pl.creator, "Buradarrr");
        assert_eq!(pl.track_count, 143);
        assert_eq!(pl.key(), "netease:6792103822");
        assert!(!pl.is_favorite());
        assert_eq!(pl.subscribed, Some(false));
    }

    /// 自建服务会把结果再包一层 `data`。只认一种形状就会在换接入方式时静默变空。
    #[test]
    fn search_reads_result_through_the_data_wrapper() {
        let out = search_playlists(
            r#"{"code":200,"data":{"code":200,"result":{"playlistCount":1,
                "playlists":[{"id":5,"name":"包了一层"}]}}}"#,
        );
        assert_eq!(out.shape, ListShape::Items);
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].name, "包了一层");
    }

    /// 匿名时 `subscribed` 是 `null`（不知道），不是 `false`（没收藏）。
    /// 用 `bool` 接就会把「不知道」说成「没收藏」，所以必须是 `Option`。
    #[test]
    fn subscribed_null_is_unknown_not_false() {
        let out = search_playlists(
            r#"{"code":200,"result":{"playlistCount":1,"playlists":[
                {"id":9,"name":"我喜欢的音乐","specialType":5,"privacy":10,"subscribed":null}
            ]}}"#,
        );
        let pl = &out.items[0];
        assert_eq!(pl.subscribed, None, "null 是「不知道」");
        assert_ne!(pl.subscribed, Some(false), "不能当成「没收藏」");
        assert!(pl.is_favorite(), "specialType=5 是我喜欢的音乐");
        assert!(pl.is_private(), "privacy=10 是私密歌单");
    }

    /// 搜索单曲：搜索结果的字段名跟歌单详情一致（`artists` / `album` / `duration`），
    /// 而且是 App 风格的 `ar` 也要认——两套名字混用过。
    #[test]
    fn search_songs_parses_both_field_naming_styles() {
        let env: SearchEnvelope<SongSearchResult> = serde_json::from_str(
            r#"{"code":200,"result":{"songCount":336,"songs":[
                {"id":186016,"name":"晴天","artists":[{"name":"周杰伦"}],
                 "album":{"id":1,"name":"叶惠美"},"duration":269000,"fee":8},
                {"id":7,"name":"甲","ar":[{"name":"A"},{"name":"B"}],
                 "al":{"id":2,"name":"专辑"},"dt":241000,"fee":0}
            ]}}"#,
        )
        .expect("解析失败");
        let (code, result) = env.pick();
        assert_eq!(code, 200);
        let result = result.expect("结果体应当在 result 里");
        let items: Vec<Track> = result
            .songs
            .into_vec()
            .into_iter()
            .flatten()
            .map(RawTrack::into_track)
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "晴天");
        assert_eq!(items[0].artists, vec!["周杰伦"]);
        assert_eq!(items[0].album, "叶惠美");
        assert_eq!(items[1].artists, vec!["A", "B"], "`ar` 要能当 `artists` 用");
        assert_eq!(items[1].duration_ms, 241_000, "`dt` 要能当 `duration` 用");
    }

    /// 歌手搜索里的歌曲数是 `musicSize`，别拿成 `mvSize`（那是 MV 数）。
    #[test]
    fn artist_search_uses_music_size_not_mv_size() {
        let env: SearchEnvelope<ArtistSearchResult> = serde_json::from_str(
            r#"{"code":200,"result":{"artistCount":85,"artists":[
                {"id":6452,"name":"周杰伦","alias":["Jay Chou","周董"],
                 "musicSize":568,"albumSize":41,"mvSize":9}
            ]}}"#,
        )
        .expect("解析失败");
        let (_, result) = env.pick();
        let items: Vec<ArtistBrief> = result
            .expect("结果体应当在 result 里")
            .artists
            .into_vec()
            .into_iter()
            .flatten()
            .map(RawSearchArtist::into_brief)
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "周杰伦");
        assert_eq!(items[0].alias, vec!["Jay Chou", "周董"]);
        assert_eq!(items[0].song_count, 568, "是 musicSize 不是 mvSize");
        assert_eq!(items[0].album_count, 41);
    }

    /// 账号歌单：`playlist` 在**顶层**，不在 `result` 里。
    ///
    /// 这是接口里最容易默默写错的一处：照 `result.playlist` 写会永远读到空，
    /// 而 `code` 是 200，什么异常都看不出来。
    #[test]
    fn user_playlists_reads_the_top_level_array() {
        let env: UserPlaylistEnvelope = serde_json::from_str(
            r#"{"code":200,"more":true,"playlist":[
                {"id":501,"name":"我喜欢的音乐","trackCount":312,"specialType":5,
                 "privacy":0,"subscribed":null,"creator":{"nickname":"我"}},
                {"id":502,"name":"私密歌单","trackCount":10,"specialType":0,
                 "privacy":10,"subscribed":true,"creator":{"nickname":"我"}}
            ]}"#,
        )
        .expect("解析失败");
        assert!(env.more);
        let present = env.playlist.is_present();
        let items: Vec<PlaylistBrief> = env
            .playlist
            .into_vec()
            .into_iter()
            .flatten()
            .map(RawPlaylist::into_brief)
            .collect();
        assert_eq!(items.len(), 2);
        let (_, shape) = classify(present, items.len(), None);
        assert_eq!(shape, ListShape::Items);
        assert!(items[0].is_favorite());
        assert!(items[1].is_private());
        assert_eq!(items[1].subscribed, Some(true));
    }

    /// 账号里一个歌单都没有是几乎不可能的（总有「我喜欢的音乐」），
    /// 所以数组字段缺失在这里值得报出来；但**不能**和「空」混为一谈。
    #[test]
    fn user_playlists_distinguishes_empty_from_missing() {
        let empty: UserPlaylistEnvelope =
            serde_json::from_str(r#"{"code":200,"more":false,"playlist":[]}"#).unwrap();
        let empty_present = empty.playlist.is_present();
        let (_, empty_shape) = classify(empty_present, 0, None);
        assert_eq!(empty_shape, ListShape::Empty);

        let missing: UserPlaylistEnvelope =
            serde_json::from_str(r#"{"code":200,"more":false}"#).unwrap();
        let missing_present = missing.playlist.is_present();
        let (_, missing_shape) = classify(missing_present, 0, None);
        assert_eq!(missing_shape, ListShape::Missing);
        assert_ne!(empty_shape, missing_shape);
    }

    /// 关键词里的中文和保留字符必须转义，否则 `&` 会凭空多切出一个参数、
    /// `#` 会把后面的内容全丢掉。
    #[test]
    fn keywords_are_percent_encoded() {
        assert_eq!(encode_query("周杰伦"), "%E5%91%A8%E6%9D%B0%E4%BC%A6");
        assert_eq!(encode_query("AC/DC & friends"), "AC%2FDC%20%26%20friends");
        assert_eq!(encode_query("a#b"), "a%23b");
        // unreserved 集合不该被改动
        assert_eq!(encode_query("abcXYZ0-._~"), "abcXYZ0-._~");
    }

    /// `classify` 的三种组合，直接钉住判据。
    #[test]
    fn classify_covers_the_three_shapes() {
        assert_eq!(classify(true, 3, Some(336)), (336, ListShape::Items));
        assert_eq!(classify(true, 0, Some(0)), (0, ListShape::Empty));
        assert_eq!(classify(false, 0, Some(0)), (0, ListShape::Empty));
        assert_eq!(classify(false, 0, None), (0, ListShape::Missing));
        // 没给计数字段但有内容时，总数退化成实际条数
        assert_eq!(classify(true, 2, None), (2, ListShape::Items));
    }
}
