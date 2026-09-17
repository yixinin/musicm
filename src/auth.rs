//! 登录凭据（cookie）的存取、脱敏与来源追踪。
//!
//! # 为什么单独一个文件、还要 600 权限
//!
//! cookie 里的 `MUSIC_U` 就是账号的等价物——拿到它就能以这个身份点歌。
//! 所以它不该和 `config.json` 混在一起：那个文件是给人看、给人手改的，
//! 默认权限是 644，还会被随手贴进聊天窗口。
//!
//! 这里的取舍是：
//!
//! - cookie 单独存 `cookie.txt`，Unix 下强制 `600`；
//! - 旧版本写在 `config.json` 里的明文会在加载时自动迁移过来，
//!   并且**从此不再写回**（见 `Config::resolve_cookie`）；
//! - 打印时一律走 [`Credentials::masked`]，不会把 `MUSIC_U` 的明文吐到终端或日志里。
//!
//! 平台差异只在这一处：Unix 设权限、非 Unix 什么都不做。因为 `#[cfg]` 写在文件内部、
//! 文件本身在两边都会被编译，所以 `tools/linuxcheck` 能编到 Unix 那一支。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// cookie 文件名，放在数据目录下。
pub const COOKIE_FILE: &str = "cookie.txt";

/// 登录态的必需字段。只有 `MUSIC_A`（匿名 token）不算登录。
pub const LOGIN_KEY: &str = "MUSIC_U";

/// 打印时要打码的字段。
const SECRET_KEYS: [&str; 6] = [
    "MUSIC_U",
    "MUSIC_A",
    "__csrf",
    "NMTID",
    "_ntes_nuid",
    "_ntes_nnid",
];

/// cookie 是怎么来的。`musicm info` 要显示它——cookie 失效时，
/// 用户得先知道该去清哪里（凭据文件？还是环境变量？）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    #[default]
    None,
    /// `MUSICM_COOKIE` 环境变量，只对当次进程有效
    Env,
    /// `data_dir/cookie.txt`
    File,
    /// 旧版本留在 `config.json` 里的明文，已被迁移到文件
    Migrated,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::None => "无",
            Origin::Env => "环境变量 MUSICM_COOKIE（本次运行有效）",
            Origin::File => "凭据文件 cookie.txt",
            Origin::Migrated => "凭据文件 cookie.txt（从 config.json 迁移）",
        }
    }

    /// 值得提示「换个终端就没了」的来源。
    pub fn is_ephemeral(self) -> bool {
        matches!(self, Origin::Env)
    }
}

/// 校验过的一串 cookie。
#[derive(Debug, Clone)]
pub struct Credentials {
    raw: String,
}

impl Credentials {
    /// 归一化并检查登录字段。
    pub fn parse(raw: &str) -> Result<Credentials> {
        let normalized = normalize(raw);
        if normalized.is_empty() {
            bail!("cookie 是空的");
        }
        let cred = Credentials { raw: normalized };
        if !cred.has_login() {
            bail!(
                "这串 cookie 里没有 {LOGIN_KEY}，它不构成登录态。\n\
                 需要的是登录后浏览器里的那条 {LOGIN_KEY}=...，\
                 匿名 cookie（只有 MUSIC_A）拿不到会员曲目。"
            );
        }
        Ok(cred)
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// 是否具备登录态。只看 `MUSIC_U` 有没有值，不判断它是否还有效
    /// （有效性必须联网问账号接口，见 `NeteaseClient::account`）。
    pub fn has_login(&self) -> bool {
        self.get(LOGIN_KEY).is_some()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        let target = key.trim();
        split_pairs(&self.raw)
            .find(|(k, _)| k.eq_ignore_ascii_case(target))
            .map(|(_, v)| v)
            .filter(|v| !v.is_empty())
    }

    /// 可安全打印的形式：敏感字段只留长度。
    pub fn masked(&self) -> String {
        let parts: Vec<String> = split_pairs(&self.raw)
            .map(|(k, v)| {
                if SECRET_KEYS
                    .iter()
                    .any(|s| k.eq_ignore_ascii_case(s))
                {
                    format!("{k}=****（{} 字符）", v.chars().count())
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        parts.join("; ")
    }
}

/// 去掉**所有**空白字符。
///
/// 从浏览器复制 cookie 时几乎一定会带上换行（DevTools 的「复制为 cURL」、
/// 多行文本域都会），而 HTTP 头里出现换行是协议错误，所以必须清掉。
///
/// 连分隔符后面的空格也一起删（`a=1; b=2` → `a=1;b=2`）。这样做是安全的：
/// 按 RFC 6265，cookie 的名字和值都是 token，不含空格和逗号，
/// 所以「删掉空白」不可能把两个合法字段粘成别的意思。
pub fn normalize(raw: &str) -> String {
    let joined: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("")
        .trim_end_matches(';')
        .to_string();
    joined
}

/// 按 `;` 切开，返回 (键, 值)。用于替代到处手搓 `contains("MUSIC_U=")`。
fn split_pairs(raw: &str) -> impl Iterator<Item = (&str, &str)> {
    raw.split(';').filter_map(|part| {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let (k, v) = part.split_once('=')?;
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            None
        } else {
            Some((k, v))
        }
    })
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(COOKIE_FILE)
}

/// 写入凭据文件。Unix 下确保只有属主可读。
pub fn save(data_dir: &Path, cookie: &str) -> Result<PathBuf> {
    crate::config::ensure_dir(data_dir)?;
    let dest = path(data_dir);
    write_private(&dest, cookie)
        .with_context(|| format!("写入凭据失败: {}", dest.display()))?;
    harden(&dest)?;
    Ok(dest)
}

/// 读取凭据文件。文件不存在返回 `None`。
pub fn load(data_dir: &Path) -> Result<Option<String>> {
    let src = path(data_dir);
    if !src.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&src)
        .with_context(|| format!("读取凭据失败: {}", src.display()))?;
    let normalized = normalize(&raw);
    if normalized.is_empty() {
        return Ok(None);
    }
    Ok(Some(normalized))
}

/// 删除凭据文件，返回它原本是否存在。
pub fn clear(data_dir: &Path) -> Result<bool> {
    let target = path(data_dir);
    if !target.exists() {
        return Ok(false);
    }
    fs::remove_file(&target)
        .with_context(|| format!("删除凭据失败: {}", target.display()))?;
    Ok(true)
}

/// 以 `600` 创建文件，避免「先写成 644 再改」的那个窗口期。
#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> Result<()> {
    fs::write(path, body)?;
    Ok(())
}

/// 文件已存在时 `mode` 不生效，所以再显式收紧一次权限。
#[cfg(unix)]
fn harden(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置凭据文件权限失败: {}", path.display()))
}

#[cfg(not(unix))]
fn harden(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例用独立目录，测试是并行跑的。
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("musicm-auth-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_strips_newlines_and_spaces() {
        assert_eq!(normalize("a=1;\n  b=2\n"), "a=1;b=2");
        assert_eq!(normalize("  MUSIC_U=xyz  "), "MUSIC_U=xyz");
        assert_eq!(normalize("a=1;"), "a=1");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn parse_requires_music_u() {
        let ok = Credentials::parse("os=pc; MUSIC_U=secret-token; appver=2.9.7").unwrap();
        assert!(ok.has_login());
        assert_eq!(ok.get("MUSIC_U"), Some("secret-token"));
        assert_eq!(ok.get("music_u"), Some("secret-token"), "键名不该大小写敏感");
        assert_eq!(ok.get("appver"), Some("2.9.7"));

        // 匿名 token 不算登录
        assert!(Credentials::parse("MUSIC_A=anon; os=pc").is_err());
        assert!(Credentials::parse("").is_err());
        assert!(Credentials::parse("MUSIC_U=").is_err(), "空值不算登录");
    }

    #[test]
    fn masked_never_leaks_the_token() {
        let cred = Credentials::parse("os=pc; MUSIC_U=abcdef123456; __csrf=deadbeef").unwrap();
        let shown = cred.masked();
        assert!(!shown.contains("abcdef123456"), "不该出现明文: {shown}");
        assert!(!shown.contains("deadbeef"), "不该出现明文: {shown}");
        assert!(shown.contains("MUSIC_U=****（12 字符）"), "实际: {shown}");
        assert!(shown.contains("os=pc"), "非敏感字段照常显示: {shown}");
    }

    #[test]
    fn save_load_clear_round_trip() {
        let dir = scratch("round-trip");
        assert_eq!(load(&dir).unwrap(), None, "没写过时应当是 None");

        let saved = save(&dir, "MUSIC_U=token-abc; os=pc").unwrap();
        assert!(saved.ends_with(COOKIE_FILE));
        // 落盘的是归一化之后的形态——空格被删掉了，这是刻意的，见 normalize 的说明
        assert_eq!(load(&dir).unwrap().as_deref(), Some("MUSIC_U=token-abc;os=pc"));

        assert!(clear(&dir).unwrap(), "第一次删应当返回 true");
        assert!(!clear(&dir).unwrap(), "已经没了再删返回 false");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn cookie_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let saved = save(&dir, "MUSIC_U=token-abc").unwrap();
        let mode = fs::metadata(&saved).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "凭据文件必须是 600，实际 {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn origin_labels_are_distinct() {
        assert_ne!(Origin::Env.label(), Origin::File.label());
        // 环境变量是「换个终端就没了」的那种，文件不是
        assert!(Origin::Env.is_ephemeral());
        assert!(!Origin::File.is_ephemeral());
        assert!(!Origin::Migrated.is_ephemeral());
    }
}
