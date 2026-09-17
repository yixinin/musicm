//! 运行配置。默认值全部落在 `$HOME/.musicm` 下，飞牛上就是 `/vol1/1000/...` 那类路径。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 音质档位。`br` 是老接口唯一认的写法（只有四档），`level` 是新接口的写法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Standard,
    Higher,
    #[default]
    Exhigh,
    Lossless,
}

impl Quality {
    pub const ALL: [Quality; 4] = [
        Quality::Standard,
        Quality::Higher,
        Quality::Exhigh,
        Quality::Lossless,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Standard => "standard",
            Quality::Higher => "higher",
            Quality::Exhigh => "exhigh",
            Quality::Lossless => "lossless",
        }
    }

    /// legacy `br` 参数值。
    pub fn br(self) -> u64 {
        match self {
            Quality::Standard => 128_000,
            Quality::Higher => 192_000,
            Quality::Exhigh => 320_000,
            Quality::Lossless => 999_000,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" | "128" | "128000" | "std" => Some(Quality::Standard),
            "higher" | "192" | "192000" => Some(Quality::Higher),
            "exhigh" | "320" | "320000" | "high" => Some(Quality::Exhigh),
            "lossless" | "999" | "999000" | "flac" => Some(Quality::Lossless),
            _ => None,
        }
    }

    /// 降级链：从请求的档位开始，逐个往下降，直到拿到可用链接。
    /// 会员档位拿不到时能自动落到免费档，而不是直接失败。
    pub fn ladder_from(self) -> Vec<Quality> {
        let order = [
            Quality::Lossless,
            Quality::Exhigh,
            Quality::Higher,
            Quality::Standard,
        ];
        let start = order.iter().position(|q| *q == self).unwrap_or(2);
        order[start..].to_vec()
    }

    /// 这个档位预期的容器格式。
    ///
    /// 只在「还没落地的曲目」上用作占位——FUSE 的 `readdir` 必须在下载之前
    /// 就报出文件名，而文件名带扩展名，所以只能先猜一个。
    /// 服务端不一定会听话（实测请求无损经常返回 320k mp3），
    /// 所以真实落地后会以实际容器为准。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn expected_ext(self) -> &'static str {
        match self {
            Quality::Lossless => "flac",
            _ => "mp3",
        }
    }
}

/// 音源接入方式。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ApiMode {
    /// 直连网易云的老版开放接口，无需自建服务。匿名只能拿免费档曲目。
    #[default]
    Direct,
    /// 指向自建 API 服务（api-enhanced / NeteaseCloudMusicApi），支持 cookie 与解灰。
    Sidecar { base: String },
}

impl ApiMode {
    pub fn prefix(&self) -> String {
        match self {
            ApiMode::Direct => "https://music.163.com/api".to_string(),
            ApiMode::Sidecar { base } => base.trim_end_matches('/').to_string(),
        }
    }

    /// 取播放链接的路径。两种服务在这一个接口上路径不同，其余接口一致。
    pub fn url_path(&self) -> &'static str {
        match self {
            ApiMode::Direct => "/song/enhance/player/url",
            ApiMode::Sidecar { .. } => "/song/url/v1",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ApiMode::Direct => "直连 music.163.com（匿名）".to_string(),
            ApiMode::Sidecar { base } => format!("自建 API 服务 {base}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    /// 音乐文件落地根目录
    pub out_root: PathBuf,
    pub api: ApiMode,
    /// 网易云 cookie，至少需要 MUSIC_U。
    ///
    /// **这个字段是只读的兼容入口**：能从旧的 `config.json` 读进来，但永远不会再被
    /// 写回去（`skip_serializing`）——凭据不该躺在明文配置里。加载时会迁移到
    /// `cookie.txt`（权限 600），运行期要读 cookie 请用 [`Config::credentials`]。
    ///
    /// 注意别写成 `skip_serializing_if = "Option::is_none"`：那个的语义是
    /// 「**是 None 时**跳过」，也就是 `Some` 时照样写出去，等于没保护。
    #[serde(default, skip_serializing)]
    pub cookie: Option<String>,
    pub quality: Quality,
    pub embed_cover: bool,
    pub write_lrc: bool,
    pub embed_lyrics: bool,

    /// cookie 的来源，只存在于内存里，不落盘。
    #[serde(skip)]
    pub cookie_origin: crate::auth::Origin,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = default_data_dir();
        Config {
            out_root: data_dir.join("library"),
            data_dir,
            api: ApiMode::Direct,
            cookie: None,
            quality: Quality::Exhigh,
            embed_cover: true,
            write_lrc: true,
            embed_lyrics: true,
            cookie_origin: crate::auth::Origin::None,
        }
    }
}

impl Config {
    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }

    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.json")
    }

    /// 加载配置：优先用 `data_dir` 参数指定的目录，其次 `MUSICM_HOME`，最后 `$HOME/.musicm`。
    /// 文件不存在时会写一份默认配置，方便用户直接改。
    pub fn load(data_dir: Option<PathBuf>) -> Result<Config> {
        let mut cfg = match &data_dir {
            Some(dir) => Config {
                data_dir: dir.clone(),
                out_root: dir.join("library"),
                ..Config::default()
            },
            None => Config::default(),
        };

        let path = cfg.config_path();
        if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("读取配置失败: {}", path.display()))?;
            let mut loaded: Config = serde_json::from_str(&raw)
                .with_context(|| format!("配置 JSON 解析失败: {}", path.display()))?;
            // 命令行显式指定的目录优先级最高
            if let Some(dir) = data_dir {
                loaded.data_dir = dir;
            }
            cfg = loaded;
        } else {
            ensure_dir(&cfg.data_dir)?;
            cfg.save()?;
        }

        cfg.apply_env();
        cfg.resolve_cookie()?;
        Ok(cfg)
    }

    /// 定下这次运行用哪份 cookie。
    ///
    /// 优先级：环境变量 > `cookie.txt` > `config.json` 里的旧明文。
    /// （命令行的 `--cookie` 由 `main` 在这之后覆盖，优先级最高。）
    ///
    /// 如果只找到了 `config.json` 里的旧明文，顺手把它搬进 `cookie.txt`——
    /// 这是老用户升级后唯一会走到迁移的路径，迁移失败要报错而不是默默丢掉凭据。
    fn resolve_cookie(&mut self) -> Result<()> {
        if self.cookie_origin == crate::auth::Origin::Env {
            return Ok(());
        }

        if let Some(stored) = crate::auth::load(&self.data_dir)? {
            self.cookie = Some(stored);
            self.cookie_origin = crate::auth::Origin::File;
            return Ok(());
        }

        if let Some(legacy) = self.cookie.clone() {
            crate::auth::save(&self.data_dir, &legacy)?;
            self.cookie = Some(legacy);
            self.cookie_origin = crate::auth::Origin::Migrated;
            // 重写一次 config.json，把原来的明文摘掉（cookie 字段是 skip_serializing）
            self.save()?;
        } else {
            self.cookie_origin = crate::auth::Origin::None;
        }
        Ok(())
    }

    fn apply_env(&mut self) {
        if let Some(v) = env_nonempty("MUSICM_COOKIE") {
            self.cookie = Some(v);
            self.cookie_origin = crate::auth::Origin::Env;
        }
        if let Some(v) = env_nonempty("MUSICM_API_BASE") {
            self.api = ApiMode::Sidecar { base: v };
        }
        if let Some(v) = env_nonempty("MUSICM_QUALITY") {
            if let Some(q) = Quality::parse(&v) {
                self.quality = q;
            }
        }
        if let Some(v) = env_nonempty("MUSICM_OUT") {
            self.out_root = PathBuf::from(v);
        }
    }

    pub fn save(&self) -> Result<()> {
        ensure_dir(&self.data_dir)?;
        let body = serde_json::to_string_pretty(self)?;
        fs::write(self.config_path(), body)
            .with_context(|| format!("写入配置失败: {}", self.config_path().display()))
    }

    /// 整理成可直接塞进请求头的形式。
    /// 从浏览器复制 cookie 时经常带上换行，这里统一清掉。
    pub fn cookie_header(&self) -> Option<String> {
        let normalized = crate::auth::normalize(self.cookie.as_deref()?);
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    /// 凭据是否构成登录态。
    ///
    /// 只判断有没有 `MUSIC_U`——**它是否还有效必须联网才知道**
    /// （见 `NeteaseClient::account`）：网易云对失效 cookie 不会报错，
    /// 只是把 `profile` 返回成 null，本地看不出来。
    pub fn has_login(&self) -> bool {
        self.credentials().is_some()
    }

    /// 解析过的凭据。cookie 存在但里面没有 `MUSIC_U` 时返回 `None`。
    pub fn credentials(&self) -> Option<crate::auth::Credentials> {
        crate::auth::Credentials::parse(self.cookie.as_deref()?).ok()
    }

    /// 给 `info` 用的一行描述，不联网。
    pub fn describe_login(&self) -> String {
        let Some(raw) = self.cookie.as_deref() else {
            return "未配置".to_string();
        };
        match crate::auth::Credentials::parse(raw) {
            Ok(cred) => cred.masked(),
            Err(_) => "⚠ 有 cookie 但不含 MUSIC_U，不会解锁会员曲目".to_string(),
        }
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_data_dir() -> PathBuf {
    if let Some(v) = env_nonempty("MUSICM_HOME") {
        return PathBuf::from(v);
    }
    home_dir().join(".musicm")
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).with_context(|| format!("创建目录失败: {}", p.display()))
}

fn env_nonempty(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}
