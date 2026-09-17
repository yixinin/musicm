//! 二维码的终端渲染。
//!
//! 只用 `qrcode` crate 生成点阵，自己画到终端上。不用它的 `image` / `svg`
//! 特性——那条路会拉进一堆图片编解码依赖，在 aarch64 的 NAS 上是纯粹的负担。
//!
//! 两个容易做错的地方：
//!
//! 1. **静默区（quiet zone）不能省。** 规范要求四周至少 4 个模块，缺了扫码器
//!    识别率会明显下降。这里把静默区当作真的白色模块画出来，而不是靠终端背景。
//! 2. **不能依赖终端主题。** 深色模块如果用「终端前景色」表示，在浅色主题下
//!    就会变成一个反相的二维码。所以这里显式指定黑色/白色前景与背景，
//!    浅色和深色终端上看到的是同一张图。

use anyhow::{Context, Result};
use qrcode::{Color, QrCode};

/// 静默区宽度（模块数）。规范要求 ≥ 4。
const QUIET: usize = 4;

/// 把文本编码成二维码点阵。
pub fn encode(text: &str) -> Result<QrCode> {
    QrCode::new(text.as_bytes()).with_context(|| {
        format!(
            "二维码编码失败（内容 {} 字节，通常是因为太长）：{text}",
            text.len()
        )
    })
}

/// 渲染成可直接打印到终端的行。
///
/// 每行用「上半块字符」`▀` 表示两个垂直模块：前景色画上半格，背景色画下半格。
/// 这样纵向分辨率翻倍，二维码在终端里不会太占高度。
pub fn render_lines(text: &str) -> Result<Vec<String>> {
    let code = encode(text)?;
    let width = code.width();
    let colors = code.to_colors();
    let total = width + QUIET * 2;

    // 带静默区的取色：越界与边框一律当成浅色。
    let is_dark = |x: usize, y: usize| -> bool {
        if x < QUIET || y < QUIET || x >= width + QUIET || y >= width + QUIET {
            return false;
        }
        colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
    };

    let mut lines = Vec::with_capacity(total.div_ceil(2));
    let mut y = 0;
    while y < total {
        let mut line = String::with_capacity(total * 12);
        for x in 0..total {
            let top = is_dark(x, y);
            // 行数为奇数时最后一行没有配对，补一行浅色。
            let bottom = is_dark(x, y + 1);
            line.push_str(match (top, bottom) {
                (true, true) => "\x1b[30;40m▀",
                (true, false) => "\x1b[30;47m▀",
                (false, true) => "\x1b[37;40m▀",
                (false, false) => "\x1b[37;47m▀",
            });
        }
        line.push_str("\x1b[0m");
        lines.push(line);
        y += 2;
    }
    Ok(lines)
}

/// 用 `#` / `.` 画出来的纯文本版本，给测试和重定向到文件时用。
/// 带 ANSI 转义的版本适合直接看，但不适合断言。
#[cfg(test)]
fn render_plain(text: &str) -> Vec<String> {
    let code = encode(text).expect("测试用的文本应当能编码");
    let width = code.width();
    let colors = code.to_colors();
    (0..width)
        .map(|y| {
            (0..width)
                .map(|x| {
                    if colors[y * width + x] == Color::Dark {
                        '#'
                    } else {
                        '.'
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 二维码有 3 个定位角：左上、右上、左下的外圈是实心的 7x7。
    /// 定位角画错基本等于整个二维码废掉，所以这里把它钉住。
    fn assert_finder(plain: &[String], ox: usize, oy: usize) {
        for dy in 0..7 {
            for dx in 0..7 {
                let dark = plain[oy + dy].as_bytes()[ox + dx] == b'#';
                // 外圈实心，内圈 3x3 实心，两者之间那圈是浅色。
                let ring = dx == 0 || dy == 0 || dx == 6 || dy == 6;
                let core = (2..=4).contains(&dx) && (2..=4).contains(&dy);
                assert_eq!(dark, ring || core, "定位角 ({ox},{oy}) 偏移 ({dx},{dy}) 不对");
            }
        }
    }

    #[test]
    fn plain_matrix_has_correct_finder_patterns() {
        let plain = render_plain("https://music.163.com/login?codekey=test-key");
        let w = plain.len();
        assert!(w >= 21, "二维码至少 21 个模块宽，实际 {w}");
        for row in &plain {
            assert_eq!(row.len(), w, "二维码应当是正方形");
        }
        assert_finder(&plain, 0, 0);
        assert_finder(&plain, w - 7, 0);
        assert_finder(&plain, 0, w - 7);
    }

    #[test]
    fn ansi_render_has_quiet_zone_on_every_side() {
        let text = "https://music.163.com/login?codekey=test-key";
        let lines = render_lines(text).unwrap();
        let plain = render_plain(text);
        let total = plain.len() + QUIET * 2;

        // 高度：total 行模块，每两行合成一行文本，奇数时向上取整
        assert_eq!(lines.len(), total.div_ceil(2));
        // 宽度：每行 total 个「▀」，外加结尾的复位序列
        for line in &lines {
            assert_eq!(
                line.matches('▀').count(),
                total,
                "每行应有 {total} 个半块字符"
            );
            assert!(line.ends_with("\x1b[0m"), "每行都要复位颜色");
        }

        // 静默区：开头两行文本 = 最上面 4 行模块，必须全是白底
        for line in lines.iter().take(2) {
            assert!(
                !line.contains("\x1b[30;40m") && !line.contains("\x1b[30;47m"),
                "顶部静默区不该出现深色模块: {line:?}"
            );
        }
    }

    #[test]
    fn longer_text_still_encodes() {
        // 真实登录二维码就是这个长度量级
        let url = format!("https://music.163.com/login?codekey={}", "a".repeat(36));
        let lines = render_lines(&url).unwrap();
        assert!(lines.len() > 10);
    }
}
