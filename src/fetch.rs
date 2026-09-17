//! 单曲落地：解析链接 → 下载 → 补标签 → 写歌词与封面。
//!
//! 「播放」在这个项目里等于「产出一个带完整元数据的可播放文件」：
//! 先有能被飞牛音乐正确识别的文件，后面的 FUSE 按需读才有意义。
//!
//! 落地顺序刻意设计成：先写临时文件 → 打标 → 原子改名。
//! 这样任何一步失败都不会在音乐库目录里留下半成品被扫描到。
//!
//! 对外有两个入口：
//! - [`Fetcher::fetch`]：按索引算路径，给命令行用
//! - [`Fetcher::ensure_stem`]：目录和文件名由调用方给，给 FUSE 出口用
//!
//! 后者是必需的：FUSE 的 `readdir` 必须先报出文件名，而文件名要等
//! 「落盘之后」才知道实际扩展名。路径必须由虚拟树统一决定，两边各算一次一定会漂移。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::config::Config;
use crate::model::{now_secs, CachedFile, Playlist, Track};
use crate::naming;
use crate::netease::NeteaseClient;
use crate::tag;

#[derive(Debug)]
pub struct FetchOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub quality: String,
    pub ext: String,
    pub tag: tag::TagReport,
    pub lrc_path: Option<PathBuf>,
    /// 降级链尝试过程中的备注（有值说明发生了降级）
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub from_cache: bool,
}

impl FetchOutcome {
    pub fn to_cached_file(&self, track: &Track) -> CachedFile {
        CachedFile {
            track: track.key(),
            path: self.path.to_string_lossy().to_string(),
            bytes: self.bytes,
            quality: self.quality.clone(),
            ext: self.ext.clone(),
            fetched_at: now_secs(),
            cover_embedded: self.tag.cover_embedded,
            lyrics_embedded: self.tag.lyrics_embedded,
            lrc_path: self.lrc_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        }
    }
}

pub struct Fetcher<'a> {
    client: &'a NeteaseClient,
    cfg: &'a Config,
}

impl<'a> Fetcher<'a> {
    pub fn new(client: &'a NeteaseClient, cfg: &'a Config) -> Self {
        Fetcher { client, cfg }
    }

    /// 按索引里的归属算出路径并落地。
    ///
    /// `siblings` 是这首歌所在歌单的全部曲目（顺序要与歌单一致）。
    /// 传了它才能和 FUSE 出口算出同一个文件名——歌单里重复收录同一首歌时，
    /// 后一次会出现 ` (2)` 后缀，两边必须用同一套算法。
    pub fn fetch(
        &self,
        track: &Track,
        playlist: Option<&Playlist>,
        siblings: Option<&[Track]>,
        force: bool,
    ) -> Result<FetchOutcome> {
        let stem = match siblings.and_then(|list| {
            let pos = list.iter().position(|t| t.key() == track.key())?;
            naming::unique_stems(list).into_iter().nth(pos)
        }) {
            Some(s) => s,
            None => naming::track_file_stem(track),
        };
        let dir = self
            .cfg
            .out_root
            .join(naming::group_rel(&track.source, playlist.map(|p| p.name.as_str())));
        self.ensure_stem(track, &dir, &stem, force)
    }

    /// 在指定目录下用指定主干落一个文件，扩展名由接口返回的容器格式决定。
    pub fn ensure_stem(
        &self,
        track: &Track,
        dir: &Path,
        stem: &str,
        force: bool,
    ) -> Result<FetchOutcome> {
        let mut warnings = Vec::new();

        // 1) 先解析链接。扩展名要等接口告诉我们格式才知道，所以路径也在这之后才能定。
        let (resolved, notes) = self.client.resolve_url(track.id, self.cfg.quality)?;
        let reason = resolved.reason.clone();
        let needs_login = resolved.needs_login();
        let logged_in = self.client.logged_in();
        let info = resolved.info.ok_or_else(|| {
            let hint = if needs_login && !logged_in {
                "这是会员或数字专辑曲目，需要登录态；执行 `musicm login --qr` 扫码登录即可解锁"
            } else if needs_login {
                "已经配了凭据却仍拿不到，多半是它过期了：用 `musicm whoami` 确认，`musicm login --qr` 重新登录"
            } else {
                "该曲目在源站无版权或已下架"
            };
            anyhow!("拿不到《{}》的播放链接：{}。{}", track.name, reason, hint)
        })?;

        let ext = naming::sanitize_ext(&info.ext);
        let dest = dir.join(format!("{stem}.{ext}"));

        // 2) 已经在库里且没要求强制重下，直接复用
        if dest.exists() && !force {
            let bytes = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            let lrc = dest.with_extension("lrc");
            return Ok(FetchOutcome {
                path: dest,
                bytes,
                quality: info.actual_level(),
                ext,
                tag: tag::TagReport::default(),
                lrc_path: lrc.exists().then_some(lrc),
                notes,
                warnings,
                from_cache: true,
            });
        }

        fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;

        // 3) 清掉上次中断留下的临时文件
        clean_stale_parts(dir, stem);

        // 4) 下载到临时文件。
        //    临时名必须保留音频扩展名，否则打标时 lofty 认不出容器格式。
        let part = dir.join(format!(".{stem}.part.{ext}"));
        let bytes = self.client.download_to(&info.url, &part)?;
        if bytes == 0 {
            let _ = fs::remove_file(&part);
            return Err(anyhow!("下载到的是空文件，源站链接可能已过期"));
        }

        // 5) 歌词与封面属于锦上添花，失败只记警告
        let lrc_text = match self.client.lyric(track.id) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("获取歌词失败: {e}"));
                None
            }
        };
        let cover_bytes = if self.cfg.embed_cover {
            match track.cover_url.as_deref() {
                Some(url) => match self.client.get_bytes(url) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        warnings.push(format!("获取封面失败: {e}"));
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

        // 6) 在临时文件上打标，成功才改名
        let plain_lyrics = lrc_text.as_deref().map(tag::strip_timestamps);
        let report = match tag::write_tags(
            &part,
            track,
            cover_bytes.as_deref(),
            plain_lyrics.as_deref(),
        ) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!(
                    "写入标签失败（文件仍可用，但飞牛可能刮削不到信息）: {e}"
                ));
                tag::TagReport::default()
            }
        };

        if dest.exists() {
            fs::remove_file(&dest)
                .with_context(|| format!("替换旧文件失败: {}", dest.display()))?;
        }
        fs::rename(&part, &dest)
            .with_context(|| format!("重命名失败: {} -> {}", part.display(), dest.display()))?;

        // 7) 同名 .lrc 与目录级 cover.jpg（飞牛的刮削器两种都认）
        let mut lrc_path = None;
        if self.cfg.write_lrc {
            if let Some(text) = &lrc_text {
                let p = dest.with_extension("lrc");
                match fs::write(&p, text) {
                    Ok(()) => lrc_path = Some(p),
                    Err(e) => warnings.push(format!("写入歌词文件失败: {e}")),
                }
            }
        }
        if let Some(cover) = &cover_bytes {
            let p = dir.join("cover.jpg");
            if !p.exists() {
                if let Err(e) = fs::write(&p, cover) {
                    warnings.push(format!("写入封面文件失败: {e}"));
                }
            }
        }

        Ok(FetchOutcome {
            path: dest,
            bytes,
            quality: info.actual_level(),
            ext,
            tag: report,
            lrc_path,
            notes,
            warnings,
            from_cache: false,
        })
    }
}

/// 清掉同一首歌上次中断留下的 `.xxx.part.mp3` 之类文件。
/// 只匹配我们自己生成的临时命名，不动任何正式文件。
fn clean_stale_parts(dir: &Path, stem: &str) {
    let prefix = format!(".{stem}.part.");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(&prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}
