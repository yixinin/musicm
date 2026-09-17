//! musicm —— 把网易云音乐的歌单变成本地音乐库，供飞牛音乐扫描播放。
//!
//! 当前里程碑：歌单扫描 + 单曲落地 + FUSE 出口。
//!
//! 分层：`netease` 管音源，`store` 管索引，`naming` 管路径命名，
//! `fetch` 管落地，`vfs` 管虚拟文件树，`mount` 管把它交给内核，
//! `fuse_fs` 是 FUSE 的具体适配。
//!
//! `fuse_fs` 是 Linux 专属的，所以它被 `#[cfg]` 挡在非 Linux 构建之外；
//! 被它牵连的 `mount` 则**在所有平台上都编译**，平台差异留在那个文件内部。
//! 这个安排是为了让开发机上的 `cargo build` 能编到其中的一半，
//! 另一半由 `tools/linuxcheck` 对着 aarch64-linux 编（详见 `mount.rs` 的文档）。

mod auth;
mod config;
mod fetch;
mod model;
mod naming;
mod netease;
mod qr;
mod store;
mod tag;
// Web 管理界面。它跨平台（HTTP 服务器跟 FUSE 无关），
// 所以不需要平台门控，宿主 `cargo build` 就能覆盖到。
mod web;

// 挂载这一步的平台分叉都在这里面，所有平台都会编译它（见该文件的模块文档）。
mod mount;

// 这两个模块只有 Linux 出口（fuse_fs）真正调用，但它们在所有平台上都会被编译：
// 一来类型错误能立刻在开发机上暴露，二来它们的单元测试本来就该跨平台跑。
// 代价是非 Linux 构建下 rustc 会认为它们是死代码，这里明确放行。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod materialize;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod vfs;

#[cfg(target_os = "linux")]
mod fuse_fs;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};

use crate::auth::{Credentials, Origin};
use crate::config::{Config, Quality};
use crate::fetch::Fetcher;
use crate::model::Track;
use crate::mount::MountArgs;
use crate::netease::{
    ArtistBrief, DailyOutcome, DailyShape, ListShape, NeteaseClient, PlaylistBrief, QrState,
    SearchKind, SearchOutcome,
};
use crate::store::Index;

#[derive(Parser, Debug)]
#[command(
    name = "musicm",
    version,
    about = "把网易云歌单变成本地音乐库，供飞牛音乐扫描播放"
)]
struct Cli {
    /// 数据目录（配置与索引都放这里），默认 $HOME/.musicm
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// 音乐输出根目录。给了就写入配置，后续沿用。
    #[arg(long, global = true)]
    out: Option<PathBuf>,

    /// 音质档位：standard / higher / exhigh / lossless。给了就写入配置，后续沿用。
    #[arg(long, global = true)]
    quality: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 扫描歌单并写入索引
    Scan {
        /// 歌单 id，即歌单链接里的数字
        playlist_id: u64,
    },
    /// 列出歌单：默认列索引里的，`--remote` 联网列账号里的
    Playlists {
        /// 联网列出账号里的歌单（自己的全都看得到，别人的只有公开歌单）
        #[arg(long)]
        remote: bool,
        /// 查哪个账号。缺省是自己——那需要登录态
        #[arg(long)]
        uid: Option<u64>,
        /// `--remote` 一次最多取回多少张
        #[arg(long, default_value_t = 200, value_name = "N")]
        limit: usize,
    },
    /// 搜索歌单 / 单曲 / 歌手
    Search {
        /// 关键词
        keyword: String,
        /// 搜哪一类；`all` 依次搜三类
        #[arg(long = "type", value_enum, default_value_t = SearchType::All)]
        kind: SearchType,
        /// 每类最多显示几条
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 从第几条开始（翻页用）
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// 顺便把搜到的单曲落地前 N 首（0 = 只列不下载）
        #[arg(long, default_value_t = 0, value_name = "N")]
        fetch: usize,
    },
    /// 列出某位歌手的热门曲目
    Artist {
        /// 歌手 id，从 `musicm search <歌手名> --type artist` 拿
        artist_id: u64,
        /// 最多显示几首
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// 顺便把前 N 首落地到音乐库
        #[arg(long, default_value_t = 0, value_name = "N")]
        fetch: usize,
    },
    /// 列出某张歌单的曲目
    Tracks {
        playlist_id: u64,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// 列出「每日推荐」曲目（需要登录，未登录时接口会静默返回空列表）
    Daily {
        /// 最多列出几首
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// 顺便把前 N 首落地到音乐库（0 = 只列不下载）
        #[arg(long, default_value_t = 0, value_name = "N")]
        fetch: usize,
    },
    /// 只解析播放链接，不下载（排查问题时用）
    Url {
        /// 曲目 id，或 `netease:123` 形式的完整键
        song_id: String,
    },
    /// 下载单曲并补齐标签，这是「播放」的落地形式
    Play {
        song_id: String,
        /// 已存在时强制重新下载
        #[arg(long)]
        force: bool,
    },
    /// 把音乐库挂载成只读文件系统（仅 Linux）
    Mount {
        /// 挂载点，不存在会自动创建。建议放在共享文件夹下的纯英文路径里
        mountpoint: PathBuf,

        /// ondemand：列出全部曲目，首次被读取时才去取回
        /// cached：只列出已经落盘的，全程零网络
        #[arg(long, value_enum, default_value_t = MountMode::Ondemand)]
        fuse_mode: MountMode,

        /// 允许其他用户访问。飞牛音乐以别的用户运行时，不打开这个就扫不到
        /// （需要 /etc/fuse.conf 里有 user_allow_other；关掉写 --allow-other=false）
        #[arg(long, default_value_t = false)]
        allow_other: bool,

        /// 事件循环线程数。取回音频是秒级阻塞操作，单线程会卡住整个文件系统
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
    /// 保存登录凭据，解锁会员曲目
    Login {
        /// 直接给 cookie 串（会留在 shell 历史里）
        #[arg(long)]
        cookie: Option<String>,
        /// 从文件读取 cookie
        #[arg(long)]
        cookie_file: Option<PathBuf>,
        /// 扫码登录：在终端里直接画出二维码，用手机网易云音乐 App 扫
        #[arg(long)]
        qr: bool,
        /// 扫码等待秒数
        #[arg(long, default_value_t = 300)]
        qr_timeout: u64,
    },
    /// 删除本地保存的凭据
    Logout,
    /// 联网确认登录态（昵称 / uid / 会员等级）
    Whoami,
    /// 查看配置与索引状态
    Info,
    /// 启动 Web 管理界面：搜索、登录、扫描、下载、挂载都在浏览器里做
    Serve {
        /// 监听地址。默认只听本机；要局域网访问就写 0.0.0.0:8765
        /// （绑到非回环地址会自动启用访问令牌）
        #[arg(long, default_value = "127.0.0.1:8765")]
        listen: String,
        /// 访问令牌。绑到非回环地址时必填，不给就自动生成一个并打印带令牌的链接
        #[arg(long)]
        token: Option<String>,
        /// 明确不要访问令牌。只在完全可信的网络里用
        #[arg(long)]
        no_auth: bool,
    },
}

/// `login` 的参数。抽成结构体是因为命令行那层要传进来的分支比较多。
struct LoginArgs {
    cookie: Option<String>,
    cookie_file: Option<PathBuf>,
    qr: bool,
    qr_timeout: u64,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum MountMode {
    /// 列出全部曲目，读到没落地的就去取
    Ondemand,
    /// 只列出已经落盘的，不联网
    Cached,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum SearchType {
    /// 三类都搜
    All,
    /// 只搜单曲
    Song,
    /// 只搜歌单
    Playlist,
    /// 只搜歌手
    Artist,
}

impl SearchType {
    /// 要搜哪几类。`all` 就是把三类依次来一遍。
    ///
    /// 顺序是「单曲 → 歌单 → 歌手」：单曲最常用；歌单结果是拿去 `scan` 的，
    /// 歌手结果是拿去接着查的，都算下一步动作，放在后面不打断主要目的。
    fn kinds(self) -> Vec<SearchKind> {
        match self {
            SearchType::All => SearchKind::ALL.to_vec(),
            SearchType::Song => vec![SearchKind::Song],
            SearchType::Playlist => vec![SearchKind::Playlist],
            SearchType::Artist => vec![SearchKind::Artist],
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\n[失败] {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut cfg = Config::load(cli.data_dir.clone())?;
    let mut dirty = false;

    if let Some(out) = cli.out {
        cfg.out_root = out;
        dirty = true;
    }
    if let Some(raw) = cli.quality.as_deref() {
        let q = Quality::parse(raw).ok_or_else(|| {
            let options = Quality::ALL
                .iter()
                .map(|q| q.as_str())
                .collect::<Vec<_>>()
                .join(" / ");
            anyhow!("未知音质档位 `{raw}`，可选: {options}")
        })?;
        if cfg.quality != q {
            cfg.quality = q;
            dirty = true;
        }
    }
    if dirty {
        cfg.save()?;
    }

    // 旧版本把 cookie 明文写在 config.json 里，Config::load 已经把它搬进凭据文件了。
    // 这件事必须说出来——用户以为自己改的是那个文件，得让他知道位置变了。
    if cfg.cookie_origin == Origin::Migrated {
        println!(
            "凭据已迁移到 {}（config.json 里的那份明文已清除）",
            auth::path(&cfg.data_dir).display()
        );
    }

    match cli.command {
        Command::Login {
            cookie,
            cookie_file,
            qr,
            qr_timeout,
        } => cmd_login(
            &cfg,
            LoginArgs {
                cookie,
                cookie_file,
                qr,
                qr_timeout,
            },
        ),
        Command::Logout => cmd_logout(&cfg),
        Command::Whoami => cmd_whoami(&cfg),
        Command::Info => cmd_info(&cfg),
        Command::Serve { listen, token, no_auth } => cmd_serve(cli.data_dir, listen, token, no_auth),
        Command::Scan { playlist_id } => cmd_scan(&cfg, playlist_id),
        Command::Playlists {
            remote,
            uid,
            limit,
        } => cmd_playlists(&cfg, remote, uid, limit),
        Command::Search {
            keyword,
            kind,
            limit,
            offset,
            fetch,
        } => cmd_search(&cfg, &keyword, kind, limit, offset, fetch),
        Command::Artist {
            artist_id,
            limit,
            fetch,
        } => cmd_artist(&cfg, artist_id, limit, fetch),
        Command::Tracks { playlist_id, limit } => cmd_tracks(&cfg, playlist_id, limit),
        Command::Daily { limit, fetch } => cmd_daily(&cfg, limit, fetch),
        Command::Url { song_id } => cmd_url(&cfg, &song_id),
        Command::Play { song_id, force } => cmd_play(&cfg, &song_id, force),
        Command::Mount {
            mountpoint,
            fuse_mode,
            allow_other,
            threads,
        } => cmd_mount(
            &cfg,
            MountArgs {
                mountpoint,
                ondemand: fuse_mode == MountMode::Ondemand,
                allow_other,
                threads,
            },
        ),
    }
}

// ---------- 登录 ----------

fn cmd_login(cfg: &Config, args: LoginArgs) -> Result<()> {
    if args.qr {
        return login_by_qr(cfg, args.qr_timeout);
    }

    let raw = read_cookie_input(&args)?;
    let cred = Credentials::parse(&raw)?;
    let saved = auth::save(&cfg.data_dir, cred.as_str())?;

    println!("凭据已保存到 {}", saved.display());
    #[cfg(unix)]
    println!("权限       600（仅属主可读）");
    println!("内容       {}", cred.masked());
    if !cred.dropped().is_empty() {
        // 重名通常是 __csrf（不同 domain/path 各一份），无害；
        // 但万一是 MUSIC_U，下面那句提示就得改口径，所以这里必须如实报出来
        println!(
            "去重       忽略 {} 条重名 cookie（{}）",
            cred.dropped().len(),
            cred.dropped().join("、")
        );
    }
    println!();

    // 保存成功不等于凭据有效，顺手联网确认一次。
    // 这一步失败不影响已经保存的凭据，所以只警告不报错。
    match NeteaseClient::new(cfg)?.account() {
        Ok(Some(acc)) => print_account(&acc),
        Ok(None) => {
            println!("⚠ 凭据已保存，但账号接口没认出登录态。");
            if cred
                .dropped()
                .iter()
                .any(|k| k.eq_ignore_ascii_case(auth::LOGIN_KEY))
            {
                println!("  你贴进来的内容里有多个 {}，这里保留了第一条——", auth::LOGIN_KEY);
                println!("  很可能留错了，换浏览器里 `.music.163.com` 那条再试一次。");
            } else {
                println!("  多半是 MUSIC_U 复制不全或已过期，重新登录一次再试。");
            }
        }
        Err(e) => println!("⚠ 已保存，但联网确认失败（不影响后续使用）：{e:#}"),
    }
    Ok(())
}

/// 从 `--cookie` / `--cookie-file` / 标准输入三处之一取 cookie。
///
/// 管道那条路是刻意留的：`pbpaste | musicm login` 不会把凭据留在 shell 历史里。
fn read_cookie_input(args: &LoginArgs) -> Result<String> {
    if let Some(raw) = &args.cookie {
        return Ok(raw.clone());
    }
    if let Some(path) = &args.cookie_file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("读取 cookie 文件失败: {}", path.display()));
    }
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("从标准输入读取 cookie 失败")?;
        if !buf.trim().is_empty() {
            return Ok(buf);
        }
    }
    bail!(
        "没有拿到 cookie。三种给法：\n\
         1) musicm login --qr            扫码登录（推荐）\n\
         2) pbpaste | musicm login       浏览器里复制后管道进来\n\
         3) musicm login --cookie 'MUSIC_U=...'（会留在 shell 历史里）"
    )
}

/// 扫码登录。
///
/// 全程走**不加密**的 `/api/login/qrcode/*`，所以不需要自建服务、也不需要在 Rust
/// 里实现 weapi/eapi 加密（那件事仍然留给自建服务）。
fn login_by_qr(cfg: &Config, timeout_secs: u64) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let (key, url) = client.qr_key()?;

    for line in qr::render_lines(&url)? {
        println!("{line}");
    }
    println!();
    println!("用网易云音乐 App 扫码授权（我 → 右上角 → 扫一扫）");
    println!("二维码内容 {url}");
    println!("向上滚动要小心：二维码滚出屏幕就没法扫了\n");

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut seen: Option<QrState> = None;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(2));
        let poll = client.qr_poll(&key)?;

        // 状态没变就不刷屏，只在跳变时吭声
        if seen != Some(poll.state) {
            match poll.state {
                QrState::Waiting => println!("等待扫码…"),
                QrState::Scanned => println!("已扫码，请在手机上确认登录"),
                // 没见过的码要把原始返回亮出来：不然接口一变，用户就只能干等满超时
                QrState::Unrecognized => println!(
                    "接口返回了没见过的状态（code={}，{}），继续等待…",
                    poll.code,
                    poll.message.trim()
                ),
                _ => {}
            }
            seen = Some(poll.state);
        }

        match poll.state {
            QrState::Confirmed => {
                let raw = poll
                    .cookie
                    .ok_or_else(|| anyhow!("授权成功，但接口没有下发凭据（Set-Cookie 是空的）"))?;
                let cred = Credentials::parse(&raw).map_err(|e| {
                    anyhow!("接口下发的凭据不完整：{e:#}。原始内容长度 {} 字符", raw.len())
                })?;
                let saved = auth::save(&cfg.data_dir, cred.as_str())?;

                println!("\n登录成功");
                println!("凭据已保存到 {}", saved.display());
                match client.account() {
                    Ok(Some(acc)) => print_account(&acc),
                    _ => println!("账号       （已保存，但这次没读到账号信息）"),
                }
                return Ok(());
            }
            QrState::Expired => bail!("二维码已过期，重新执行 `musicm login --qr`"),
            // 终止态：再等下去只是把超时耗光，立刻停下来给替代方案
            QrState::Blocked => return Err(risk_control_error(poll.code, &poll.message)),
            // 不变式：上面三个覆盖了所有终止态。以后 `QrState` 新增一个终止态却忘了
            // 在这里处理，就会退化成「傻等满超时」——正是 8821 那个 bug 的形状。
            ongoing => debug_assert!(
                !ongoing.is_terminal(),
                "有终止态漏了处理: {ongoing:?}"
            ),
        }
    }
    bail!("等待 {timeout_secs} 秒仍未确认，二维码已作废。重新执行 `musicm login --qr`")
}

/// 8821 的说明。
///
/// 这是个「换个办法」的错误，不是「再试一次」的错误：网易云判定请求不像官方客户端，
/// 要求过行为验证码（滑块/点选），而第三方客户端拿不到那个验证码——所以重扫多少次
/// 都会在**扫码之后**被同样拦下。必须一次性把可用的替代路径讲清楚，
/// 否则用户只会反复重扫，然后以为是自己账号的问题。
fn risk_control_error(code: i32, message: &str) -> anyhow::Error {
    anyhow!(
        "登录被网易云风控拦下（code={code}，{}）。\n\
         \n\
         这不是二维码过期，重扫不会变好。网易云要求过「行为验证码」（滑块/点选），\n\
         而命令行/第三方客户端拿不到它，所以每次都会在扫码成功那一刻被拒。\n\
         \n\
         改走 cookie，这条路不受风控影响：\n\
         1. 浏览器登录 https://music.163.com，确认是已登录状态；\n\
         2. F12 → Application → Cookies → https://music.163.com；\n\
         3. **只要 MUSIC_U 这一条**（值很长，别复制错）；\n\
         4. musicm login --cookie 'MUSIC_U=...'\n\
         \n\
         __csrf 是给加密接口用的，我们走明文接口，不需要它。\n\
         那一栏经常有好几个值（不同 domain/path 各一份），工具会自动去重，\n\
         所以整段抄下来也不会出错。\n\
         凭据有效期通常几十天，过期后再抄一次即可。",
        message.trim()
    )
}

fn print_account(acc: &crate::netease::Account) {
    println!("账号       {}（uid {}）", acc.nickname, acc.uid);
    println!(
        "会员       {}",
        match acc.vip_type {
            0 => "非会员（会员曲目仍拿不到链接）".to_string(),
            other => format!("vipType={other}"),
        }
    );
}

fn cmd_logout(cfg: &Config) -> Result<()> {
    let target = auth::path(&cfg.data_dir);
    if auth::clear(&cfg.data_dir)? {
        println!("已删除 {}", target.display());
    } else {
        println!("本来就没有保存过凭据");
    }
    if cfg.cookie_origin == Origin::Env {
        println!("提醒：MUSICM_COOKIE 环境变量仍然生效，本次运行依旧带着登录态");
    }
    Ok(())
}

fn cmd_whoami(cfg: &Config) -> Result<()> {
    println!("凭据来源   {}", cfg.cookie_origin.label());
    if cfg.cookie.is_some() {
        println!("凭据内容   {}", cfg.describe_login());
    }
    println!("音源接入   {}", cfg.api.describe());
    println!();

    let client = NeteaseClient::new(cfg)?;
    match client.account()? {
        Some(acc) => {
            println!("状态       已登录");
            print_account(&acc);
            Ok(())
        }
        None => {
            // 网易云不会明说「凭据过期」，只把 profile 返回成 null，
            // 所以这里也分不清到底是没配还是过期了，只能把两种可能都摆出来。
            println!("状态       未登录");
            if cfg.cookie.is_some() {
                println!("原因       凭据没被认可，多半已过期。重新执行 `musicm login --qr`");
            } else {
                println!("原因       没有配置凭据。`musicm login --qr` 可以扫码登录");
            }
            Ok(())
        }
    }
}

// ---------- 子命令实现 ----------

/// `musicm serve`：把命令行能做的事搬到浏览器里。
///
/// 它会一直占着终端（按 Ctrl-C 停止），所以启动前先把要用的信息打清楚：
/// 数据目录、音乐目录、以及带令牌的访问链接。
fn cmd_serve(
    data_dir: Option<PathBuf>,
    listen: String,
    token: Option<String>,
    no_auth: bool,
) -> Result<()> {
    if no_auth && !is_loopback_addr(&listen) {
        println!(
            "⚠ 用 --no-auth 把一个不需要登录的界面暴露在网络上。\n\
             这个界面能改凭据、能往音乐库里写文件，确认局域网可信再继续。\n"
        );
    }
    crate::web::serve(crate::web::ServeOptions {
        data_dir,
        listen,
        token,
        no_auth,
    })
}

/// 监听地址是不是只听本机。用来决定要不要为 `--no-auth` 打个警告。
fn is_loopback_addr(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn cmd_info(cfg: &Config) -> Result<()> {
    let index = Index::load(&cfg.index_path())?;
    let stats = index.stats();

    println!("配置");
    println!("  数据目录   {}", cfg.data_dir.display());
    println!("  音乐目录   {}", cfg.out_root.display());
    println!("  配置文件   {}", cfg.config_path().display());
    println!("  索引文件   {}", cfg.index_path().display());
    println!("  音源接入   {}", cfg.api.describe());
    println!("  音质档位   {}", cfg.quality.as_str());
    println!("  凭据来源   {}", cfg.cookie_origin.label());
    println!("  凭据内容   {}", cfg.describe_login());
    if cfg.cookie.is_some() {
        println!("  凭据文件   {}", auth::path(&cfg.data_dir).display());
    }
    if !cfg.has_login() {
        println!("  提示       没有登录态时只能取到免费档曲目；`musicm login --qr` 可扫码登录");
    }
    if cfg.cookie_origin.is_ephemeral() {
        println!("  注意       这份凭据来自本次运行的参数/环境变量，换个终端就没了");
    }

    println!("\n索引");
    println!("  歌单       {}", stats.playlists);
    println!("  曲目       {}", stats.tracks);
    if stats.tracks > 0 {
        println!(
            "  会员受限   {} 首（占 {:.0}%）",
            stats.vip_only,
            stats.vip_only as f64 / stats.tracks as f64 * 100.0
        );
    }
    println!(
        "  已落地     {} 个文件，共 {:.1} MB",
        stats.files,
        stats.cached_bytes as f64 / 1024.0 / 1024.0
    );
    Ok(())
}

fn cmd_scan(cfg: &Config, playlist_id: u64) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let mut index = Index::load(&cfg.index_path())?;

    println!("扫描歌单 {playlist_id} ...");
    let (playlist, tracks) = client.playlist_detail(playlist_id)?;

    let vip = tracks.iter().filter(|t| t.vip_only()).count();
    let total_secs: u64 = tracks.iter().map(|t| t.duration_secs()).sum();

    let outcome = index.upsert_playlist(playlist.clone(), tracks);
    index.save(&cfg.index_path())?;

    println!("\n  歌单     {}", playlist.name);
    println!("  创建者   {}", playlist.creator);
    println!(
        "  曲目     接口声明 {} 首，实际取到 {} 首",
        playlist.declared_count, outcome.total
    );
    if playlist.declared_count > outcome.total {
        println!(
            "  提示     少 {} 首，可能是歌单里存在已下架曲目",
            playlist.declared_count - outcome.total
        );
    }
    println!("  时长     约 {} 小时", total_secs / 3600);
    println!(
        "  受限     {} 首需要会员/付费（{}）",
        vip,
        if cfg.has_login() {
            "已配置 cookie，应该能取到"
        } else {
            "当前无 cookie，播放会失败"
        }
    );
    println!(
        "  索引     新增 {} / 更新 {}，累计 {} 首曲目",
        outcome.added,
        outcome.updated,
        index.stats().tracks
    );
    println!("  接口     本次请求 {} 次", client.requests_made());

    let head: Vec<_> = index.playlist_tracks(&playlist.key()).into_iter().take(3).collect();
    if !head.is_empty() {
        println!("\n  前 3 首：");
        for t in head {
            println!(
                "    {:>10}  {} - {}  [{}]",
                t.id,
                t.name,
                t.artist_line(),
                t.duration_label()
            );
        }
    }
    Ok(())
}

fn cmd_playlists(cfg: &Config, remote: bool, uid: Option<u64>, limit: usize) -> Result<()> {
    if remote {
        return cmd_remote_playlists(cfg, uid, limit);
    }

    let index = Index::load(&cfg.index_path())?;
    if index.playlists().next().is_none() {
        println!("索引里还没有歌单，先执行: musicm scan <歌单id>");
        println!("不知道有哪些歌单可扫？`musicm playlists --remote` 会列出你账号里的全部歌单。");
        return Ok(());
    }
    println!("{}  {}", pad("键", 16), pad("名称", 34));
    for pl in index.playlists() {
        println!(
            "{}  {}  {:>4} 首   {}",
            pad(&pl.key(), 16),
            pad(&truncate(&pl.name, 32), 34),
            pl.track_ids.len(),
            pl.creator
        );
    }
    println!();
    println!("提示   `--remote` 可以列出账号里的歌单（含还没扫描的）");
    Ok(())
}

/// 列出账号里的歌单。
///
/// uid 缺省取登录账号的：`/user/playlist` 要 uid，而「我」这个概念只在登录态下成立。
/// 匿名其实也能查别人的公开歌单，所以给了 `--uid` 就允许不登录——
/// 但必须把「私密歌单看不见」说清楚，否则用户会以为自己的歌单丢了。
fn cmd_remote_playlists(cfg: &Config, uid: Option<u64>, limit: usize) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;

    let (uid, who) = match uid {
        Some(id) => (id, format!("uid {id}")),
        None => {
            let acc = client.account()?.ok_or_else(|| {
                anyhow!(
                    "没有登录态就读不出「自己」是谁。两种办法：\n\
                     1) musicm login --cookie 'MUSIC_U=...'  配置凭据后重试\n\
                     2) musicm playlists --remote --uid <uid>  直接指定要查哪个账号"
                )
            })?;
            (acc.uid, format!("{}（uid {}）", acc.nickname, acc.uid))
        }
    };

    let got = client.user_playlists(uid, limit, 0)?;
    if got.playlists.is_empty() {
        // 空结果也要能解释：数组字段不在 = 接口结构变了，不是「他没有歌单」
        match got.shape {
            ListShape::Missing => println!(
                "接口没有返回歌单列表（code={}），多半是接口结构变了。",
                got.code
            ),
            _ => println!("uid {} 名下没有可见的歌单。", got.uid),
        }
        return Ok(());
    }

    let index = Index::load(&cfg.index_path())?;
    println!("{who} 的歌单（{} 张）", got.playlists.len());
    println!(
        "{}  {}  {}  {}  {}",
        pad("id", 12),
        pad("名称", 34),
        pad("曲目", 4),
        pad("标记", 14),
        "索引"
    );
    for pl in &got.playlists {
        let mut marks: Vec<&str> = Vec::new();
        if pl.is_favorite() {
            marks.push("我喜欢");
        }
        if pl.is_private() {
            marks.push("私密");
        }
        // 只有明确说「收藏了」才标：匿名时接口给的是 null（不知道），
        // 把它显示成「没收藏」是编造出来的信息
        if pl.subscribed == Some(true) {
            marks.push("收藏");
        }
        println!(
            "{}  {}  {:>4}  {}  {}",
            pad(&pl.id.to_string(), 12),
            pad(&truncate(&pl.name, 32), 34),
            pl.track_count,
            pad(&marks.join("/"), 14),
            if index.playlist(&pl.key()).is_some() {
                "已索引"
            } else {
                "未扫描"
            }
        );
    }

    println!();
    if got.more {
        println!("  提示     接口还有下一页，加大 --limit 再取一次");
    }
    if !cfg.has_login() {
        println!("  注意     当前没有登录态，只列得出公开歌单；");
        println!("           私密歌单要在 `musicm login` 之后才看得见");
    }
    println!("  下一步   musicm scan <id> 把歌单加入索引");
    Ok(())
}

/// `musicm search`：一次可以搜歌单 / 单曲 / 歌手。
///
/// 三类结果落在不同的下一步动作上，所以分三块打印而不是混成一张表——
/// 单曲能直接落地、歌单要 `scan`、歌手要接着看热门曲目。
fn cmd_search(
    cfg: &Config,
    keyword: &str,
    kind: SearchType,
    limit: usize,
    offset: usize,
    fetch: usize,
) -> Result<()> {
    let kw = keyword.trim();
    if kw.is_empty() {
        bail!("关键词是空的。用法: musicm search 晴天");
    }
    let kinds = kind.kinds();
    if fetch > 0 && !kinds.contains(&SearchKind::Song) {
        bail!("--fetch 只对单曲有意义，配上 `--type song` 或 `--type all` 才用得上");
    }

    let client = NeteaseClient::new(cfg)?;
    let index = Index::load(&cfg.index_path())?;
    let mut song_hits: Vec<Track> = Vec::new();

    for (i, k) in kinds.iter().copied().enumerate() {
        if i > 0 {
            println!();
        }
        match k {
            SearchKind::Song => {
                let out = client.search_songs(kw, offset, limit)?;
                print_song_hits(&index, &out, offset);
                song_hits = out.items;
            }
            SearchKind::Playlist => {
                let out = client.search_playlists(kw, offset, limit)?;
                print_playlist_hits(&index, &out, offset);
            }
            SearchKind::Artist => {
                let out = client.search_artists(kw, offset, limit)?;
                print_artist_hits(&out, offset);
            }
        }
    }

    if fetch > 0 {
        fetch_search_hits(cfg, &client, &song_hits, fetch)?;
    }
    Ok(())
}

/// 空结果也要能解释，而且「真没搜到」和「接口改版」必须分开说。
///
/// 实测：搜索无命中时网易云会把结果数组整个省掉，只留一个 `xxxCount: 0`
/// （`{"result":{"playlistCount":0},"code":200}`）。若把「数组不在」一律当成
/// 接口改版，用户搜一个生僻词就会被告知「结构变了」，然后去改代码。
fn explain_empty_search(kind: SearchKind, shape: ListShape, code: i32, offset: usize) {
    match shape {
        // 两样都缺才叫改版（判据见 `ListShape`）：这是「去改代码」的信号，
        // 不是「换个关键词」，所以必须和下面两档分开说。
        ListShape::Missing => println!(
            "{}：{}（code={code}），多半是接口结构变了。",
            kind.label(),
            shape.label()
        ),
        // 翻了页才空，和一开始就空是两件事
        _ if offset > 0 => println!(
            "{}：没有更多了（已经翻到第 {offset} 条之后）。",
            kind.label()
        ),
        _ => println!("{}：没有匹配的结果。", kind.label()),
    }
}

fn print_song_hits(index: &Index, out: &SearchOutcome<Track>, offset: usize) {
    if out.items.is_empty() {
        return explain_empty_search(SearchKind::Song, out.shape, out.code, offset);
    }
    println!(
        "单曲（命中 {} 条，显示第 {} - {} 条）",
        out.total,
        offset + 1,
        offset + out.items.len()
    );
    for (i, t) in out.items.iter().enumerate() {
        println!(
            "{:>3}. {:>10}  {}  {}  {:>6}  {}  {}",
            offset + i + 1,
            t.id,
            pad(&truncate(&t.name, 30), 32),
            pad(&truncate(&t.artist_line(), 20), 22),
            t.duration_label(),
            if t.vip_only() { "会员" } else { "免费" },
            if index.file_of(&t.key()).is_some() {
                "已落地"
            } else {
                "未下载"
            }
        );
    }
    println!("  提示     musicm play <id> 落地某一首，或加 --fetch N 一次下前 N 首");
}

fn print_playlist_hits(index: &Index, out: &SearchOutcome<PlaylistBrief>, offset: usize) {
    if out.items.is_empty() {
        return explain_empty_search(SearchKind::Playlist, out.shape, out.code, offset);
    }
    println!(
        "歌单（命中 {} 条，显示第 {} - {} 条）",
        out.total,
        offset + 1,
        offset + out.items.len()
    );
    for (i, pl) in out.items.iter().enumerate() {
        println!(
            "{:>3}. {:>12}  {}  {:>5} 首  {}  {}",
            offset + i + 1,
            pl.id,
            pad(&truncate(&pl.name, 30), 32),
            pl.track_count,
            pad(&truncate(&pl.creator, 12), 14),
            if index.playlist(&pl.key()).is_some() {
                "已索引"
            } else {
                "未扫描"
            }
        );
    }
    println!("  提示     musicm scan <id> 把它加入索引");
}

fn print_artist_hits(out: &SearchOutcome<ArtistBrief>, offset: usize) {
    if out.items.is_empty() {
        return explain_empty_search(SearchKind::Artist, out.shape, out.code, offset);
    }
    println!(
        "歌手（命中 {} 条，显示第 {} - {} 条）",
        out.total,
        offset + 1,
        offset + out.items.len()
    );
    for (i, a) in out.items.iter().enumerate() {
        let alias = if a.alias.is_empty() {
            String::new()
        } else {
            format!("（{}）", a.alias.join(" / "))
        };
        println!(
            "{:>3}. {:>10}  {}  {} 首  {} 张专辑",
            offset + i + 1,
            a.id,
            pad(&truncate(&format!("{}{alias}", a.name), 32), 34),
            a.song_count,
            a.album_count
        );
    }
    println!("  提示     musicm artist <id> 看他的热门曲目（可加 --fetch N 落地）");
}

/// 落地搜到的单曲。
///
/// 先补一次曲目详情再下：搜索接口的专辑对象里只有 `picId`、**没有 `picUrl`**，
/// 直接拿搜索结果去下会丢封面，而飞牛的刮削正是靠封面和标签。
fn fetch_search_hits(
    cfg: &Config,
    client: &NeteaseClient,
    hits: &[Track],
    count: usize,
) -> Result<()> {
    let ids: Vec<u64> = hits.iter().take(count).map(|t| t.id).collect();
    if ids.is_empty() {
        println!();
        println!("没有可落地的单曲（本次搜索结果为空）。");
        return Ok(());
    }
    let detailed = client.songs_detail(&ids)?;
    if detailed.is_empty() {
        println!();
        println!("补曲目详情时接口什么都没返回，取消落地。");
        return Ok(());
    }
    fetch_many(cfg, client, &detailed, None, detailed.len(), "搜索结果")
}

/// `musicm artist`：某位歌手的热门曲目。
///
/// 热门曲目是接口按热度给的、不属于任何专辑或歌单，所以落地时统一进「单曲」目录。
fn cmd_artist(cfg: &Config, artist_id: u64, limit: usize, fetch: usize) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let (code, tracks) = client.artist_top_songs(artist_id)?;

    if tracks.is_empty() {
        println!("歌手 {artist_id} 没有返回曲目（接口 code={code}）。");
        println!("  确认 id 是否正确：musicm search <歌手名> --type artist");
        return Ok(());
    }

    let index = Index::load(&cfg.index_path())?;
    let shown = limit.min(tracks.len());
    println!("歌手 {artist_id} 的热门曲目（共 {} 首，显示前 {shown} 首）", tracks.len());
    for (i, t) in tracks.iter().take(shown).enumerate() {
        println!(
            "{:>3}. {:>10}  {}  {}  {:>6}  {}  {}",
            i + 1,
            t.id,
            pad(&truncate(&t.name, 30), 32),
            pad(&truncate(&t.artist_line(), 20), 22),
            t.duration_label(),
            if t.vip_only() { "会员" } else { "免费" },
            if index.file_of(&t.key()).is_some() {
                "已落地"
            } else {
                "未下载"
            }
        );
    }

    if fetch > 0 {
        fetch_many(cfg, &client, &tracks, None, fetch, "歌手热门")?;
    } else {
        println!();
        println!("  提示     musicm play <id> 落地某一首，或加 --fetch N 一次下前 N 首");
    }
    Ok(())
}

fn cmd_tracks(cfg: &Config, playlist_id: u64, limit: usize) -> Result<()> {
    let index = Index::load(&cfg.index_path())?;
    let key = store::playlist_key_for("netease", playlist_id);
    let Some(playlist) = index.playlist(&key) else {
        return Err(anyhow!(
            "索引里没有歌单 {playlist_id}，先执行: musicm scan {playlist_id}"
        ));
    };
    let tracks = index.playlist_tracks(&key);

    println!(
        "{}（{} 首，显示前 {} 首）",
        playlist.name,
        tracks.len(),
        limit.min(tracks.len())
    );
    for (i, t) in tracks.iter().take(limit).enumerate() {
        let mark = if t.vip_only() { "会员" } else { "免费" };
        let state = match index.file_of(&t.key()) {
            Some(f) => format!("已落地 {}", f.size_label()),
            None => "未下载".to_string(),
        };
        println!(
            "{:>3}. {:>10}  {}  {}  {:>6}  {}  {}",
            i + 1,
            t.id,
            pad(&truncate(&t.name, 32), 34),
            pad(&truncate(&t.artist_line(), 22), 24),
            t.duration_label(),
            mark,
            state
        );
    }
    Ok(())
}

/// `musicm daily`。
///
/// 刻意**不写索引**：日推每天都在变，把它做成歌单会让索引里堆着一次性的曲目，
/// 而且「今天扫描过、明天内容全变」会让缓存归属变得很难讲清楚。
/// 代价是它跟 `scan` 的产物不在一套账上，这一点在输出里明说。
fn cmd_daily(cfg: &Config, limit: usize, fetch_count: usize) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let daily = client.daily_songs()?;

    // 空结果必须先解释再返回：这个接口未登录时不报错，
    // 直接渲染成「今日推荐 0 首」会让用户去怀疑自己的账号。
    if daily.tracks.is_empty() {
        explain_empty_daily(cfg, &daily);
        return Ok(());
    }

    let index = Index::load(&cfg.index_path())?;
    let dir = cfg
        .out_root
        .join(naming::group_rel("netease", Some(naming::DAILY_GROUP)));
    let stems = naming::unique_stems(&daily.tracks);
    let on_disk = naming::landed_stems(&dir);
    let shown = limit.min(daily.tracks.len());

    println!("每日推荐（{} 首，显示前 {} 首）", daily.tracks.len(), shown);
    for (i, t) in daily.tracks.iter().take(shown).enumerate() {
        // 先问索引：这首歌可能早就随着某张歌单下过了；索引里没有再看日推目录。
        let state = match index.file_of(&t.key()) {
            Some(_) => "已落地",
            None if on_disk.contains(&stems[i]) => "已落地",
            None => "未下载",
        };
        println!(
            "{:>3}. {:>10}  {}  {}  {:>6}  {}  {}",
            i + 1,
            t.id,
            pad(&truncate(&t.name, 32), 34),
            pad(&truncate(&t.artist_line(), 22), 24),
            t.duration_label(),
            if t.vip_only() { "会员" } else { "免费" },
            state
        );
    }

    println!();
    println!("  提示     日推曲目不属于任何歌单，musicm info 统计不到它们；");
    println!("           `play <id>` 只认索引，会把同一首歌当单曲另取一份到 {}，",
        display_path(Path::new(&naming::group_rel("netease", None))));
    println!("           要落地就用 --fetch N，文件写进 {}", display_path(&dir));

    if fetch_count > 0 {
        fetch_many(
            cfg,
            &client,
            &daily.tracks,
            Some(naming::DAILY_GROUP),
            fetch_count,
            "每日推荐",
        )?;
    }
    Ok(())
}

/// 空日推的三种成因必须分开说，否则提示只会误导。
///
/// 实测：匿名的和失效的 cookie 都返回 `code=200` + 空列表，**没有任何错误码**；
/// 只有「字段整个不在响应里」才说明接口改版。把三者混成一句「今天没有推荐」，
/// 用户就会一直去检查自己的账号，而问题其实在凭据或接口上。
fn explain_empty_daily(cfg: &Config, daily: &DailyOutcome) {
    println!(
        "每日推荐没有返回曲目（接口 code={}，读到的是 {}）。",
        daily.code,
        daily.shape.label()
    );
    println!();

    match daily.shape {
        DailyShape::Missing => {
            println!("响应里没有可识别的曲目字段，多半是接口结构变了——");
            println!("这跟登录态无关，重试和重新登录都改变不了，得先核对接口。");
        }
        // 字段在、列表为空：这才是「没登录」的形状
        _ if cfg.has_login() => {
            println!("这不是「今天没有推荐」，而是凭据已经不被认可了：");
            println!("  1) musicm whoami                        确认它是否还有效");
            println!("  2) musicm login --cookie 'MUSIC_U=...'  失效就重新抄一份");
        }
        _ => {
            println!("每日推荐必须登录才能取：网易云对未登录的请求不报错，只回一个空列表。");
            println!("  1. 浏览器登录 https://music.163.com，确认右上角是已登录状态；");
            println!("  2. F12 → Application → Cookies → https://music.163.com；");
            println!("  3. 抄 MUSIC_U 那一条（值很长，别复制错）：musicm login --cookie 'MUSIC_U=...'");
            println!("（扫码登录会被网易云风控拦在扫码成功那一刻，别在这条路上耗时间）");
        }
    }
}

/// 把一批「不属于任何歌单」的曲目落地。
///
/// 每日推荐、搜索结果、歌手热门都是这一类。目录由 `naming::group_rel` 算，
/// 文件名主干走 `naming::unique_stems`——与 FUSE 出口、`play` 用的是同一套规则，
/// 同一首歌不会因为入口不同而落到两个地方（`group = None` 时是 `<音源>/单曲`）。
///
/// 只写文件、不动索引（原因见 `cmd_daily` 的说明）；已经在索引里的曲目直接跳过，
/// 免得同一首歌在歌单目录和单曲目录各存一份。
fn fetch_many(
    cfg: &Config,
    client: &NeteaseClient,
    tracks: &[Track],
    group: Option<&str>,
    count: usize,
    label: &str,
) -> Result<()> {
    let index = Index::load(&cfg.index_path())?;
    let dir = cfg.out_root.join(naming::group_rel("netease", group));
    let stems = naming::unique_stems(tracks);
    let want = count.min(tracks.len());
    let fetcher = Fetcher::new(client, cfg);

    println!();
    println!("落地前 {want} 首{label}到 {} ...", display_path(&dir));
    let (mut fresh, mut cached, mut failed) = (0usize, 0usize, 0usize);

    for (track, stem) in tracks.iter().take(want).zip(stems.iter()) {
        // 索引里有记录，说明它早就随某张歌单落过地了。这里不能只看目标路径：
        // 那样同一首歌会在歌单目录和单曲目录各下一份。
        if let Some(f) = index.file_of(&track.key()) {
            cached += 1;
            println!("  跳过  {}（已在库，{}）", track.name, f.size_label());
            continue;
        }
        match fetcher.ensure_stem(track, &dir, stem, false) {
            Ok(o) if o.from_cache => {
                cached += 1;
                println!("  跳过  {}（已在库中）", track.name);
            }
            Ok(o) => {
                fresh += 1;
                let name = o
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                println!("  完成  {}  ->  {name}", track.name);
                for w in &o.warnings {
                    println!("  警告  {name}：{w}");
                }
            }
            // 会员曲目、地区限制都会走到这里，说清楚是哪一首的什么问题
            Err(e) => {
                failed += 1;
                println!("  失败  {}：{e:#}", track.name);
            }
        }
    }

    println!("  合计  新落地 {fresh} / 已在库 {cached} / 失败 {failed}");
    if fresh > 0 {
        println!(
            "  说明  这 {fresh} 个文件不属于任何歌单：musicm info 的统计看不到它们，"
        );
        println!("         FUSE 挂载会从磁盘扫到，同一首歌以后再取到会直接复用。");
    }
    Ok(())
}

/// 拼好的路径里混着 `/`（来自 `naming::group_rel`），打印前统一成平台分隔符。
///
/// 与 `vfs::abs` 的处理保持一致：同一个目录在挂载里和命令行里不该长得不一样。
fn display_path(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn cmd_url(cfg: &Config, song_id: &str) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let index = Index::load(&cfg.index_path())?;

    let (id, name) = match index.resolve_track(song_id) {
        Some(t) => (t.id, format!("{} - {}", t.name, t.artist_line())),
        None => {
            let id: u64 = song_id
                .parse()
                .with_context(|| format!("索引里没有 `{song_id}`，且它不是纯数字曲目 id"))?;
            (id, format!("曲目 {id}（索引中无元数据）"))
        }
    };

    let (outcome, notes) = client.resolve_url(id, cfg.quality)?;
    println!("{name}");
    let resolved = match outcome.info {
        Some(info) => {
            println!(
                "  档位     {} -> .{} / {} kbps",
                info.quality_label(),
                info.ext,
                info.kbps()
            );
            println!("  大小     {:.1} MB", info.size_mb());
            println!("  链接     {}", info.url);
            true
        }
        None => {
            println!("  结果     拿不到播放链接：{}", outcome.reason);
            false
        }
    };
    if !notes.is_empty() {
        // 成功时说明发生了降级；全部失败时只是尝试记录。
        let label = if resolved { "降级" } else { "尝试" };
        println!("  {label}     {}", notes.join(" | "));
    }
    Ok(())
}

fn cmd_play(cfg: &Config, song_id: &str, force: bool) -> Result<()> {
    let client = NeteaseClient::new(cfg)?;
    let mut index = Index::load(&cfg.index_path())?;

    // 索引里没有也能放：`search` / `artist` / `daily` 给出的都是裸数字 id，
    // 缺了这一步，搜到的歌就只能看不能听。元数据现取一次即可——
    // `cmd_url` 早就是这么做的，两条路的口径不该不一样。
    let (track, from_index) = match index.resolve_track(song_id).cloned() {
        Some(t) => (t, true),
        None => {
            let id: u64 = song_id.trim().parse().with_context(|| {
                format!(
                    "索引里找不到 `{song_id}`，它也不是纯数字曲目 id。\n\
                     先用 `musicm search <关键词>` 找到 id，或 scan 一次歌单。"
                )
            })?;
            let t = client
                .songs_detail(&[id])?
                .pop()
                .ok_or_else(|| anyhow!("接口没有返回曲目 {id} 的详情，id 可能不存在或已下架"))?;
            (t, false)
        }
    };

    let playlist = index.owning_playlist(track.id).cloned();
    println!(
        "播放 {} - {}{}",
        track.name,
        track.artist_line(),
        match &playlist {
            Some(p) => format!("  （歌单：{}）", p.name),
            None => String::new(),
        }
    );
    if !from_index {
        // 说清楚会落到哪：没有歌单归属就是「单曲」目录，和 `search --fetch` 是同一个地方。
        // 也提醒一句重复的可能——用 `daily --fetch` 取过的文件不在索引里，这里认不出来。
        println!(
            "  说明     这首歌不在索引里（现场取回的元数据），会落到 {}",
            display_path(Path::new(&naming::group_rel("netease", None)))
        );
    }

    let fetcher = Fetcher::new(&client, cfg);
    // 把整张歌单的曲目一起传进去，这样重复收录的曲目会拿到和 FUSE 出口一致的文件名
    let siblings = playlist
        .as_ref()
        .map(|p| index.playlist_tracks(&p.key()));
    let outcome = fetcher.fetch(&track, playlist.as_ref(), siblings.as_deref(), force)?;

    if outcome.from_cache {
        println!("  状态     已在库中，跳过下载（--force 可强制重下）");
    } else {
        println!("  音质     {} -> .{}", outcome.quality, outcome.ext);
    }
    // 路径里混着 `/`（来自 `naming::group_rel`），过一遍 display_path 才不会出现
    // `网易云/单曲\01 xxx.mp3` 这种半截分隔符——挂载里看到的样子就是它该有的样子。
    println!("  文件     {}", display_path(&outcome.path));
    println!("  大小     {:.1} MB", outcome.bytes as f64 / 1024.0 / 1024.0);

    if !outcome.from_cache {
        let tag_note = if outcome.tag.tag_type.is_empty() {
            "未打标".to_string()
        } else {
            format!(
                "{}（封面{} / 歌词{}）",
                outcome.tag.tag_type,
                if outcome.tag.cover_embedded { "已内嵌" } else { "无" },
                if outcome.tag.lyrics_embedded { "已内嵌" } else { "无" }
            )
        };
        println!("  标签     {tag_note}");
    }
    if let Some(lrc) = &outcome.lrc_path {
        println!("  歌词     {}", display_path(lrc));
    }
    if !outcome.notes.is_empty() {
        println!("  降级     {}", outcome.notes.join(" | "));
    }
    for w in &outcome.warnings {
        println!("  警告     {w}");
    }

    // 现取元数据的曲目（搜索结果点播）本来不在索引里，先补一条元数据记录：
    // 缺了这一步，落地之后 `info` 统计不到它、FUSE 的树里也看不到它。
    if !from_index {
        index.remember_track(track.clone());
    }
    index.record_file(outcome.to_cached_file(&track));
    index.save(&cfg.index_path())?;
    Ok(())
}

/// 挂载。
///
/// 建树、摘要打印这些步骤是平台无关的，所以放在这里；
/// 只有「把树交给内核」那一步按平台分叉（见 `mount_now`）。
fn cmd_mount(cfg: &Config, args: MountArgs) -> Result<()> {
    let index = Index::load(&cfg.index_path())?;
    let indexed = index.stats().tracks;

    let vfs = vfs::Vfs::build(&cfg.out_root, &index, cfg.quality, args.ondemand);
    let tree = vfs.stats();

    // 判据是「树里到底有没有东西」，不是「索引里有没有曲目」。
    //
    // 以前这里要求索引非空，理由是「你大概忘了 scan」。但 daily / search / play
    // 这三条落地路径都**刻意不进索引**（见 fetch_many），所以一个只装了日推的库
    // 是完全合法的挂载对象——cached 模式本来就是纯磁盘扫描。那个检查会把这种
    // 用法一并拒掉，而拒绝的理由（"先 scan"）对用户来说是答非所问。
    if tree.files == 0 && indexed == 0 {
        return Err(anyhow!(
            "没有可挂载的内容：{} 里没有任何文件，索引里也没有曲目。\n\
             先执行 musicm scan <歌单id>，或者用 musicm daily --fetch N / \
             musicm play <曲目id> 落地几个文件。",
            cfg.out_root.display()
        ));
    }

    println!(
        "挂载 musicm（{}）",
        if args.ondemand {
            "ondemand 按需取回"
        } else {
            "cached 仅已落盘"
        }
    );
    println!("  挂载点   {}", args.mountpoint.display());
    println!("  音乐库   {}", cfg.out_root.display());
    println!(
        "  内容     {} 个目录 / {} 个文件，已落盘 {:.1} MB",
        tree.dirs,
        tree.files,
        tree.bytes as f64 / 1024.0 / 1024.0
    );
    if tree.pending > 0 {
        println!(
            "  未落盘   {} 个曲目，读取时才取回（音频的尺寸此刻是按码率估的）",
            tree.pending
        );
    }
    if args.ondemand && tree.pending > 0 {
        println!("  提醒     飞牛音乐整库扫描会逐个读元信息，等于把整张歌单下完。");
        println!("           想先扫完再听，用 --fuse-mode cached，或先 musicm play 预热。");
    }
    if indexed == 0 {
        println!(
            "  提醒     索引里没有曲目：这次挂的是纯磁盘内容（{} 个文件）。",
            tree.files
        );
        println!("           日推 / 搜索 / 单曲落地都属于这一类，它们本来就不进索引。");
        if args.ondemand {
            println!("           此刻没有可取回的曲目，ondemand 与 cached 效果相同。");
        }
    } else if tree.files == 0 {
        println!(
            "  提醒     cached 模式下现在什么都看不到：{} 个索引进来的曲目还都没落地。",
            indexed
        );
        println!("           想按需取用就换 --fuse-mode ondemand，或先 musicm play 预热。");
    }
    println!(
        "  更新     新落地的文件会在 {} 秒内自动出现，挂载期间不必重挂。",
        mount::REFRESH_SECS
    );
    if !args.allow_other {
        println!("  权限     仅当前用户可读，飞牛音乐以别的用户运行时扫不到，需要 --allow-other");
    }
    println!("  卸载     fusermount3 -u {}", args.mountpoint.display());
    println!();

    let materializer: Box<dyn vfs::Materializer> = if args.ondemand {
        Box::new(materialize::LibraryMaterializer::new(cfg, index)?)
    } else {
        Box::new(materialize::OfflineMaterializer)
    };

    mount::mount_now(cfg, args, vfs, materializer)
}

// ---------- 终端排版：中文是双宽字符，直接用 `{:<20}` 会歪 ----------

fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x20000..=0x3FFFD
    )
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// 按显示宽度截断，超长时以省略号结尾。
fn truncate(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > max_width.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// 按显示宽度补齐到指定列宽。
fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--type` 到接口 type 的映射，钉住一次。
    ///
    /// 网易云的 type 是数字（1 单曲 / 1000 歌单 / 100 歌手），
    /// 写错了不会报错，只会「搜什么都搜不到」——正是最难查的那种错。
    #[test]
    fn search_type_maps_to_the_api_kinds() {
        assert_eq!(SearchType::Song.kinds(), vec![SearchKind::Song]);
        assert_eq!(SearchType::Playlist.kinds(), vec![SearchKind::Playlist]);
        assert_eq!(SearchType::Artist.kinds(), vec![SearchKind::Artist]);
        assert_eq!(SearchType::All.kinds().len(), 3);
        assert!(SearchType::All.kinds().contains(&SearchKind::Song));

        assert_eq!(SearchKind::Song.type_code(), 1);
        assert_eq!(SearchKind::Playlist.type_code(), 1000);
        assert_eq!(SearchKind::Artist.type_code(), 100);
    }
}
