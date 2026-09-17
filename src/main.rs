//! musicm —— 把网易云音乐的歌单变成本地音乐库，供飞牛音乐扫描播放。
//!
//! 当前里程碑：歌单扫描 + 单曲落地 + FUSE 出口。
//!
//! 分层：`netease` 管音源，`store` 管索引，`naming` 管路径命名，
//! `fetch` 管落地，`vfs` 管虚拟文件树，`fuse_fs` 管把树挂到内核上。
//! 其中只有 `fuse_fs` 是 Linux 专属的。

mod config;
mod fetch;
mod model;
mod naming;
mod netease;
mod store;
mod tag;

// 这两个模块只有 Linux 出口（fuse_fs）真正调用，但它们在所有平台上都会被编译：
// 一来类型错误能立刻在开发机上暴露，二来它们的单元测试本来就该跨平台跑。
// 代价是非 Linux 构建下 rustc 会认为它们是死代码，这里明确放行。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod materialize;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod vfs;

#[cfg(target_os = "linux")]
mod fuse_fs;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{Config, Quality};
use crate::fetch::Fetcher;
use crate::model::Playlist;
use crate::netease::NeteaseClient;
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

    /// 网易云 cookie（至少包含 MUSIC_U）。给了就写入配置，后续沿用。
    #[arg(long, global = true)]
    cookie: Option<String>,

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
    /// 列出索引里的歌单
    Playlists,
    /// 列出某张歌单的曲目
    Tracks {
        playlist_id: u64,
        #[arg(long, default_value_t = 30)]
        limit: usize,
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
    /// 查看配置与索引状态
    Info,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum MountMode {
    /// 列出全部曲目，读到没落地的就去取
    Ondemand,
    /// 只列出已经落盘的，不联网
    Cached,
}

/// 挂载参数。抽出来是因为真正挂载的那一步在平台之间分叉，
/// 而这些参数两边都要拼。
struct MountArgs {
    mountpoint: PathBuf,
    ondemand: bool,
    allow_other: bool,
    /// 只有 Linux 下的 FUSE 事件循环用得到
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    threads: usize,
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
    if let Some(cookie) = cli.cookie {
        cfg.cookie = Some(cookie);
        dirty = true;
    }
    if dirty {
        cfg.save()?;
    }

    match cli.command {
        Command::Info => cmd_info(&cfg),
        Command::Scan { playlist_id } => cmd_scan(&cfg, playlist_id),
        Command::Playlists => cmd_playlists(&cfg),
        Command::Tracks { playlist_id, limit } => cmd_tracks(&cfg, playlist_id, limit),
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

// ---------- 子命令实现 ----------

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
    println!(
        "  登录态     {}",
        if cfg.has_login() {
            "已配置 cookie"
        } else {
            "未配置 cookie（只能取到免费档曲目）"
        }
    );

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

fn cmd_playlists(cfg: &Config) -> Result<()> {
    let index = Index::load(&cfg.index_path())?;
    if index.playlists().next().is_none() {
        println!("索引里还没有歌单，先执行: musicm scan <歌单id>");
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

    let track = index
        .resolve_track(song_id)
        .cloned()
        .ok_or_else(|| {
            anyhow!("索引里找不到 `{song_id}`。先 scan 一次歌单，或直接传曲目 id（纯数字）。")
        })?;

    let playlist = owning_playlist(&index, track.id).cloned();
    println!(
        "播放 {} - {}{}",
        track.name,
        track.artist_line(),
        match &playlist {
            Some(p) => format!("  （歌单：{}）", p.name),
            None => String::new(),
        }
    );

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
    println!("  文件     {}", outcome.path.display());
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
        println!("  歌词     {}", lrc.display());
    }
    if !outcome.notes.is_empty() {
        println!("  降级     {}", outcome.notes.join(" | "));
    }
    for w in &outcome.warnings {
        println!("  警告     {w}");
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
    if index.stats().tracks == 0 {
        return Err(anyhow!("索引里没有曲目，先执行: musicm scan <歌单id>"));
    }
    if !cfg.out_root.is_dir() {
        return Err(anyhow!(
            "音乐目录还不存在: {}。先执行一次 musicm play <曲目id>",
            cfg.out_root.display()
        ));
    }

    let vfs = vfs::Vfs::build(&cfg.out_root, &index, cfg.quality, args.ondemand);
    let tree = vfs.stats();

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

    mount_now(cfg, args, vfs, materializer)
}

#[cfg(target_os = "linux")]
fn mount_now(
    cfg: &Config,
    args: MountArgs,
    vfs: vfs::Vfs,
    materializer: Box<dyn vfs::Materializer>,
) -> Result<()> {
    let mountpoint = args.mountpoint.clone();
    let req = fuse_fs::MountRequest {
        mountpoint: mountpoint.clone(),
        vfs,
        out_root: cfg.out_root.clone(),
        materializer,
        ondemand: args.ondemand,
        allow_other: args.allow_other,
        threads: args.threads,
    };

    match fuse_fs::run(req) {
        Ok(()) => {
            println!("已卸载 {mountpoint}");
            Ok(())
        }
        Err(e) if args.allow_other => Err(anyhow!(
            "{e:#}\n\n\
             提示：如果错误里出现 allow_other / user_allow_other，说明 /etc/fuse.conf \
             还没放行。取消 `user_allow_other` 那一行的注释（需要 root）再挂一次；\
             或者先用 --allow-other=false 验证功能本身是否正常。"
        )),
        Err(e) => Err(e),
    }
}

/// 非 Linux 平台没有 FUSE。但文件树本身是平台无关的，把它算出来打印一下，
/// 至少能确认路径生成和曲目识别是否符合预期——这也让这些代码在所有平台上都被编译到。
#[cfg(not(target_os = "linux"))]
fn mount_now(
    _cfg: &Config,
    args: MountArgs,
    vfs: vfs::Vfs,
    _materializer: Box<dyn vfs::Materializer>,
) -> Result<()> {
    println!("（当前平台不是 Linux，无法真正挂载，下面是本地算出的文件树骨架）");
    for source in vfs.children(vfs::ROOT_INO) {
        println!("  {}/", source.name);
        for group in vfs.children(source.ino) {
            println!("    {}/   {} 项", group.name, vfs.children(group.ino).len());
        }
    }
    Err(anyhow!(
        "FUSE 只在 Linux 上可用，当前是 {}，因此没有挂载 {}。\n\
         飞牛 fnOS 就是 Debian/Linux：把源码拷过去 `cargo build --release` 即可。",
        std::env::consts::OS,
        args.mountpoint.display()
    ))
}

/// 找到这首歌所属的第一张歌单，用来决定它在音乐库里的目录位置。
fn owning_playlist<'a>(index: &'a Index, id: u64) -> Option<&'a Playlist> {
    index.playlists().find(|p| p.track_ids.contains(&id))
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
