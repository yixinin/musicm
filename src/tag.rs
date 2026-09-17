//! 标签写入。
//!
//! 飞牛音乐的刮削依赖内嵌元数据（官方 FAQ 明确写了：元信息缺失时封面和歌词都会匹配失败），
//! 所以下载完立刻补齐标签，而不是只靠文件名。
//!
//! 两个刻意的选择：
//! - 用 `Tag::new(tag_type)` + `save_to_path` 直接覆盖对应类型的标签，
//!   不走「先解析整个文件再改主标签」那条路，少一层失败可能。
//! - 写 ID3v2.3 而非 2.4：2.3 是网易云自己文件在用的版本，
//!   对老播放器和部分国产转码链更友好。

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use lofty::config::WriteOptions;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::{Accessor, TagExt};
use lofty::tag::{ItemKey, Tag, TagType};

use crate::model::Track;

#[derive(Debug, Clone, Copy, Default)]
pub struct TagReport {
    pub cover_embedded: bool,
    pub lyrics_embedded: bool,
    pub tag_type: &'static str,
}

/// 容器扩展名 → 该容器该用的标签类型。返回 None 表示这类文件不打标。
fn tag_type_for(path: &Path) -> Option<(TagType, &'static str)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    match ext.as_str() {
        "mp3" => Some((TagType::Id3v2, "ID3v2.3")),
        // 网易云的无损给的是裸 FLAC 流
        "flac" => Some((TagType::VorbisComments, "VorbisComment")),
        // 其他容器暂不打标，避免写坏结构
        _ => None,
    }
}

/// 给已落地的音频文件补标签。封面和歌词都是可选的，失败了也不会连累主流程。
pub fn write_tags(
    path: &Path,
    track: &Track,
    cover: Option<&[u8]>,
    lyrics_plain: Option<&str>,
) -> Result<TagReport> {
    let Some((tag_type, label)) = tag_type_for(path) else {
        return Err(anyhow!("该容器暂不支持写入标签，跳过打标"));
    };

    let mut tag = Tag::new(tag_type);
    tag.set_title(track.name.clone());
    tag.set_artist(track.artist_line());
    tag.set_album(track.album.clone());
    if track.track_no > 0 {
        tag.set_track(track.track_no);
    }
    if track.disc > 0 {
        tag.set_disk(track.disc);
    }

    let lyrics_embedded = match lyrics_plain {
        Some(text) if !text.trim().is_empty() => {
            // USLT：不带时间戳的纯歌词。带时间戳的完整版本写到同名 .lrc 里。
            tag.insert_text(ItemKey::UnsyncLyrics, text.to_string());
            true
        }
        _ => false,
    };

    let mut cover_embedded = false;
    if let Some(bytes) = cover {
        match Picture::from_reader(&mut &bytes[..]) {
            Ok(mut pic) => {
                // 图片类型从字节里认不出来，得手动指定为封面
                pic.set_pic_type(PictureType::CoverFront);
                tag.push_picture(pic);
                cover_embedded = true;
            }
            Err(_) => {
                // 封面可能是 webp 之类 lofty 认不出的格式，不影响后面的写入
            }
        }
    }

    let mut opts = WriteOptions::new();
    let opts = opts.use_id3v23(true);
    tag.save_to_path(path, opts)
        .with_context(|| format!("写入标签失败: {}", path.display()))?;

    Ok(TagReport {
        cover_embedded,
        lyrics_embedded,
        tag_type: label,
    })
}

/// 把 LRC 的时间戳剥掉，得到可以塞进 USLT 的纯文本。
/// 形如 `[00:12.345]` 的会被去掉，`[ar:歌手]` 这类元信息标签原样保留。
pub fn strip_timestamps(lrc: &str) -> String {
    let mut out = String::new();

    for line in lrc.lines() {
        let mut text = String::new();
        let mut chars = line.chars().peekable();

        while let Some(c) = chars.next() {
            if c != '[' {
                text.push(c);
                continue;
            }
            // 收集到匹配的右括号，判断它是不是时间戳
            let mut buf = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == ']' {
                    closed = true;
                    break;
                }
                buf.push(c2);
            }
            let looks_like_time = closed
                && buf.contains(':')
                && buf
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false);
            if !looks_like_time {
                text.push('[');
                text.push_str(&buf);
                if closed {
                    text.push(']');
                }
            }
        }

        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_lrc_timestamps() {
        let lrc = "[00:00.000] 作词 : 黄家驹\n[00:12.340]今天我\n[00:15.900]寒夜里看雪飘过";
        let plain = strip_timestamps(lrc);
        assert_eq!(plain, "作词 : 黄家驹\n今天我\n寒夜里看雪飘过");
    }

    #[test]
    fn keeps_unknown_bracket_content() {
        let lrc = "[ti:海阔天空]\n[00:01.000]仍然自由自我";
        let plain = strip_timestamps(lrc);
        assert!(plain.contains("[ti:海阔天空]"));
        assert!(plain.contains("仍然自由自我"));
        assert!(!plain.contains("[00:01.000]"));
    }

    #[test]
    fn tag_type_mapping() {
        assert!(tag_type_for(Path::new("a.mp3")).is_some());
        assert!(tag_type_for(Path::new("a.FLAC")).is_some());
        assert!(tag_type_for(Path::new("a.m4a")).is_none());
    }
}
