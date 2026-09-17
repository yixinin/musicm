//! 把虚拟文件树接到真实音源上的那一层。
//!
//! [`crate::vfs`] 只知道自己有一堆「还没落地的节点」，不知道网易云的存在；
//! 这里负责在需要的时候真正去取回并打标。
//!
//! 这个模块本身不依赖任何平台特性，所以在 Windows 上也能编译和测试；
//! 只有真正挂载文件系统的那一半（`fuse_fs`）是 Linux 专属的。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::Result;

use crate::config::Config;
use crate::fetch::Fetcher;
use crate::model::now_secs;
use crate::netease::NeteaseClient;
use crate::store::Index;
use crate::vfs::{Materialized, Materializer};

/// 按需取回。FUSE 读到还没落地的曲目时走到这里。
pub struct LibraryMaterializer {
    client: NeteaseClient,
    cfg: Config,
    index: Mutex<Index>,
    index_path: PathBuf,
    /// 串行化取回。
    ///
    /// 接口本身有 300ms 限速，并发下载只会一起变慢，还更容易触发风控；
    /// 而且同一个文件被两个进程同时打开时，串行化天然避免了重复下载。
    gate: Mutex<()>,
}

impl LibraryMaterializer {
    pub fn new(cfg: &Config, index: Index) -> Result<Self> {
        Ok(LibraryMaterializer {
            client: NeteaseClient::new(cfg)?,
            cfg: cfg.clone(),
            index: Mutex::new(index),
            index_path: cfg.index_path(),
            gate: Mutex::new(()),
        })
    }
}

impl Materializer for LibraryMaterializer {
    fn materialize(&self, track_key: &str, dir: &Path, stem: &str) -> Result<Materialized, String> {
        let _gate = self.gate.lock().map_err(|_| "取回锁已损坏".to_string())?;

        let track = {
            let index = self.index.lock().map_err(|_| "索引锁已损坏".to_string())?;
            index.track(track_key).cloned()
        }
        .ok_or_else(|| format!("索引里没有曲目 {track_key}"))?;

        let fetcher = Fetcher::new(&self.client, &self.cfg);
        let outcome = fetcher
            .ensure_stem(&track, dir, stem, false)
            .map_err(|e| format!("{e:#}"))?;

        let mtime = std::fs::metadata(&outcome.path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or_else(now_secs);

        // 索引写成失败不算取回失败——音频已经在磁盘上了，重启后重新扫描即可
        if let Ok(mut index) = self.index.lock() {
            index.record_file(outcome.to_cached_file(&track));
            if let Err(e) = index.save(&self.index_path) {
                eprintln!("[musicm] 索引写入失败: {e:#}");
            }
        }

        Ok(Materialized {
            path: outcome.path,
            size: outcome.bytes,
            mtime,
        })
    }
}

/// `cached` 模式下用不到取回，但结构上得有一个。
///
/// 比起「构造一个真的 HTTP 客户端但永远不用」，这样更省事，也更容易看出意图。
pub struct OfflineMaterializer;

impl Materializer for OfflineMaterializer {
    fn materialize(&self, track_key: &str, _dir: &Path, _stem: &str) -> Result<Materialized, String> {
        Err(format!(
            "曲目 {track_key} 还没有落盘，而当前是 cached 模式（不联网）。\
             先执行 musicm play <id> 把它取下来，或者改用 --fuse-mode ondemand。"
        ))
    }
}
