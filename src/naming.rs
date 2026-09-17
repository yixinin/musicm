//! 虚拟文件树的路径生成。
//!
//! 文件名要同时讨好三方：Linux 内核（只禁 `/` 和 NUL）、SMB/Windows 客户端
//! （禁 `\ : * ? " < > |`、首尾点号、保留设备名），以及飞牛的刮削器（截断、
//! 空名都会让它匹配失败）。所以这里从严处理。

use std::collections::HashSet;

use crate::model::Track;

/// 跨平台都不安全的字符，统一替换成下划线。
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Windows 保留设备名，命中后加前缀，否则 SMB 客户端读写会失败。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 单个路径段的字符上限。留足余量，避免路径总长撞上 260 / 4096 之类的限制。
const MAX_CHARS: usize = 100;

/// 把任意字符串压成一个安全的路径段。
pub fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;

    for ch in raw.chars() {
        let ch = if ch.is_control() || ILLEGAL.contains(&ch) {
            '_'
        } else {
            ch
        };
        // 折叠连续空白
        if ch == ' ' || ch == '\t' {
            if prev_space {
                continue;
            }
            prev_space = true;
            out.push(' ');
            continue;
        }
        prev_space = false;
        out.push(ch);
    }

    // 首尾的空格和点号在 Windows / SMB 上是雷
    let trimmed = out.trim_matches(|c: char| c == ' ' || c == '.');

    let mut s = if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    };

    if s.chars().count() > MAX_CHARS {
        s = s.chars().take(MAX_CHARS).collect();
        s = s.trim_end_matches(|c: char| c == ' ' || c == '.').to_string();
    }

    if RESERVED.contains(&s.to_ascii_uppercase().as_str()) {
        s.insert(0, '_');
    }

    s
}

/// 音源目录名。
pub fn source_label(source: &str) -> &'static str {
    match source {
        "qq" => "QQ音乐",
        _ => "网易云",
    }
}

/// 只保留字母数字的扩展名，缺省回落到 mp3。
pub fn sanitize_ext(ext: &str) -> String {
    let cleaned: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "mp3".to_string()
    } else {
        cleaned
    }
}

/// `01 歌名 - 歌手`，序号缺失时退化成 `歌名 - 歌手`。
pub fn track_file_stem(track: &Track) -> String {
    let body = format!("{} - {}", track.name, track.artist_line());
    let stem = if track.track_no == 0 {
        body
    } else {
        format!("{:02} {}", track.track_no, body)
    };
    sanitize_component(&stem)
}

/// 曲目所属的目录：`<音源>/<歌单>`。
/// 还没有归属歌单时（比如直接按 id 点播）落到 `<音源>/单曲`。
///
/// FUSE 出口与下载流程共用这一个函数，两边算出的目录必须一模一样，
/// 否则「虚拟树里的路径」和「磁盘上的路径」会对不上，缓存就永远命不中。
///
/// 返回 `String` 而不是 `PathBuf`，是因为虚拟树里的路径一律用 `/` 分隔；
/// 交给 `Path::join` 时它会自己按平台转换，不存在分隔符漂移的问题。
pub fn group_rel(source: &str, playlist_name: Option<&str>) -> String {
    let group = match playlist_name {
        Some(name) => sanitize_component(name),
        None => "单曲".to_string(),
    };
    format!("{}/{}", source_label(source), group)
}

/// 给一批曲目算出互不冲突的文件名主干（不含扩展名）。
///
/// 歌单里重复收录同一首歌并不罕见，两张不同的歌也可能被 sanitize 成同一个名字。
/// 如果不管，FUSE 的 `readdir` 就会吐出重名条目——内核对同名 dentry 只保留一个，
/// 另一首就永远读不到了。所以第 2、3 个重名的加 ` (2)`、` (3)` 后缀。
///
/// 结果是「按给定顺序确定性生成」的：只要曲目顺序不变，名字就不变。
pub fn unique_stems(tracks: &[Track]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::with_capacity(tracks.len());
    let mut out = Vec::with_capacity(tracks.len());

    for track in tracks {
        let base = track_file_stem(track);
        let mut candidate = base.clone();
        let mut n = 2usize;
        // 后缀本身也可能撞上别的曲目的自然名字，所以循环到真的不重复为止
        while used.contains(&candidate) {
            candidate = with_suffix(&base, n);
            n += 1;
        }
        used.insert(candidate.clone());
        out.push(candidate);
    }
    out
}

/// 加 ` (N)` 后缀，并保证总长仍然不超过单段的字符上限。
fn with_suffix(stem: &str, n: usize) -> String {
    let suffix = format!(" ({n})");
    let budget = MAX_CHARS.saturating_sub(suffix.chars().count());
    let head: String = stem.chars().take(budget).collect();
    let head = head
        .trim_end_matches(|c: char| c == ' ' || c == '.')
        .to_string();
    format!("{head}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_illegal_chars() {
        assert_eq!(sanitize_component("AC/DC: Back?"), "AC_DC_ Back_");
    }

    #[test]
    fn trims_dots_and_spaces() {
        assert_eq!(sanitize_component("  ..歌名..  "), "歌名");
    }

    #[test]
    fn guards_reserved_names() {
        assert_eq!(sanitize_component("con"), "_con");
    }

    #[test]
    fn empty_becomes_untitled() {
        assert_eq!(sanitize_component("   "), "untitled");
    }

    #[test]
    fn collapses_runs_but_keeps_cjk() {
        assert_eq!(sanitize_component("海阔    天空"), "海阔 天空");
    }

    #[test]
    fn ext_is_filtered() {
        assert_eq!(sanitize_ext("FLAC"), "flac");
        assert_eq!(sanitize_ext("../x"), "x");
        assert_eq!(sanitize_ext(""), "mp3");
    }

    fn track(id: u64, no: u32, name: &str, artist: &str) -> Track {
        Track {
            source: "netease".to_string(),
            id,
            name: name.to_string(),
            artists: vec![artist.to_string()],
            album: "专辑".to_string(),
            album_id: 1,
            cover_url: None,
            duration_ms: 200_000,
            track_no: no,
            disc: 1,
            fee: 0,
            playable: None,
        }
    }

    #[test]
    fn group_is_source_then_playlist() {
        assert_eq!(
            group_rel("netease", Some("热歌榜")),
            "网易云/热歌榜"
        );
        assert_eq!(group_rel("netease", None), "网易云/单曲");
        assert_eq!(group_rel("qq", Some("A/B")), "QQ音乐/A_B");
    }

    #[test]
    fn unique_stems_leaves_distinct_names_alone() {
        let src = vec![track(1, 1, "甲", "A"), track(2, 2, "乙", "B")];
        assert_eq!(unique_stems(&src), vec!["01 甲 - A", "02 乙 - B"]);
    }

    #[test]
    fn unique_stems_suffixes_repeats() {
        // 同一首歌在同一张歌单里出现两次
        let src = vec![track(1, 1, "甲", "A"), track(1, 1, "甲", "A"), track(1, 1, "甲", "A")];
        assert_eq!(
            unique_stems(&src),
            vec!["01 甲 - A", "01 甲 - A (2)", "01 甲 - A (3)"]
        );
    }

    #[test]
    fn unique_stems_dodges_natural_name_collision() {
        // 第二首的自然名字正好等于第一首的加后缀结果，仍然必须两两不同
        let src = vec![
            track(1, 1, "甲", "A"),
            track(2, 1, "甲", "A"),
            track(3, 1, "甲 - A (2)", "x"),
        ];
        let got = unique_stems(&src);
        let set: std::collections::HashSet<_> = got.iter().collect();
        assert_eq!(set.len(), got.len(), "生成了重名: {got:?}");
    }
}
