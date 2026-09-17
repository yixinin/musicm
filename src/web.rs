//! Web 管理界面。把命令行已有的能力（搜索、登录、扫描、取回、挂载）套一层浏览器界面。
//!
//! # 为什么手写 HTTP 而不引 `tiny_http` / `axum`
//!
//! 这个程序要跑在飞牛 fnOS（Debian/aarch64）上，而项目一开始就定了两件事：
//! 依赖要纯 Rust（不碰 C 工具链），依赖树要小（编译一次要几分钟的 NAS 上很贵）。
//! 顺着这两条推下来，最好的选择就是**不引新依赖**：
//!
//! - `axum` 会拖进 `tokio` + `hyper` + `tower`，几十个 crate，aarch64 上首编耗时长；
//! - `tiny_http` 很轻，但为了一个「家里一个人点几下」的界面去加依赖，
//!   换来的只是少写 200 行，而它带来的 request/response 语义我们其实也只用到十分之一。
//!
//! 于是这里用 `std::net` 手写了一个 HTTP/1.1 服务器。它**明确不是**通用服务器，
//! 只打算服务自己的前端，所以刻意做了几个简化，且每个简化都写在实现处：
//!
//! - 只支持 `Connection: close`（一个连接一次请求）。浏览器的 `fetch()` 本来就会
//!   挑空闲连接，内网场景下多建一次 TCP 的代价可以忽略，而 keep-alive 要写对的
//!   状态机（半关闭、流水线、超时）远比这一次握手值钱。
//! - 只认 `Content-Length` 的请求体，不解析 chunked。前端一律发 JSON。
//! - 不支持 HTTPS。家庭内网用，需要外网访问时前面挂一层反代更合适。
//!
//! # 安全边界
//!
//! 这个界面能改凭据、能下载音乐，所以它默认**只监听 127.0.0.1**。
//! 一旦 `--listen` 到非回环地址，就必须带访问令牌：用户没给就自动生成一个，
//! 并把带令牌的链接打印出来（见 [`decide_token`]）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::auth::{self, Credentials};
use crate::config::Config;
use crate::fetch::Fetcher;
use crate::model::{now_millis, now_secs, Playlist, Track};
use crate::mount::{self, MountArgs};
use crate::naming;
use crate::netease::{ListShape, NeteaseClient, SearchKind};
use crate::store::Index;
use crate::vfs;

// 前端资源在编译期嵌进二进制：NAS 上拷过去就是一个文件，不需要再管目录结构，
// 也不会因为少拷一个 .js 而打开一个白屏。
const PAGE: &str = include_str!("web/index.html");
const STYLE: &str = include_str!("web/app.css");
const SCRIPT: &str = include_str!("web/app.js");

/// 请求头总长上限。挡住「一直发头不发空行」的连接。
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// 请求体上限（JSON 而已，16 MB 已经非常宽裕）。
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// 单个任务的日志行数上限。下载几千首时行数是按曲目涨的，不能让它无限涨。
const MAX_JOB_LINES: usize = 2000;
/// 保留多少个任务。够回溯最近几次操作，又不至于一直攒着。
const MAX_JOBS: usize = 20;
/// 同时处理的连接数上限。这是个内部界面，超了直接拒绝比排队更诚实。
const MAX_CONNS: usize = 32;

const TOKEN_HEADER: &str = "x-musicm-token";

/// 启动参数。
pub struct ServeOptions {
    /// `--data-dir` 指定的数据目录，决定去哪里找 `config.json` / `index.json`
    pub data_dir: Option<PathBuf>,
    /// 监听地址，形如 `127.0.0.1:8765`
    pub listen: String,
    pub token: Option<String>,
    /// 明确要求「连令牌都别要」。只在完全可信的网络里用。
    pub no_auth: bool,
}

/// 启动服务器。这个函数不返回（除非出错）。
pub fn serve(opts: ServeOptions) -> Result<()> {
    let listener = TcpListener::bind(&opts.listen)
        .with_context(|| format!("监听 {} 失败（地址写错了、端口被占用、或者端口 < 1024 需要 root）", opts.listen))?;
    let bound = listener.local_addr()?;

    // 先加载一次配置：一方面把它打印出来给用户看，另一方面顺手把
    // 「config.json 不存在 → 建一份默认」这在启动前做掉，免得第一个请求撞上它。
    let cfg = Config::load(opts.data_dir.clone())?;

    let token = decide_token(&opts, bound.ip());

    let shared = Arc::new(Shared {
        data_dir: opts.data_dir.clone(),
        token: token.clone(),
        jobs: Mutex::new(Jobs::default()),
        mount: Arc::new(Mutex::new(MountSlot::default())),
        live: AtomicUsize::new(0),
    });

    print_banner(&cfg, &bound, token.as_deref());

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("接受连接失败: {e}");
                continue;
            }
        };

        // 超过上限就直接回绝。不排队，是因为界面上同时点几下不该让后面的请求
        // 悄悄等上一分钟——用户更愿意看到「忙」而不是转圈。
        if shared.live.load(Ordering::SeqCst) >= MAX_CONNS {
            let mut s = stream;
            let _ = write_reply(&mut s, &Reply::json(503, &json!({"error": "同时连接数已达上限，请稍后再试"})), false);
            continue;
        }
        shared.live.fetch_add(1, Ordering::SeqCst);
        let cloned = Arc::clone(&shared);
        thread::spawn(move || {
            handle_conn(stream, &cloned);
            cloned.live.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

fn print_banner(cfg: &Config, bound: &std::net::SocketAddr, token: Option<&str>) {
    let shown_host = if bound.ip().is_loopback() || bound.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bound.ip().to_string()
    };
    let url = format!("http://{shown_host}:{}", bound.port());

    println!("musicm 管理界面已启动");
    println!("  数据目录  {}", cfg.data_dir.display());
    println!("  音乐目录  {}", cfg.out_root.display());
    println!("  音源      {}", cfg.api.describe());
    println!("  监听      {bound}");

    match token {
        Some(t) => {
            println!("  打开      {url}/?token={t}");
            println!();
            println!("  这个链接自带访问令牌；打开后页面会把它存起来，并把地址栏里的令牌去掉。");
            println!("  注意：这个令牌是进程启动时生成的，重启 musicm 会换一个新的。");
        }
        None => {
            println!("  打开      {url}/");
            println!();
            println!("  当前只监听本机地址，且没有启用访问令牌。");
        }
    }

    if let Some(ip) = lan_ip() {
        println!("  局域网    也可以用 http://{ip}:{}/ 访问（按 Ctrl-C 停止服务）", bound.port());
    } else {
        println!("  按 Ctrl-C 停止服务");
    }
    println!();
}

/// 决定要不要启用访问令牌。
///
/// 规则很简单：**监听地址不是回环，就必须有令牌**。
/// 界面能改登录凭据、能往音乐库里写文件，暴露在局域网上还不设防是不负责任的。
/// 用户没给令牌就自动生成一个并打印，而不是干脆不启动——
/// 「起不来」只会把人逼去加 `--no-auth`，那才是真的把门敞开。
fn decide_token(opts: &ServeOptions, ip: IpAddr) -> Option<String> {
    if opts.no_auth {
        return None;
    }
    if let Some(t) = opts.token.clone() {
        return Some(t);
    }
    if ip.is_loopback() {
        return None;
    }
    Some(random_token())
}

/// 生成一个够用的随机令牌。
///
/// 刻意不引 `rand`：这个令牌的用途是「挡住扫到 IP 就点开的脚本」，不是对抗
/// 有针对的攻击者。真要放到公网上，应该自己用 `--token` 传一个足够长的，
/// 或者直接只在内网用。
fn random_token() -> String {
    fn splitmix64(mut x: u64) -> u64 {
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }

    // 光靠「时间 + pid + 栈地址」在同一个进程里是**完全确定的**——
    // 而这个函数本来就可能在同一次运行里被调多次（启动时生成、测试里连着调两次），
    // 那样就会拿到两个一模一样的令牌。所以再掺一个全局递增计数。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);

    let mut x = now_millis();
    x ^= (std::process::id() as u64).wrapping_shl(32);
    // 栈地址带一点 ASLR 的熵
    x ^= (&x as *const u64 as u64).rotate_left(17);
    x ^= seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);

    let mut out = String::with_capacity(32);
    for _ in 0..4 {
        x = splitmix64(x.wrapping_add(0x9E37_79B9_7F4A_7C15));
        out.push_str(&format!("{:08x}", (x >> 32) as u32));
    }
    out
}

/// 本机在局域网里的地址，用来在启动时打印一个能直接点的链接。
///
/// UDP 的 `connect` 不发任何包，只是让内核填好出口地址，所以不会真的连外网，
/// 也没有超时风险。失败（没网、多网卡、奇怪的路由）就返回 `None`，
/// 反正这个信息只是锦上添花。
fn lan_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("223.5.5.5:80").ok()?;
    let addr = sock.local_addr().ok()?;
    (!addr.ip().is_loopback()).then_some(addr.ip())
}

// ---------- 共享状态 ----------

struct Shared {
    /// `--data-dir` 的覆盖值。每次请求都重新 `Config::load`，
    /// 这样用户手改 `config.json` 之后刷新页面就能看到变化。
    data_dir: Option<PathBuf>,
    token: Option<String>,
    jobs: Mutex<Jobs>,
    /// 挂载状态。放在 `Arc` 里是因为挂载线程要在它结束/失败时改它。
    mount: Arc<Mutex<MountSlot>>,
    live: AtomicUsize,
}

impl Shared {
    fn cfg(&self) -> Result<Config> {
        Config::load(self.data_dir.clone())
    }

    fn index(&self) -> Result<Index> {
        let cfg = self.cfg()?;
        Index::load(&cfg.index_path())
    }
}

/// 锁被毒化时（某个线程在持锁期间 panic）不跟着 panic，
/// 而是把状态抢回来继续服务——一个请求写坏了不该让整个界面挂掉。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- 后台任务 ----------

#[derive(Default)]
struct Jobs {
    next_id: u64,
    items: Vec<Arc<Mutex<Job>>>,
}

struct Job {
    id: u64,
    kind: String,
    title: String,
    running: bool,
    started_at: u64,
    finished_at: Option<u64>,
    error: Option<String>,
    result: Option<Value>,
    lines: Vec<String>,
}

impl Job {
    /// 列表里用的摘要（不含日志，日志可能很长）。
    fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "running": self.running,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "error": self.error,
            "lines": self.lines.len(),
        })
    }

    fn detail(&self) -> Value {
        let mut v = self.summary();
        v["log"] = json!(self.lines);
        v["result"] = self.result.clone().unwrap_or(Value::Null);
        v
    }
}

/// 任务线程往界面上说话的通道。
#[derive(Clone)]
struct Progress(Arc<Mutex<Job>>);

impl Progress {
    fn say(&self, line: impl Into<String>) {
        let mut job = lock(&self.0);
        if job.lines.len() >= MAX_JOB_LINES {
            job.lines.remove(0);
        }
        job.lines.push(line.into());
    }
}

/// 起一个后台任务，返回它的 id。
fn spawn_job<F>(shared: &Arc<Shared>, kind: &str, title: String, body: F) -> u64
where
    F: FnOnce(&Progress) -> Result<Value> + Send + 'static,
{
    let (id, handle) = {
        let mut jobs = lock(&shared.jobs);
        jobs.next_id += 1;
        let id = jobs.next_id;
        let handle = Arc::new(Mutex::new(Job {
            id,
            kind: kind.to_string(),
            title,
            running: true,
            started_at: now_secs(),
            finished_at: None,
            error: None,
            result: None,
            lines: Vec::new(),
        }));
        jobs.items.push(Arc::clone(&handle));
        // 只留最近若干个。界面上没人会翻到更早，攒着只是占内存。
        while jobs.items.len() > MAX_JOBS {
            jobs.items.remove(0);
        }
        (id, handle)
    };

    let progress = Progress(Arc::clone(&handle));
    thread::spawn(move || {
        match body(&progress) {
            Ok(v) => {
                let mut job = lock(&handle);
                job.running = false;
                job.finished_at = Some(now_secs());
                job.result = Some(v);
            }
            Err(e) => {
                let mut job = lock(&handle);
                job.running = false;
                job.finished_at = Some(now_secs());
                job.error = Some(format!("{e:#}"));
                job.lines.push(format!("失败：{e:#}"));
            }
        }
    });
    id
}

// ---------- 挂载状态 ----------

#[derive(Default)]
struct MountSlot {
    mountpoint: Option<PathBuf>,
    mode: String,
    running: bool,
    started_at: u64,
    message: Option<String>,
}

impl MountSlot {
    /// 界面关心的挂载状态。
    ///
    /// **内核里的真实情况优先于内存记录**：进程重启之后内存记录是空的，
    /// 但挂载可能还挂着。所以每次都问一次 `/proc/mounts`，
    /// 不能只报「我们这次有没有挂」。
    fn view(&self) -> Value {
        let detected = self
            .mountpoint
            .as_ref()
            .and_then(|p| mount::mounted_fstype(p));
        json!({
            "supported": mount::fuse_supported(),
            "running": self.running,
            "mounted": detected.is_some(),
            "mountpoint": self.mountpoint.as_ref().map(|p| p.display().to_string()),
            "mode": self.mode,
            "started_at": self.started_at,
            "message": self.message,
            "fstype": detected,
        })
    }
}

// ---------- HTTP ----------

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    /// 头名一律小写
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .map(str::trim)
    }
}

struct Reply {
    status: u16,
    ctype: &'static str,
    body: Vec<u8>,
}

impl Reply {
    fn json(status: u16, v: &Value) -> Reply {
        // 兜底响应必须是纯 ASCII：字节串字面量 `b"..."` 里不能放中文
        let body = serde_json::to_vec(v)
            .unwrap_or_else(|_| br#"{"error":"serialization failed"}"#.to_vec());
        Reply { status, ctype: "application/json; charset=utf-8", body }
    }

    fn error(status: u16, msg: impl Into<String>) -> Reply {
        Reply::json(status, &json!({ "error": msg.into() }))
    }

    fn static_asset(ctype: &'static str, body: &'static str) -> Reply {
        Reply { status: 200, ctype, body: body.as_bytes().to_vec() }
    }
}

fn handle_conn(stream: std::net::TcpStream, shared: &Arc<Shared>) {
    let mut stream = stream;
    // 读请求给个超时：半开连接（连上不发东西）会把一个线程占死。
    // Rust 标准库没有 per-op 超时，只能在 socket 上设一次。
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));

    let result = (|| -> Result<Option<(HttpRequest, bool)>> {
        let mut reader = BufReader::new(&stream);
        let Some(req) = read_request(&mut reader)? else {
            return Ok(None);
        };
        let head_only = req.method == "HEAD";
        Ok(Some((req, head_only)))
    })();

    let reply = match result {
        Ok(None) => return,
        Ok(Some((req, head_only))) => {
            let r = route(&req, shared);
            let _ = write_reply(&mut stream, &r, head_only);
            return;
        }
        Err(_) => Reply::error(400, "请求解析失败：只支持 HTTP/1.1，且头必须在 64 KB 以内"),
    };
    let _ = write_reply(&mut stream, &reply, false);
}

/// 读出一行（不含换行符）。读不到完整一行就当错误——HTTP 的行不该长到 64 KB。
fn read_line<R: BufRead>(reader: &mut R) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    // 用 `take` 卡住上限：不设限的话，一个只发头不发空行的连接能把内存吃干
    let mut limited = Read::take(reader, MAX_HEADER_BYTES as u64);
    let n = limited
        .read_until(b'\n', &mut buf)
        .context("读取请求行失败")?;
    if n == 0 {
        bail!("连接被关闭");
    }
    while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        buf.pop();
    }
    if n as usize >= MAX_HEADER_BYTES {
        bail!("请求行过长");
    }
    Ok(buf)
}

fn read_request(reader: &mut impl BufRead) -> Result<Option<HttpRequest>> {
    // 浏览器有时会先发一个空连接（探测、preconnect），读到 EOF 就安静地关掉。
    let first = match read_line(reader) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if first.is_empty() {
        return Ok(None);
    }

    let line = String::from_utf8_lossy(&first);
    let mut parts = line.split_whitespace();
    let method = parts.next().context("请求行缺方法")?.to_uppercase();
    let target = parts.next().context("请求行缺目标")?.to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (percent_decode(p), parse_query(q)),
        None => (percent_decode(&target), HashMap::new()),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let raw = read_line(reader)?;
        if raw.is_empty() {
            break;
        }
        let text = String::from_utf8_lossy(&raw);
        if let Some((k, v)) = text.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let mut body = Vec::new();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > 0 {
        if len > MAX_BODY_BYTES {
            bail!("请求体过大（{len} 字节，上限 {MAX_BODY_BYTES}）");
        }
        body.resize(len, 0);
        reader.read_exact(&mut body).context("读取请求体失败")?;
    }

    Ok(Some(HttpRequest { method, path, query, headers, body }))
}

/// 只解 `%XX`。不把 `+` 当空格——那是表单编码的规矩，
/// 我们的查询串一律由前端用 `encodeURIComponent` 生成，空格是 `%20`。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn write_reply(w: &mut impl Write, reply: &Reply, head_only: bool) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.ctype,
        reply.body.len()
    );
    w.write_all(head.as_bytes())?;
    if !head_only {
        w.write_all(&reply.body)?;
    }
    w.flush()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

// ---------- 路由 ----------

fn route(req: &HttpRequest, shared: &Arc<Shared>) -> Reply {
    // 静态资源不校验令牌：页面本身要先能打开，才有机会让用户填令牌。
    // 它们也不含任何敏感信息（代码是公开的、数据都在 /api 里）。
    match (req.method.as_str(), req.path.as_str()) {
        ("GET" | "HEAD", "/" | "/index.html") => return Reply::static_asset("text/html; charset=utf-8", PAGE),
        ("GET" | "HEAD", "/app.css") => return Reply::static_asset("text/css; charset=utf-8", STYLE),
        ("GET" | "HEAD", "/app.js") => {
            return Reply::static_asset("application/javascript; charset=utf-8", SCRIPT);
        }
        _ => {}
    }

    let path = req.path.as_str();
    if !path.starts_with("/api/") {
        return Reply::error(404, format!("没有这个地址: {path}"));
    }

    if let Some(expected) = shared.token.as_deref() {
        match req.header(TOKEN_HEADER) {
            Some(got) if got == expected => {}
            _ => {
                return Reply::error(
                    401,
                    "访问令牌不对。启动 musicm 时终端里打印过带令牌的链接，用它打开就行。",
                );
            }
        }
    }

    let body = if req.body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&req.body) {
            Ok(v) => v,
            Err(e) => return Reply::error(400, format!("请求体不是合法 JSON: {e}")),
        }
    };

    let out = match (req.method.as_str(), path) {
        ("GET", "/api/status") => api(|| api_status(shared)),
        ("GET", "/api/playlists") => api(|| api_playlists(shared)),
        ("GET", "/api/my-playlists") => api(|| api_my_playlists(shared, &req.query)),
        ("GET", "/api/search") => api(|| api_search(shared, &req.query)),
        ("GET", "/api/daily") => api(|| api_daily(shared, &req.query)),
        ("GET", "/api/jobs") => api(|| api_jobs(shared)),
        ("POST", "/api/login") => api(|| api_login(shared, &body)),
        ("POST", "/api/logout") => api(|| api_logout(shared)),
        ("POST", "/api/whoami") => api(|| api_whoami(shared)),
        ("POST", "/api/scan") => api(|| api_scan(shared, &body)),
        ("POST", "/api/play") => api(|| api_play(shared, &body)),
        ("POST", "/api/fetch") => api(|| api_fetch(shared, &body)),
        ("POST", "/api/mount") => api(|| api_mount(shared, &body)),
        ("POST", "/api/unmount") => api(|| api_unmount(shared, &body)),

        _ => {
            if let Some(rest) = path.strip_prefix("/api/playlists/") {
                let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
                match sub {
                    "tracks" => api(|| api_tracks(shared, id, &req.query)),
                    "" => api(|| api_playlists(shared)),
                    _ => Reply::error(404, format!("没有这个接口: {path}")),
                }
            } else if let Some(rest) = path.strip_prefix("/api/artist/") {
                let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
                match sub {
                    "songs" => api(|| api_artist_songs(shared, id, &req.query)),
                    _ => Reply::error(404, format!("没有这个接口: {path}")),
                }
            } else if let Some(id) = path.strip_prefix("/api/jobs/") {
                api(|| api_job(shared, id))
            } else {
                Reply::error(404, format!("没有这个接口: {path}"))
            }
        }
    };
    out
}

/// 把 `Result<Value>` 变成响应。
///
/// 业务错误一律 400 而不是 500：这些错误几乎都是「输入不对 / 源站没给东西 /
/// 接口改版」这类用户能处理的情况，报成 500 会让人以为是程序崩了。
fn api<F: FnOnce() -> Result<Value>>(f: F) -> Reply {
    match f() {
        Ok(v) => Reply::json(200, &v),
        Err(e) => Reply::error(400, format!("{e:#}")),
    }
}

// ---------- 参数读取 ----------

fn q_str<'a>(q: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    q.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

fn q_usize(q: &HashMap<String, String>, key: &str, default: usize) -> usize {
    q.get(key)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn b_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str()).map(str::trim)
}

fn b_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}

fn b_bool(v: &Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

/// id 列表。前端传的是字符串数组，里面既可以是纯数字也可以是 `netease:123`。
fn b_ids(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    x.as_u64()
                        .map(|n| n.to_string())
                        .or_else(|| x.as_str().map(|s| s.trim().to_string()))
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ---------- 各接口 ----------

fn api_status(shared: &Shared) -> Result<Value> {
    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let stats = index.stats();
    let logged_in = cfg.has_login();

    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "fuse_supported": mount::fuse_supported(),
        "data_dir": cfg.data_dir.display().to_string(),
        "out_root": cfg.out_root.display().to_string(),
        "out_root_exists": cfg.out_root.is_dir(),
        "config_path": cfg.config_path().display().to_string(),
        "index_path": cfg.index_path().display().to_string(),
        "api": cfg.api.describe(),
        "quality": cfg.quality.as_str(),
        "quality_options": crate::config::Quality::ALL.iter().map(|q| q.as_str()).collect::<Vec<_>>(),
        "login": {
            "has_cookie": cfg.cookie.is_some(),
            "has_login": logged_in,
            "origin": cfg.cookie_origin.label(),
            // 脱敏后的形态，不含 MUSIC_U 明文（见 auth::Credentials::masked）
            "masked": cfg.describe_login(),
        },
        "index": {
            "playlists": stats.playlists,
            "tracks": stats.tracks,
            "files": stats.files,
            "cached_bytes": stats.cached_bytes,
            "vip_only": stats.vip_only,
        },
        "mount": lock(&shared.mount).view(),
    }))
}

fn api_playlists(shared: &Shared) -> Result<Value> {
    let index = shared.index()?;
    let items: Vec<Value> = index
        .playlists()
        .map(|pl| {
            let landed = pl
                .track_ids
                .iter()
                .filter(|id| index.file_of(&format!("{}:{}", pl.source, id)).is_some())
                .count();
            json!({
                "key": pl.key(),
                "id": pl.id,
                "name": pl.name,
                "creator": pl.creator,
                "tracks": pl.track_ids.len(),
                "landed": landed,
                "declared": pl.declared_count,
                "scanned_at": pl.scanned_at,
            })
        })
        .collect();
    Ok(json!({ "playlists": items }))
}

fn api_tracks(shared: &Shared, id: &str, q: &HashMap<String, String>) -> Result<Value> {
    let index = shared.index()?;
    let pid: u64 = id
        .parse()
        .map_err(|_| anyhow!("歌单 id 不是数字: {id}"))?;
    let key = crate::store::playlist_key_for("netease", pid);
    let pl = index
        .playlist(&key)
        .ok_or_else(|| anyhow!("索引里没有歌单 {pid}，先用「扫描歌单」把它加进来"))?;

    let limit = q_usize(q, "limit", 2000);
    let all = index.playlist_tracks(&key);
    let shown: Vec<&Track> = all.iter().take(limit).collect();

    let tracks: Vec<Value> = shown
        .iter()
        .map(|t| track_view(t, index.file_of(&t.key())))
        .collect();

    Ok(json!({
        "playlist": { "id": pl.id, "name": pl.name, "creator": pl.creator, "scanned_at": pl.scanned_at },
        "total": all.len(),
        "shown": tracks.len(),
        "tracks": tracks,
    }))
}

fn track_view(t: &Track, file: Option<&crate::model::CachedFile>) -> Value {
    json!({
        "id": t.id,
        "key": t.key(),
        "name": t.name,
        "artist": t.artist_line(),
        "album": t.album,
        "duration": t.duration_label(),
        "vip": t.vip_only(),
        "cached": file.is_some(),
        "size": file.map(|f| f.size_label()),
        "quality": file.map(|f| f.quality.clone()),
        "path": file.map(|f| f.path.clone()),
    })
}

fn api_search(shared: &Shared, q: &HashMap<String, String>) -> Result<Value> {
    let keyword = q_str(q, "q").unwrap_or("").trim();
    if keyword.is_empty() {
        bail!("请输入关键词");
    }
    let limit = q_usize(q, "limit", 20).clamp(1, 100);
    let offset = q_usize(q, "offset", 0);
    let kinds: Vec<SearchKind> = match q_str(q, "type").unwrap_or("all") {
        "song" => vec![SearchKind::Song],
        "playlist" => vec![SearchKind::Playlist],
        "artist" => vec![SearchKind::Artist],
        _ => SearchKind::ALL.to_vec(),
    };

    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let client = NeteaseClient::new(&cfg)?;

    let mut groups = Vec::new();
    for kind in kinds {
        let group = match kind {
            SearchKind::Song => {
                let out = client.search_songs(keyword, offset, limit)?;
                search_group(
                    kind,
                    &out.shape,
                    out.code,
                    out.total,
                    out.items
                        .iter()
                        .map(|t| {
                            json!({
                                "kind": "song",
                                "id": t.id,
                                "name": t.name,
                                "artist": t.artist_line(),
                                "album": t.album,
                                "duration": t.duration_label(),
                                "vip": t.vip_only(),
                                "cached": index.file_of(&t.key()).is_some(),
                                "indexed": index.tracks.contains_key(&t.key()),
                            })
                        })
                        .collect(),
                )
            }
            SearchKind::Playlist => {
                let out = client.search_playlists(keyword, offset, limit)?;
                search_group(
                    kind,
                    &out.shape,
                    out.code,
                    out.total,
                    out.items
                        .iter()
                        .map(|p| {
                            json!({
                                "kind": "playlist",
                                "id": p.id,
                                "name": p.name,
                                "creator": p.creator,
                                "count": p.track_count,
                                "indexed": index.playlists.contains_key(&p.key()),
                            })
                        })
                        .collect(),
                )
            }
            SearchKind::Artist => {
                let out = client.search_artists(keyword, offset, limit)?;
                search_group(
                    kind,
                    &out.shape,
                    out.code,
                    out.total,
                    out.items
                        .iter()
                        .map(|a| {
                            json!({
                                "kind": "artist",
                                "id": a.id,
                                "name": a.name,
                                "alias": a.alias.join(" / "),
                                "count": a.song_count,
                            })
                        })
                        .collect(),
                )
            }
        };
        groups.push(group);
    }

    Ok(json!({ "query": keyword, "offset": offset, "limit": limit, "groups": groups }))
}

/// 把一次搜索的结果整理成一个分组，空结果要带上理由。
///
/// 「空」必须能解释：实测搜索无命中时数组字段整个消失（`code` 仍然是 200），
/// 而不带理由的空列表会让人以为程序坏了，跑去改代码而不是换个关键词。
fn search_group(
    kind: SearchKind,
    shape: &ListShape,
    code: i32,
    total: usize,
    items: Vec<Value>,
) -> Value {
    let empty = if items.is_empty() {
        Some(match shape {
            ListShape::Items => "接口有响应，但本次没有取到条目".to_string(),
            ListShape::Empty => "没有匹配的结果".to_string(),
            ListShape::Missing => format!(
                "响应里没有可识别的结果字段（code={code}）。接口可能改版了，\
                 或者当前是匿名状态而 {} 需要登录才能搜。",
                kind.label()
            ),
        })
    } else {
        None
    };
    json!({
        "kind": match kind { SearchKind::Song => "song", SearchKind::Playlist => "playlist", SearchKind::Artist => "artist" },
        "label": kind.label(),
        "total": total,
        "code": code,
        "items": items,
        "empty": empty,
    })
}

fn api_my_playlists(shared: &Shared, q: &HashMap<String, String>) -> Result<Value> {
    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let client = NeteaseClient::new(&cfg)?;
    let limit = q_usize(q, "limit", 50).clamp(1, 100);
    let offset = q_usize(q, "offset", 0);

    // 匿名时没有 uid 可用：先问一次账号接口，拿不到就说明没登录
    let uid = match client.account()? {
        Some(acc) => acc.uid,
        None => bail!("还没有登录，或者凭据已经失效。先到「账号」里登录，才能列出自己的歌单。"),
    };
    let got = client.user_playlists(uid, limit, offset)?;

    let items: Vec<Value> = got
        .playlists
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "creator": p.creator,
                "count": p.track_count,
                "indexed": index.playlists.contains_key(&p.key()),
                "favorite": p.is_favorite(),
                "private": p.is_private(),
            })
        })
        .collect();

    let empty = if items.is_empty() {
        Some(match got.shape {
            ListShape::Items => "这个账号下没有歌单".to_string(),
            ListShape::Empty => "这个账号下还没有歌单".to_string(),
            ListShape::Missing => format!(
                "响应里没有可识别的歌单字段（code={}）。匿名只能看别人的公开歌单；\
                 要列自己的，需要先登录。",
                got.code
            ),
        })
    } else {
        None
    };

    Ok(json!({ "uid": uid, "more": got.more, "playlists": items, "empty": empty }))
}

fn api_artist_songs(shared: &Shared, id: &str, q: &HashMap<String, String>) -> Result<Value> {
    let aid: u64 = id.parse().map_err(|_| anyhow!("歌手 id 不是数字: {id}"))?;
    let limit = q_usize(q, "limit", 50);

    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let client = NeteaseClient::new(&cfg)?;
    let (code, tracks) = client.artist_top_songs(aid)?;
    // 这个接口的 `limit` 参数实测不生效（固定 50 首），截断只能自己做
    let shown: Vec<&Track> = tracks.iter().take(limit).collect();

    Ok(json!({
        "artist_id": aid,
        "code": code,
        "total": tracks.len(),
        "tracks": shown.iter().map(|t| json!({
            "id": t.id,
            "name": t.name,
            "artist": t.artist_line(),
            "album": t.album,
            "duration": t.duration_label(),
            "vip": t.vip_only(),
            "cached": index.file_of(&t.key()).is_some(),
        })).collect::<Vec<_>>(),
    }))
}

fn api_daily(shared: &Shared, q: &HashMap<String, String>) -> Result<Value> {
    let limit = q_usize(q, "limit", 30);
    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let client = NeteaseClient::new(&cfg)?;
    let daily = client.daily_songs()?;

    if daily.tracks.is_empty() {
        // 这个接口的失败方式是「静默空数组」：未登录和凭据失效都返回 code=200 + 空，
        // 不给任何错误码。所以只能靠「本地有没有存过 cookie」来猜是哪一种。
        let hint = if cfg.cookie.is_some() {
            "本地有凭据，但接口仍然返回空——多半是它失效了，去「账号」重新登录一次。"
        } else {
            "还没有登录，每日推荐要先登录才能拿到。"
        };
        return Ok(json!({
            "code": daily.code,
            "shape": daily.shape.label(),
            "tracks": [],
            "empty": format!("接口返回空（code={}）。{hint}", daily.code),
        }));
    }

    let dir = cfg
        .out_root
        .join(naming::group_rel("netease", Some(naming::DAILY_GROUP)));
    let stems = naming::unique_stems(&daily.tracks);
    let on_disk = naming::landed_stems(&dir);

    let tracks: Vec<Value> = daily
        .tracks
        .iter()
        .zip(stems.iter())
        .map(|(t, stem)| {
            let landed = index.file_of(&t.key()).is_some() || on_disk.contains(stem);
            json!({
                "id": t.id,
                "name": t.name,
                "artist": t.artist_line(),
                "album": t.album,
                "duration": t.duration_label(),
                "vip": t.vip_only(),
                "cached": landed,
            })
        })
        .collect();

    Ok(json!({
        "code": daily.code,
        "shape": daily.shape.label(),
        "total": daily.tracks.len(),
        "shown": limit.min(tracks.len()),
        "dir": dir.display().to_string(),
        "tracks": tracks,
    }))
}

fn api_login(shared: &Shared, body: &Value) -> Result<Value> {
    let raw = b_str(body, "cookie").unwrap_or("").trim();
    if raw.is_empty() {
        bail!("请粘贴 cookie。需要的是登录后浏览器里那条 `MUSIC_U=...`，\
               匿名 cookie（只有 MUSIC_A）拿不到会员曲目。");
    }

    // 先解析再落盘：格式不对（比如缺 MUSIC_U）时什么都别写，
    // 免得把一份坏凭据存进 cookie.txt 覆盖掉原来那份好的。
    let cred = Credentials::parse(raw)?;
    let mut cfg = shared.cfg()?;
    let saved_at = auth::save(&cfg.data_dir, cred.as_str())?;

    // 让这一次请求就用上新凭据——否则下面的校验还在用旧 cookie，
    // 明明登录成功了却报「未登录」。
    cfg.cookie = Some(cred.as_str().to_string());
    cfg.cookie_origin = auth::Origin::File;

    let client = NeteaseClient::new(&cfg)?;
    let account = client.account()?;

    Ok(json!({
        "saved": saved_at.display().to_string(),
        "masked": cred.masked(),
        // 同名 cookie 只留了第一条，报出来免得用户以为抄漏了
        "dropped": cred.dropped(),
        "account": account.as_ref().map(|a| json!({"uid": a.uid, "nickname": a.nickname, "vip": a.vip_type})),
        "note": match &account {
            Some(a) => format!("登录成功：{}（uid {}）", a.nickname, a.uid),
            None => "凭据已保存，但账号接口没有返回登录信息——它多半已经失效了，\
                     请重新从浏览器复制一次 cookie。".to_string(),
        },
    }))
}

fn api_logout(shared: &Shared) -> Result<Value> {
    let cfg = shared.cfg()?;
    let removed = auth::clear(&cfg.data_dir)?;
    Ok(json!({
        "removed": removed,
        "note": if removed { "已清除本地凭据" } else { "本地本来就没有存凭据" },
    }))
}

fn api_whoami(shared: &Shared) -> Result<Value> {
    let cfg = shared.cfg()?;
    let client = NeteaseClient::new(&cfg)?;
    let account = client.account()?;
    Ok(json!({
        "has_cookie": cfg.cookie.is_some(),
        "origin": cfg.cookie_origin.label(),
        "logged_in": account.is_some(),
        "account": account.as_ref().map(|a| json!({"uid": a.uid, "nickname": a.nickname, "vip": a.vip_type})),
        "note": match &account {
            Some(a) => format!("已登录：{}（uid {}）", a.nickname, a.uid),
            None if cfg.cookie.is_some() => "本地有凭据，但账号接口不认——它已经失效了，重新登录一次吧。".to_string(),
            None => "当前是匿名状态，只能拿到免费档曲目。".to_string(),
        },
    }))
}

fn api_scan(shared: &Arc<Shared>, body: &Value) -> Result<Value> {
    let pid = b_u64(body, "playlist_id")
        .or_else(|| b_u64(body, "id"))
        .ok_or_else(|| anyhow!("请给出歌单 id"))?;
    let cloned = Arc::clone(shared);
    let id = spawn_job(
        shared,
        "scan",
        format!("扫描歌单 {pid}"),
        move |p| job_scan(&cloned, p, pid),
    );
    Ok(json!({ "job": id }))
}

fn job_scan(shared: &Shared, p: &Progress, pid: u64) -> Result<Value> {
    let cfg = shared.cfg()?;
    p.say(format!("向音源请求歌单 {pid} …"));
    let client = NeteaseClient::new(&cfg)?;
    let (playlist, tracks) = client.playlist_detail(pid)?;

    p.say(format!(
        "拿到《{}》：接口给了 {} 首（它自己声明有 {} 首）",
        playlist.name,
        tracks.len(),
        playlist.declared_count
    ));
    if tracks.is_empty() {
        bail!("歌单《{}》一首都没返回。它可能设置了权限、需要登录，或者 id 不存在。", playlist.name);
    }

    let mut index = shared.index()?;
    let outcome = index.upsert_playlist(playlist.clone(), tracks.clone());
    index.save(&cfg.index_path())?;
    p.say(format!(
        "索引已更新：新增 {} 首、更新 {} 首，共 {} 首",
        outcome.added, outcome.updated, outcome.total
    ));

    Ok(json!({
        "playlist": { "id": playlist.id, "name": playlist.name, "creator": playlist.creator },
        "added": outcome.added,
        "updated": outcome.updated,
        "total": outcome.total,
    }))
}

/// 落地一批曲目（走的是和 `musicm play` 同一条路：按索引决定目录、写回索引）。
fn api_play(shared: &Arc<Shared>, body: &Value) -> Result<Value> {
    let ids = b_ids(body, "ids");
    if ids.is_empty() {
        if let Some(one) = b_u64(body, "song_id").or_else(|| b_u64(body, "id")) {
            return start_play_job(shared, vec![one.to_string()], b_bool(body, "force", false));
        }
        bail!("请给出要下载的曲目 id");
    }
    let force = b_bool(body, "force", false);
    start_play_job(shared, ids, force)
}

fn start_play_job(shared: &Arc<Shared>, ids: Vec<String>, force: bool) -> Result<Value> {
    let title = if ids.len() == 1 {
        format!("下载 {}", ids[0])
    } else {
        format!("下载 {} 首", ids.len())
    };
    let cloned = Arc::clone(shared);
    let id = spawn_job(shared, "play", title, move |p| {
        job_play(&cloned, p, ids, force)
    });
    Ok(json!({ "job": id }))
}

fn job_play(shared: &Shared, p: &Progress, ids: Vec<String>, force: bool) -> Result<Value> {
    let cfg = shared.cfg()?;
    let client = NeteaseClient::new(&cfg)?;
    let fetcher = Fetcher::new(&client, &cfg);

    let mut done = 0usize;
    let mut cached = 0usize;
    let mut failed = 0usize;
    let mut files: Vec<Value> = Vec::new();

    for (i, raw) in ids.iter().enumerate() {
        p.say(format!("[{}/{}] 处理 {raw}", i + 1, ids.len()));
        let mut index = shared.index()?;

        // 索引里有的直接用；没有就现场取一次详情——搜索结果、歌手热门
        // 都是这么进来的。顺带补上封面：搜索接口不给 picUrl，
        // 缺了封面飞牛的刮削会匹配不上。
        let (track, from_index) = match index.resolve_track(raw) {
            Some(t) => (t.clone(), true),
            None => {
                let id: u64 = raw.parse().map_err(|_| anyhow!("曲目 id 不是数字: {raw}"))?;
                let got = client.songs_detail(&[id])?;
                let t = got
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("接口没有返回曲目 {id} 的详情，id 可能不存在或已下架"))?;
                (t, false)
            }
        };
        p.say(format!("   {} - {}", track.name, track.artist_line()));

        let playlist: Option<Playlist> = index.owning_playlist(track.id).cloned();
        let siblings: Option<Vec<Track>> = playlist
            .as_ref()
            .map(|pl| index.playlist_tracks(&pl.key()));

        match fetcher.fetch(&track, playlist.as_ref(), siblings.as_deref(), force) {
            Ok(outcome) => {
                // 现取的曲目先补一条元数据记录，否则落地之后统计和 FUSE 树里都看不到
                if !from_index {
                    index.remember_track(track.clone());
                }
                index.record_file(outcome.to_cached_file(&track));
                index.save(&cfg.index_path())?;

                let file = outcome.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                for n in &outcome.notes {
                    p.say(format!("   备注：{n}"));
                }
                for w in &outcome.warnings {
                    p.say(format!("   警告：{w}"));
                }
                if outcome.from_cache {
                    cached += 1;
                    p.say(format!("   已在库里，跳过：{file}"));
                } else {
                    done += 1;
                    p.say(format!(
                        "   完成：{file}（{}，{}）",
                        outcome.quality,
                        human_bytes(outcome.bytes)
                    ));
                }
                files.push(json!({
                    "id": track.id,
                    "name": track.name,
                    "path": outcome.path.to_string_lossy(),
                    "from_cache": outcome.from_cache,
                }));
            }
            Err(e) => {
                failed += 1;
                p.say(format!("   失败：{e:#}"));
            }
        }
    }

    p.say(format!("结束：下载 {done} 首、已存在 {cached} 首、失败 {failed} 首"));
    if done == 0 && cached == 0 && failed > 0 {
        bail!("全部失败。常见原因是没登录（会员曲目）或凭据失效。");
    }
    Ok(json!({ "done": done, "cached": cached, "failed": failed, "files": files }))
}

/// 落地一批曲目，但**不写索引**（与 `musicm daily --fetch` 一致）。
///
/// 每日推荐每天都变，写进索引会让索引堆着一堆一次性曲目；
/// 所以这些文件只落到磁盘，状态靠反查目录来认。
fn api_fetch(shared: &Arc<Shared>, body: &Value) -> Result<Value> {
    let ids: Vec<u64> = b_ids(body, "ids")
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    if ids.is_empty() {
        bail!("请给出要下载的曲目 id");
    }
    let group = b_str(body, "group").map(|s| s.to_string());

    let cloned = Arc::clone(shared);
    let title = match &group {
        Some(g) => format!("下载 {} 首到 {g}", ids.len()),
        None => format!("下载 {} 首", ids.len()),
    };
    let id = spawn_job(shared, "fetch", title, move |p| {
        job_fetch(&cloned, p, ids, group)
    });
    Ok(json!({ "job": id }))
}

fn job_fetch(
    shared: &Shared,
    p: &Progress,
    ids: Vec<u64>,
    group: Option<String>,
) -> Result<Value> {
    let cfg = shared.cfg()?;
    let index = shared.index()?;
    let client = NeteaseClient::new(&cfg)?;

    p.say(format!("取回 {} 首的元数据 …", ids.len()));
    let tracks = client.songs_detail(&ids)?;
    if tracks.is_empty() {
        bail!("接口没有返回这些曲目的详情。id 可能不对，或者当前是匿名状态拿不到。");
    }

    let dir = cfg.out_root.join(naming::group_rel("netease", group.as_deref()));
    p.say(format!("落地目录：{}", dir.display()));
    let stems = naming::unique_stems(&tracks);
    let fetcher = Fetcher::new(&client, &cfg);

    let mut done = 0usize;
    let mut cached = 0usize;
    let mut failed = 0usize;
    let mut files: Vec<Value> = Vec::new();

    for (i, (t, stem)) in tracks.iter().zip(stems.iter()).enumerate() {
        p.say(format!("[{}/{}] {} - {}", i + 1, tracks.len(), t.name, t.artist_line()));

        // 已经在库里的先跳过，免得白跑一趟网络。
        // 日推不进索引，所以还要再问一次磁盘。
        if index.file_of(&t.key()).is_some() {
            cached += 1;
            p.say("   索引里已经有记录，跳过");
            continue;
        }

        match fetcher.ensure_stem(t, &dir, stem, false) {
            Ok(outcome) => {
                let file = outcome.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                for n in &outcome.notes {
                    p.say(format!("   备注：{n}"));
                }
                if outcome.from_cache {
                    cached += 1;
                    p.say(format!("   磁盘上已经有了，跳过：{file}"));
                } else {
                    done += 1;
                    p.say(format!(
                        "   完成：{file}（{}，{}）",
                        outcome.quality,
                        human_bytes(outcome.bytes)
                    ));
                }
                files.push(json!({ "id": t.id, "name": t.name, "path": outcome.path.to_string_lossy() }));
            }
            Err(e) => {
                failed += 1;
                p.say(format!("   失败：{e:#}"));
            }
        }
    }

    p.say(format!("结束：下载 {done} 首、已存在 {cached} 首、失败 {failed} 首"));
    if done == 0 && cached == 0 && failed > 0 {
        bail!("全部失败。常见原因是没登录（会员曲目）或凭据失效。");
    }
    Ok(json!({ "done": done, "cached": cached, "failed": failed, "dir": dir.display().to_string(), "files": files }))
}

fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    // 不到 1 KB 就直接报字节数：硬套 KB 会得到「0 KB」，比不说还糟
    if n < 1024 {
        return format!("{n} B");
    }
    let mb = n as f64 / KB / KB;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.0} KB", n as f64 / KB)
    }
}

// ---------- 挂载 ----------

fn api_mount(shared: &Arc<Shared>, body: &Value) -> Result<Value> {
    if !mount::fuse_supported() {
        bail!(
            "挂载需要 FUSE，而它只在 Linux 上可用（当前是 {}）。\n\
             飞牛 fnOS 就是 Debian/Linux：把 musicm 拷过去跑就能挂载；\
             在 {} 上这个界面只能用来管理索引和下载。",
            std::env::consts::OS,
            std::env::consts::OS
        );
    }

    let raw = b_str(body, "mountpoint").unwrap_or("").trim();
    if raw.is_empty() {
        bail!("请填挂载点，比如 /vol1/1000/music");
    }
    let mountpoint = PathBuf::from(raw);
    let ondemand = b_str(body, "mode").map(|m| m != "cached").unwrap_or(true);
    let allow_other = b_bool(body, "allow_other", false);
    let threads = b_u64(body, "threads").unwrap_or(4).clamp(1, 16) as usize;

    if let Some(fst) = mount::mounted_fstype(&mountpoint) {
        bail!("{} 上已经挂着 {fst} 了，先卸载再挂。", mountpoint.display());
    }

    let cfg = shared.cfg()?;
    let index = shared.index()?;
    if index.playlists.is_empty() && index.files.is_empty() {
        bail!("索引是空的，挂载出来也是个空目录。先扫描一张歌单。");
    }

    {
        let mut slot = lock(&shared.mount);
        if slot.running {
            bail!(
                "已经有一个挂载在跑（{}），先卸载它再挂别的。",
                slot.mountpoint.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
        *slot = MountSlot {
            mountpoint: Some(mountpoint.clone()),
            mode: if ondemand { "ondemand".to_string() } else { "cached".to_string() },
            running: true,
            started_at: now_secs(),
            message: Some("正在挂载…".to_string()),
        };
    }

    let slot = Arc::clone(&shared.mount);
    let mode_label = if ondemand { "ondemand" } else { "cached" };
    // 挂载线程要拿走挂载点，而外面还要用它拼响应，所以先留一份
    let thread_mp = mountpoint.clone();
    thread::spawn(move || {
        let outcome = (|| -> Result<()> {
            // 先按索引建树，再把索引交给按需取回器（顺序不能反：
            // build 只借用 index，materializer 要拿走它）
            let tree = vfs::Vfs::build(&cfg.out_root, &index, cfg.quality, ondemand);
            let materializer: Box<dyn vfs::Materializer> = if ondemand {
                Box::new(crate::materialize::LibraryMaterializer::new(&cfg, index)?)
            } else {
                Box::new(crate::materialize::OfflineMaterializer)
            };
            mount::mount_now(
                &cfg,
                MountArgs { mountpoint: thread_mp.clone(), ondemand, allow_other, threads },
                tree,
                materializer,
            )
        })();

        let mut guard = lock(&slot);
        guard.running = false;
        guard.message = Some(match outcome {
            // mount_now 只在被卸载时正常返回
            Ok(()) => format!("已卸载 {}", thread_mp.display()),
            Err(e) => format!("{e:#}"),
        });
    });

    Ok(json!({
        "accepted": true,
        "mountpoint": mountpoint.display().to_string(),
        "mode": mode_label,
        "note": "挂载线程已启动。刷新「概览」可以看到它是否真的挂上了。",
    }))
}

fn api_unmount(shared: &Arc<Shared>, body: &Value) -> Result<Value> {
    let raw = b_str(body, "mountpoint").unwrap_or("").trim();
    let target = if raw.is_empty() {
        // 没给就用我们记下的那个：界面上点「卸载」时不该让用户再填一遍路径
        lock(&shared.mount)
            .mountpoint
            .clone()
            .ok_or_else(|| anyhow!("还没有挂载过，也没有给出要卸载的路径"))?
    } else {
        PathBuf::from(raw)
    };

    if mount::mounted_fstype(&target).is_none() {
        bail!("{} 现在没有挂载（内核里查不到），不用卸载。", target.display());
    }

    mount::unmount(&target)?;

    let mut slot = lock(&shared.mount);
    slot.running = false;
    slot.message = Some(format!("已卸载 {}", target.display()));
    Ok(json!({ "unmounted": target.display().to_string() }))
}

// ---------- 任务查询 ----------

fn api_jobs(shared: &Shared) -> Result<Value> {
    let jobs = lock(&shared.jobs);
    let items: Vec<Value> = jobs.items.iter().map(|j| lock(j).summary()).collect();
    Ok(json!({ "jobs": items }))
}

fn api_job(shared: &Shared, id: &str) -> Result<Value> {
    let wanted: u64 = id
        .parse()
        .map_err(|_| anyhow!("任务 id 不是数字: {id}"))?;
    let jobs = lock(&shared.jobs);
    let job = jobs
        .items
        .iter()
        .find(|j| lock(j).id == wanted)
        .ok_or_else(|| anyhow!("没有任务 {wanted}"))?;
    Ok(lock(job).detail())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_round_trips_chinese() {
        // 前端用 encodeURIComponent 编码中文和空格，这里必须原样还原
        assert_eq!(percent_decode("%E5%91%A8%E6%9D%B0%E4%BC%A6"), "周杰伦");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a+b", "`+` 不是空格，那是表单编码的规矩");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn broken_escapes_are_left_alone() {
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn query_pairs_are_split_and_decoded() {
        let q = parse_query("q=%E5%91%A8&limit=20&offset=0");
        assert_eq!(q.get("q").map(String::as_str), Some("周"));
        assert_eq!(q_usize(&q, "limit", 5), 20);
        assert_eq!(q_usize(&q, "offset", 5), 0);
        assert_eq!(q_usize(&q, "missing", 7), 7, "缺省值要生效");
    }

    #[test]
    fn ids_accept_both_numbers_and_keys() {
        let body: Value = serde_json::from_str(r#"{"ids":[123,"456","netease:789"]}"#).unwrap();
        assert_eq!(b_ids(&body, "ids"), vec!["123", "456", "netease:789"]);
        assert!(b_ids(&Value::Null, "ids").is_empty());
    }

    #[test]
    fn token_is_required_only_when_bound_outside_loopback() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.10".parse().unwrap();

        // 只听本机：默认不要令牌，界面开箱即用
        assert!(decide_token(&opts(None, false), loopback).is_none());
        // 听局域网：必须有一个，没给就自动生成
        assert!(decide_token(&opts(None, false), lan).is_some());
        // 用户自己给的那必须用他的
        assert_eq!(decide_token(&opts(Some("abc".into()), false), lan).as_deref(), Some("abc"));
        // 明确裸奔时不生成
        assert!(decide_token(&opts(None, true), lan).is_none());
    }

    #[test]
    fn generated_tokens_differ() {
        // 同一个进程里连着生成两次不该撞上，否则令牌形同虚设
        assert_ne!(random_token(), random_token());
    }

    fn opts(token: Option<String>, no_auth: bool) -> ServeOptions {
        ServeOptions { data_dir: None, listen: String::new(), token, no_auth }
    }

    /// 空结果必须带理由：不带理由的空列表会让人以为程序坏了。
    #[test]
    fn empty_search_groups_always_carry_a_reason() {
        let empty = search_group(SearchKind::Song, &ListShape::Empty, 200, 0, vec![]);
        assert!(empty["empty"].as_str().unwrap().contains("没有匹配"));

        let missing = search_group(SearchKind::Playlist, &ListShape::Missing, 200, 0, vec![]);
        assert!(missing["empty"].as_str().unwrap().contains("改版"));

        // 有内容时不该冒出理由字段
        let filled = search_group(SearchKind::Song, &ListShape::Items, 200, 1, vec![json!({})]);
        assert!(filled["empty"].is_null());
    }

    #[test]
    fn byte_sizes_read_like_a_human_expects() {
        assert_eq!(human_bytes(512), "512 B", "不到 1 KB 不能报成 0 KB");
        assert_eq!(human_bytes(2048), "2 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
