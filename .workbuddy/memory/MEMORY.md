# musicm — project conventions

把网易云/QQ 音乐的歌单变成本地音乐库，供飞牛音乐（fnOS 原生音乐应用）扫描播放。

## Hard constraints

- **目标运行环境是飞牛 fnOS（Debian）/ aarch64。** 所有依赖选型都要服从这条。
  - 不用 `rusqlite` bundled、不用 `native-tls`/OpenSSL —— NAS 上装 C 工具链很麻烦。
  - 用 `ureq` + rustls、`lofty`（纯 Rust）。`ring` 是唯一需要 gcc 的依赖。
  - 直接在 NAS 上 `cargo build --release`，不要折腾交叉编译。
- 开发机是 Windows x86_64，代码必须保持可移植（不要写平台相关 API）。
- 数据目录默认 `$HOME/.musicm`（配置 `config.json` + 索引 `index.json`），
  音乐落地目录默认 `<data_dir>/library`，可用 `--out` 覆盖。部署时用纯英文路径。

## Architecture decisions (do not silently revert)

- 索引用 **JSON**（原子写：临时文件 + rename），不是 SQLite。规模到十万级再换 `redb`。
- **扫描与播放严格分离**：扫描/列目录只读索引，零网络请求；只有真正取音频时才发接口。
  这是几千首曲目不超时的前提。
- **元数据必须内嵌**。飞牛音乐官方 FAQ 明确：元信息缺失时封面与歌词匹配会失败。
  下载流程固定为「临时文件（保留音频扩展名）→ 打标 → 原子改名」，失败不留半成品。
- ID3 写 **2.3**（`WriteOptions::use_id3v23(true)`），对齐网易云自身文件与兼容性。
- 歌词双写：带时间戳的 `.lrc` + 剥掉时间戳的纯文本进 USLT。
- 音质降级链：`母带 → hires → 无损 → 极高 → 标准`，逐档尝试。
  服务端可能「请求无损返回 320k mp3」，所以**请求档位与实际档位必须分开记录**
  （`AudioInfo::quality` vs `AudioInfo::actual_level`）。
- 网易云响应字段类型不稳定（见技能 `netease-music-api`），**所有**原始字段都要用
  `netease.rs` 里的 `flex::*` 宽松反序列化，不要直接 `#[derive(Deserialize)]` 裸字段。

## 路径与文件树的唯一真相来源（别绕过）

- **虚拟树的路径 = 磁盘上的路径**，两边必须由同一套函数算出来：
  `naming::group_rel(source, playlist_name)` 给目录，`naming::unique_stems(tracks)` 给文件名主干。
  任何一边自己拼路径，缓存就会永远命不中。
- 因此 `fetch.rs` 的对外入口是 `ensure_stem(track, dir, stem, force)`——
  目录和主干由调用方（索引或虚拟树）给，`Fetcher::fetch()` 只是它的一个便利包装。
- 歌单内**同名文件必须去重**（`unique_stems` 给第 2、3 个加 ` (2)`）：
  FUSE 的 `readdir` 吐出重名条目时，内核 dentry 缓存只会留一个。
- 曲目文件的**扩展名以实际落盘的容器为准**。未落盘时按档位猜（`Quality::expected_ext`），
  猜错时 `Vfs::adopt` 负责改名并重新登记 inode。

## 出口层：FUSE（M4 上半，已实现）

- `vfs.rs` 是平台无关的文件树内核（可跨平台单测），`fuse_fs.rs` 是 Linux 专属的薄适配层。
  这么切的原因：FUSE 代码在 Windows 开发机上编译不到，能移出平台专属层的逻辑必须移出去。
- **不要为了 FUSE 去引 libfuse**：`fuser 0.18` 的 `default = []`，不开 `libfuse` 特性时
  Linux 上走 pure-rust 挂载路径，靠 `fusermount3` 完成挂载；NAS 上只需 `apt install fuse3`。
- 挂载动作按平台分叉，但建树与摘要打印是共享的：`cmd_mount` → `mount::mount_now()`。
  非 Linux 平台会把树骨架打印出来再报「FUSE 只在 Linux 上可用」，不是直接编译失败。

## 平台专属代码的位置（踩过坑，别再改回去）

- **反模式：在 `main.rs` 里用 `#[cfg(target_os = "linux")]` 挖掉一块代码。**
  那样两边都编不到它——Windows 的 `cargo build` 看不到 Linux 那一支，
  `tools/linuxcheck` 也没引用 `main.rs`。真实后果：`println!("已卸载 {mountpoint}")`
  （`PathBuf` 没实现 `Display`）在开发机上 21 个测试全绿零警告，NAS 上一编译就炸。
- **正解：平台专属代码放进「总是被编译的叶子模块」，`#[cfg]` 留在文件内部。**
  现在承担这个角色的是 `src/mount.rs`：`main.rs` 无条件 `mod mount;`，
  文件内部再按平台分叉。Windows 构建编非 Linux 那一支，交叉检查编 Linux 那一支，
  合起来每一行都被真正编译过。
- 允许留在 `main.rs` 的只有 `#[cfg(target_os = "linux")] mod fuse_fs;` 这种
  **模块声明**——它门控的是整个文件，而那个文件已经在 `linuxcheck` 列表里。
- 改动后自查：`Grep "cfg\(target_os"` 看每个门控落在哪个文件，
  确认那个文件在 `tools/linuxcheck/src/lib.rs` 里。
- 取回放在 `open()` 而不是 `read()`；`lookup/getattr/readdir` 一律不联网。
- 两种模式：`--fuse-mode ondemand`（列出全部，首读取回）/ `cached`（只列已落盘，零网络）。
  **给飞牛音乐扫描用 cached**，否则整库扫描 = 全量下载。

## 验证手段：`tools/linuxcheck`

开发机是 Windows，`fuse_fs.rs` 与 `mount.rs` 的 Linux 分支不在宿主编译图里
（改坏了 `cargo build` 都不报错），所以每次动 Linux 专属代码后必须跑：

```
cd tools/linuxcheck && cargo check --target aarch64-unknown-linux-gnu
```

它用 `#[path]` 引用真实源文件，没有副本所以不会过期。**新增平台专属文件后，
必须把那个文件也加进 `tools/linuxcheck/src/lib.rs`，否则盲区原样回来。**
原理、自查清单与「怎么证明检查器没在骗你」见技能 `rust-cross-target-check`。
三个命令一起跑才算完：`cargo build` + `cargo test` + 上面这条。

## Milestone status

- ✅ M0 打通音源（直连匿名接口）
- ✅ M1 歌单扫描 + 索引持久化
- ✅ M2 单曲落地（取链 → 下载 → 打标 → 歌词/封面）
- ✅ M4a FUSE 出口（只读、按需取回、树骨架已用真实数据验证）
- ⬜ M3 缓存配额与预取队列（`--fuse-mode ondemand` 的体验取决于它）
- ⬜ M4b WebDAV 出口（飞牛远程挂载，零 root 更优先）
- ⬜ M5 cookie 登录态 + `ApiMode::Sidecar`（解灰/无损）
- ⬜ M6 QQ 音乐音源 + 跨源合并去重

## CLI surface

`scan <歌单id>` · `playlists` · `tracks <歌单id> [--limit]` · `url <曲目id>` ·
`play <曲目id> [--force]` · `mount <挂载点> [--fuse-mode ondemand|cached] [--allow-other] [--threads N]` · `info`
全局参数 `--data-dir / --out / --quality / --cookie`，**给了就写入配置并沿用**。
