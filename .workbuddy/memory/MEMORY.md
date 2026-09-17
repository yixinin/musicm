# musicm — project conventions

Turns Netease/QQ Music playlists into a local music library that 飞牛音乐 (fnOS's
native music app) can scan and play.
(Memory files are in English per a standing user request, even though the CLI and UI are Chinese.)

## Hard constraints

- **Target runtime is 飞牛 fnOS (Debian) / aarch64**; every dependency choice follows.
  - No bundled `rusqlite`, no `native-tls`/OpenSSL (a C toolchain on the NAS is painful).
    Use `ureq` + rustls, `lofty` (pure Rust). `ring` is the only dep needing gcc.
  - Build on the NAS with `cargo build --release`; don't cross-compile.
- Dev machine is Windows x86_64; keep code portable (no platform-specific APIs).
- Data dir `$HOME/.musicm` (`config.json` + `index.json`); music lands in
  `<data_dir>/library`, overridable with `--out`. Use ASCII paths in deployment.

## Architecture decisions (do not silently revert)

- Index is **JSON** (atomic write: temp file + rename), not SQLite. Move to `redb` at ~100k tracks.
- **Scanning and playback are strictly separated**: scanning/listing reads only the index and
  makes zero network calls; only fetching audio hits the API. That's what keeps thousands of
  tracks from timing out.
- **Metadata must be embedded** (fnOS's FAQ: cover/lyric matching fails without it). Download
  flow is fixed: temp file (keeping the audio extension) → tag → atomic rename; failures never
  leave half-finished files in the library.
- ID3 written as **2.3** (`WriteOptions::use_id3v23(true)`).
- Lyrics written twice: timestamped `.lrc` + stripped plain text into USLT.
- Quality ladder `母带 → hires → 无损 → 极高 → 标准`, descending. The server may return 320k mp3
  when asked for lossless, so **requested and actual level are recorded separately**
  (`AudioInfo::quality` vs `AudioInfo::actual_level`).
- Netease field types are unstable (skill `netease-music-api`): **all** raw fields use the
  lenient `flex::*` deserializers, never bare `#[derive(Deserialize)]`.
- **Empty-vs-missing field rule — applies to every list API**: an empty array and an absent
  field must stay distinguishable; never print a bare "0 条".
  - `DailyShape`: field present but empty = not logged in; field absent = API changed.
  - `ListShape` (search / account playlists): on no match the array vanishes entirely
    (`{"result":{"playlistCount":0}}`); rule is "array absent **and** count absent" = changed.
    Read the count with `flex::as_opt_u64` — `#[serde(default)]` collapses 0 and absent and
    destroys the evidence. The self-reported total (`songCount=336`) is usually far larger than
    the page size; display uses it.
  - Helpers: `as_opt_u64`, `as_opt_bool` (`null` = unknown ≠ `false`; `/user/playlist`'s
    `subscribed` is null when anonymous), `MaybeList::is_present/into_vec`.

## Paths: one source of truth

- **Virtual tree paths == on-disk paths.** Both come from the same functions:
  `naming::group_rel(source, playlist_name)` for dirs, `naming::unique_stems(tracks)` for stems.
  Anything that builds its own paths means the cache never hits.
- So `fetch.rs`'s real entry point is `ensure_stem(track, dir, stem, force)` — the caller (index
  or virtual tree) supplies dir + stem; `Fetcher::fetch()` is a convenience wrapper.
- **Duplicate names inside a playlist must be deduped** (`unique_stems` appends ` (2)`): when
  FUSE's `readdir` emits duplicate entries the kernel dentry cache keeps only one.
- **The extension follows the container actually written**: guessed from the level before landing
  (`Quality::expected_ext`); on mismatch `Vfs::adopt` renames and re-registers the inode.
- `naming::landed_stems(dir)` = stems already on disk (skips `.part` shards); lives in `naming.rs`
  because CLI and Web both need it. `naming::DAILY_GROUP` ("每日推荐") likewise.
- **One landing helper for every entry point**: `search` / `artist` / `daily` / `play` share
  `fetch_many(tracks, group, count, label)` — dir from `group_rel` (`None` → `<音源>/单曲`;
  daily passes `Some(DAILY_GROUP)`), stems from `unique_stems`, and it checks `index.file_of`
  first to skip already-landed tracks. Hand-built paths store the same song in two directories.
- Search results **must not be landed directly**: the search API's `album` has only `picId`, no
  `picUrl` (verified), so call `songs_detail(&ids)` first or the cover is lost — and fnOS
  scraping depends on it.

## Platform-specific code placement (learned the hard way)

- **Anti-pattern: a `#[cfg(target_os = "linux")]` block inside `main.rs`** — neither side compiles
  it (Windows `cargo build` skips it; `linuxcheck` doesn't reference `main.rs`). Real cost:
  `println!("已卸载 {mountpoint}")` (`PathBuf` has no `Display`) passed 21 green tests with zero
  warnings on the dev machine, then blew up on the NAS.
- **Correct: platform-specific code in a "leaf module that is always compiled", forking with
  `#[cfg]` inside the file.** `src/mount.rs` plays that role; `main.rs` declares `mod mount;`
  unconditionally.
- The only `#[cfg]` allowed in `main.rs` is a **module declaration** like
  `#[cfg(target_os = "linux")] mod fuse_fs;` — it gates a whole file that is already in the
  `linuxcheck` list.
- Self-check after changes: `Grep "cfg\(target_os"` and confirm each gated item lives in a file
  listed in `tools/linuxcheck/src/lib.rs`.

## FUSE export

- `vfs.rs` = platform-independent tree core (unit-testable everywhere); `fuse_fs.rs` = thin Linux
  adapter.
- **No libfuse**: `fuser 0.18` with `default-features = false` gives a pure-Rust mount path via
  `fusermount3`; the NAS only needs `apt install fuse3`.
- Fetch happens in `open()`, never in `read()`; `lookup`/`getattr`/`readdir` never touch the network.
- Two modes: `--fuse-mode ondemand` (list everything, fetch on first read) / `cached` (list only
  what's landed, zero network). **Use cached for 飞牛扫描**, otherwise a full scan = downloading
  the whole library.
- **The tree is built at mount time, then incrementally refreshed.** `Vfs::build` runs once; after
  that `Vfs::refresh` merges "index ∪ disk" into the live tree, gated by `RefreshGate`
  (`mount.rs`, so both platforms compile and test it) and hooked on FUSE `readdir` — 30 s
  (`mount::REFRESH_SECS`, shared with the CLI hint so the two can't drift). This is what makes
  `daily --fetch` after mounting visible without a remount.
  - **Never rebuild the tree, only merge.** Inodes must stay put (kernel dentry cache). Merge reuses
    `by_rel`; only new paths get new numbers.
  - **Additive only, never delete.** Removing "not found on disk" nodes races with `Vfs::adopt`: the
    fetch flow is "placeholder with guessed ext → land → adopt renames", and during that window the
    guessed path legitimately doesn't exist. Cost: manually deleted files linger until remount.
  - `RefreshStats` counts *new file nodes* and *pending→materialised upgrades* separately; counting
    the latter as "materialised nodes went up" would count freshly landed files as upgrades and
    make the log lie.
- Files land on disk and the tree is built from a disk scan, so **a landed directory that belongs
  to no playlist still shows up in both modes** (verified with 每日推荐).
- `--allow-other` is needed when 飞牛音乐 runs as a different user (`/etc/fuse.conf` must
  uncomment `user_allow_other`).
- `mount.rs` also owns the Web UI's control surface, platform-forked inside that file:
  `fuse_supported()` (`const fn` returning a runtime bool so the UI can grey out the button),
  `unmount()` (shells out to `fusermount3`/`fusermount -u`), `mounted_fstype()` (reads
  `/proc/mounts`, since the in-memory "mounted" flag is lost on restart).
- `cmd_mount`'s guard is **"is there anything in the tree", not "is the index non-empty"**. It once
  required `index.tracks > 0` ("you probably forgot to scan"), which rejected the legitimate case of
  a library holding only daily/search/single-track landings — none of which are indexed by design.
  Now: error only when tree and index are *both* empty; index-empty prints "this is pure disk
  content"; index-non-empty + cached + empty tree prints "nothing landed yet". All four cases
  smoke-tested.

## Verification: `tools/linuxcheck`

- The dev machine is Windows, so `fuse_fs.rs` and the Linux branch of `mount.rs` are not in the
  host compile graph. Run `cd tools/linuxcheck && cargo check --target aarch64-unknown-linux-gnu`.
- It `#[path]`-includes the **real** source files, so it can never go stale. **After adding a
  platform-specific file, add it to `tools/linuxcheck/src/lib.rs`**, or the blind spot returns.
  "Done" = `cargo build` + `cargo test` + the line above.
- **Must run inside `tools/linuxcheck/`, never the repo root**: the root crate depends on `ring`,
  so `cargo check --target aarch64-…` there looks for `aarch64-linux-gnu-gcc` and fails with
  `failed to run custom build command for ring` — wrong directory, not bad code.
- Currently checked: `model` `auth` `qr` `config` `naming` `store` `vfs` `fuse_fs` `mount`.
  `auth.rs`'s `write_private`/`harden` are `#[cfg(unix)]` — only the cross check compiles them, so
  run it after touching credential persistence.
- Gotchas: **warnings replay from the incremental cache** (`cargo clean -p linuxcheck` to confirm;
  on a cache hit nothing prints, so "fast and silent" ≠ clean). **`cargo test` does not produce
  `target/debug/musicm.exe`** — run `cargo build` before CLI smoke tests. PowerShell's
  `& exe args *> file` writes **UTF-16LE**; transcode before reading.

## Web admin UI (`serve`)

- `musicm serve [--listen 127.0.0.1:8765] [--token T] [--no-auth]`: everything the CLI can do over
  HTTP (search, login, scan, download, mount, jobs).
- **Hand-written HTTP server on `std::net`, zero new dependencies** (`axum` drags in
  tokio/hyper/tower; `tiny_http` saves little). Documented simplifications: `Connection: close`
  only, `Content-Length` bodies only, no HTTPS (reverse proxy in front).
- Front end `src/web/{index.html,app.css,app.js}` embedded with `include_str!` so the NAS binary is
  self-contained; no CDN, no framework.
- **Auth: a non-loopback listen requires a token.** Omitted → one is generated and printed as a
  ready-to-click URL; `--no-auth` opts out with a warning. Token is checked on `/api/*` only so
  static assets load first.
- **`src/web.rs` must NOT be added to `linuxcheck`** — it depends on `netease`/`fetch`
  (→ `ureq`/`ring`), and linuxcheck deliberately keeps only pure-Rust deps. It is
  platform-independent, so host `cargo build` covers it — which is exactly why all platform bits
  live in `mount.rs`.
- Long operations (scan / download) run as **background jobs** with an append-only log the UI polls
  (`/api/jobs`, `/api/jobs/:id`): a 200-track download can't block an HTTP request. Each job
  re-loads `Config`/`Index` itself rather than sharing mutable state (`Index` isn't `Clone`; a
  long-held lock would block every other request). `Shared` also re-`Config::load`s per request,
  so hand-edited `config.json` is picked up immediately.
- `/api/play` writes back to the index (same path as `musicm play`, incl. `remember_track`);
  `/api/fetch` deliberately does not (matches `daily --fetch`).

## Credentials & login

- ⚠️ **QR login is blocked by Netease risk control; never make it the main path.** After a
  successful scan it returns `code=8821` ("行为验证码"). It arrives *after* the scan succeeds, so
  it looks like slowness; it can never succeed and re-scanning or changing `type` doesn't help
  (skill §9).
- **Pasting a cookie is the reliable path**: `login --cookie '<串>'` / `--cookie-file <路径>` /
  stdin. The value must contain `MUSIC_U` (`MUSIC_A` alone is not a login).
- **`8821` must be terminal.** It once shared `QrState::Unknown` with "sidecar returned `{}`", so
  users polished a dead QR for 5 minutes. Three states now: `Empty` (no code, keep polling) /
  `Blocked` (8821, stop now) / `Unrecognized` (print raw code+message, keep polling), arbitrated
  by `QrState::is_terminal()` with a `debug_assert!` invariant in the poll loop.
- Cookie lives **only in `<data_dir>/cookie.txt`**, mode 600 on Unix, never written back to
  `config.json`. That's why `Config::cookie` uses `#[serde(default, skip_serializing)]` — **not**
  `skip_serializing_if = "Option::is_none"` (that only skips `None`, so plaintext still lands on
  disk).
- Report where credentials came from (`auth::Origin`: env / file / migrated) so users know what to
  clear when they expire.
- `--cookie` belongs to `login` only — no global same-named flag (it once wrote twice per
  invocation).
- Accept three paste shapes: bare `MUSIC_U=…`, labelled `Cookie: MUSIC_U=…` (DevTools header line),
  a whole "copy as cURL". **The label is recognised only at the start** — a legitimate value
  containing `cookie:` (`MUSIC_U=abc; note=cookie:x`) must not be truncated; two tests guard each
  other.
- ⚠️ `ureq` reads `HTTP_PROXY`/`ALL_PROXY`. **Running through a proxy easily trips risk control**
  (datacenter egress IP) and breaks `127.0.0.1` test servers with `CONNECT proxy failed` — clear
  the proxy env vars for local smoke tests.

## Daily recommendations (`daily`)

- ⚠️ **This API fails as a silent empty array**: both "not logged in" and "cookie expired" return
  `code=200` + `recommend: []` with **no error code** (same trap as `account`'s `profile:null`).
  `netease.rs` uses a three-state `flex::MaybeList` (Missing / Empty / Items) so "field absent =
  API changed" is separable from "present but empty = not logged in"; the CLI picks its wording
  from whether a cookie exists locally.
- **Both response shapes must parse**: plain `/api/` gives `{code, recommend:[…]}`,
  weapi/newer sidecars give `{code, data:{dailySongs:[…]}}`; App-style `ar`/`al`/`dt` are
  recognised via `#[serde(alias)]` on `RawTrack`. Sidecar path is `/recommend/songs` (not
  `/v1/discovery/…`) and needs `timestamp=` to break cache.
- **Deliberately not indexed**: daily changes every day and would pile one-off tracks into the
  index. `--fetch N` only writes files to `<out>/网易云/每日推荐/`. "Already landed" = check the
  index, then the directory's stems.
- Consequences to state plainly: these files don't appear in `musicm info` stats, and
  `play <id>` won't reuse the landed copy (it fetches a new one into `网易云/单曲/`).
- **Mounting them** (verified on a mock library): the tree is built from a disk scan of `<out>`, so
  a landed `每日推荐` directory shows up in **both** modes, but daily is never in `plan()`, so
  ondemand mode gives it **no placeholder** — it must be landed first (`daily --fetch N`), and
  there is no fetch-on-read path for it. Since 2026-09-17 the mount also refreshes itself (see the
  FUSE section), so a `daily --fetch` after mounting appears within 30 s without a remount.
- Working recipe: `scan <歌单id>` once (or skip it — the guard now allows a daily-only library),
  `daily --fetch N`, then `mount <挂载点> --fuse-mode cached --allow-other`; or point `--out` at
  the fnOS media folder and skip FUSE entirely, since daily files are ordinary tagged files.

## Search / account playlists / artists

- API details and three response-shape traps live in the `netease-music-api` skill §11 — read it
  before touching these commands.
- The `--type` → numeric type mapping (1 / 1000 / 100) **fails silently, you just never find
  anything**, so the `search_type_maps_to_the_api_kinds` test is a rivet.
- `/user/playlist`'s `playlist` is **top-level** (not under `result`); `more` means another page;
  anonymous sees only public playlists. `artist/top/song`'s `song` is top-level and **`limit` does
  not work** (fixed 50) — truncate yourself.
- Keywords must be percent-encoded (`encode_query`, hand-rolled, no dep): Chinese, spaces, `&`,
  `#` all occur and raw concatenation shreds the query string.
- The three result kinds lead to different next actions (songs can land / playlists need scan /
  artists need another query), so print three blocks with the next command at the end of each;
  mark 已索引 / 已落地 from the index.

## CLI surface

`scan <歌单id>` · `playlists [--remote [--uid N] [--limit N]]` ·
`tracks <歌单id> [--limit]` · `daily [--limit N] [--fetch N]` ·
`search <关键词> [--type all|song|playlist|artist] [--limit N] [--offset N] [--fetch N]` ·
`artist <歌手id> [--limit N] [--fetch N]` · `url <曲目id>` · `play <曲目id> [--force]` ·
`mount <挂载点> [--fuse-mode ondemand|cached] [--allow-other] [--threads N]` ·
`login [--qr] [--cookie <串> | --cookie-file <路径>]` · `logout` · `whoami` · `info` ·
`serve [--listen ADDR] [--token T] [--no-auth]`
Global flags `--data-dir / --out / --quality`; **given ones are written to config and reused**.

- `playlists` reads the index by default; `--remote` lists the account's playlists (self by
  default, needs login; `--uid` allows anonymous lookup of someone else's public ones).
- `play <id>` **accepts bare numeric ids not in the index** (fetches metadata via `songs_detail`
  on the spot), which is what makes ids from `search`/`artist` useful.
- `whoami` really hits the account API (distinguishes "no login" from "login expired"), so it
  fails offline — deliberately; it's a probe.

## Milestone status

- ✅ M0 source access (direct anonymous API)
- ✅ M1 playlist scan + index persistence
- ✅ M2 single-track landing (resolve → download → tag → lyrics/cover)
- ✅ M4a FUSE export (read-only, on-demand, tree verified with real data)
- ✅ M5a credentials: QR + credential file + Direct/Sidecar modes
- ✅ M7 Web admin UI (`serve`)
- ✅ `daily` standalone command (list + `--fetch`, deliberately unindexed)
- ⬜ M3 cache quota & prefetch queue (ondemand FUSE experience depends on it)
- ⬜ M4b WebDAV export (fnOS remote mount, no root needed — higher priority)
- ⬜ M5b real lossless/master landing (login works; VIP resolve rate unverified)
- ⬜ M6 QQ Music source + cross-source dedup
