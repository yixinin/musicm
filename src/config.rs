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
    /// 网易云 cookie，至少需要 MUSIC_U
    #[serde(default)]
    pub cookie: Option<String>,
    pub quality: Quality,
    pub embed_cover: bool,
    pub write_lrc: bool,
    pub embed_lyrics: bool,
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
        Ok(cfg)
    }

    fn apply_env(&mut self) {
        if let Some(v) = env_nonempty("MUSICM_COOKIE") {
            self.cookie = Some(v);
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
        self.cookie.as_ref().map(|c| {
            c.split_whitespace()
                .collect::<Vec<_>>()
                .join("")
        }).filter(|c| !c.is_empty())
    }

    pub fn has_login(&self) -> bool {
        self.cookie_header()
            .map(|c| c.contains("MUSIC_U="))
            .unwrap_or(false)
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
