# musicm — project conventions

Turns Netease/QQ Music playlists into a local music library that 飞牛音乐 (fnOS's
native music app) can scan and play.

(Memory files are written in English per a standing user request, even though the
CLI output and UI are Chinese.)

## Hard constraints

- **Target runtime is 飞牛 fnOS (Debian) / aarch64.** Every dependency choice
  follows from this.
  - No `rusqlite` bundled, no `native-tls`/OpenSSL — installing a C toolchain on
    the NAS is painful. Use `ureq` + rustls, `lofty` (pure Rust). `ring` is the
    only dep needing gcc.
  - Build directly on the NAS with `cargo build --release`; don't cross-compile.
- Dev machine is Windows x86_64; keep code portable (no platform-specific APIs).
- Data dir defaults to `$HOME/.musicm` (`config.json` + `index.json`); music lands
  in `<data_dir>/library`, overridable with `--out`. Use ASCII paths in deployment.

## Architecture decisions (do not silently revert)

- Index is **JSON** (atomic write: temp file + rename), not SQLite. Move to `redb`
  at ~100k tracks.
- **Scanning and playback are strictly separated**: scanning/listing reads only the
  index and makes zero network calls; only actually fetching audio hits the API.
  This is what keeps thousands of tracks from timing out.
- **Metadata must be embedded.** fnOS's own FAQ states cover/lyric matching fails
  without it. Download flow is fixed: temp file (keeping the audio extension) →
  tag → atomic rename. Failures never leave half-finished files in the library.
- ID3 written as **2.3** (`WriteOptions::use_id3v23(true)`).
- Lyrics written twice: timestamped `.lrc` + stripped plain text into USLT.
- Quality ladder: `母带 → hires → 无损 → 极高 → 标准`, tried in descending order.
  The server may "return 320k mp3 when asked for lossless", so **requested level
  and actual level are recorded separately** (`AudioInfo::quality` vs
  `AudioInfo::actual_level`).
- Netease response field types are unstable (see the `netease-music-api` skill):
  **all** raw fields must use the lenient `flex::*` deserializers in `netease.rs`,
  never bare `#[derive(Deserialize)]` fields.

## Single source of truth for paths (don't bypass it)

- **Virtual tree paths == on-disk paths.** Both must come from the same functions:
  `naming::group_rel(source, playlist_name)` for directories,
  `naming::unique_stems(tracks)` for filename stems. If either side builds its own
  paths, the cache never hits.
- Hence `fetch.rs`'s real entry point is `ensure_stem(track, dir, stem, force)` —
  caller (index or virtual tree) supplies dir + stem; `Fetcher::fetch()` is a
  convenience wrapper.
- **Duplicate names inside a playlist must be deduped** (`unique_stems` appends
  ` (2)`): when FUSE's `readdir` emits duplicate entries, the kernel dentry cache
  keeps only one.
- Track file **extension follows the container actually written**. Before landing
  it's guessed from the level (`Quality::expected_ext`); on mismatch `Vfs::adopt`
  renames and re-registers the inode.
- `naming::landed_stems(dir)` = which stems are already on disk (skips `.part`
  shards). Lives in `naming.rs` because both CLI and Web need it; two copies would
  drift and disagree about "downloaded".
- `naming::DAILY_GROUP` ("每日推荐") is the daily-recommendation directory name,
  shared by CLI and Web for the same reason.

## Platform-specific code placement (learned the hard way)

- **Anti-pattern: carving out a block in `main.rs` with
  `#[cfg(target_os = "linux")]`.** Neither side compiles it — Windows
  `cargo build` can't see the Linux branch, and `tools/linuxcheck` doesn't
  reference `main.rs`. Real consequence: `println!("已卸载 {mountpoint}")`
  (`PathBuf` has no `Display`) passed 21 green tests with zero warnings on the dev
  machine, then blew up on the NAS.
- **Correct: put platform-specific code in a "leaf module that is always
  compiled", with `#[cfg]` inside the file.** `src/mount.rs` plays that role:
  `main.rs` declares `mod mount;` unconditionally, the file forks internally.
  Windows builds the non-Linux branch, the cross-check builds the Linux one, so
  every line gets really compiled.
- The only `#[cfg]` allowed in `main.rs` is a **module declaration** like
  `#[cfg(target_os = "linux")] mod fuse_fs;` — it gates a whole file, and that file
  is already in the `linuxcheck` list.
- After changing things, self-check: `Grep "cfg\(target_os"` and confirm each gated
  item lives in a file listed in `tools/linuxcheck/src/lib.rs`.

## FUSE export (M4a, done)

- `vfs.rs` = platform-independent tree core (unit-testable everywhere);
  `fuse_fs.rs` = thin Linux adapter.
- **Don't pull in libfuse for FUSE**: `fuser 0.18` with `default-features = false`
  takes a pure-Rust mount path on Linux via `fusermount3`; the NAS only needs
  `apt install fuse3`.
- Fetch happens in `open()`, not `read()`; `lookup`/`getattr`/`readdir` never touch
  the network.
- Two modes: `--fuse-mode ondemand` (list everything, fetch on first read) /
  `cached` (list only what's landed, zero network). **Use cached for 飞牛 scanning**,
  otherwise scanning the whole library = downloading everything.
- `mount.rs` also owns the control surface the Web UI needs, all platform-forked
  inside that file: `fuse_supported()` (a `const fn` returning a runtime bool so the
  UI can grey out the button), `unmount()` (shells out to `fusermount3`/`fusermount`
  -u), `mounted_fstype()` (reads `/proc/mounts`, since after a restart the in-memory
  "mounted" flag is gone but the kernel mount is not).

## Verification: `tools/linuxcheck`

The dev machine is Windows, so `fuse_fs.rs` and the Linux branch of `mount.rs` are
not in the host compile graph. After touching Linux-only code, run:

```
cd tools/linuxcheck && cargo check --target aarch64-unknown-linux-gnu
```

It `#[path]`-includes the **real** source files, so it can never go stale. **After
adding a platform-specific file, add it to `tools/linuxcheck/src/lib.rs` too**, or
the blind spot returns. The three commands that count as "done": `cargo build` +
`cargo test` + the line above.

**Must be run inside `tools/linuxcheck/`, never the repo root.** The root
`musicm` crate depends on `ring`, so `cargo check --target aarch64-…` there goes
looking for `aarch64-linux-gnu-gcc` and fails with
`failed to run custom build command for ring` — that means wrong directory, not bad
code. `linuxcheck` deliberately keeps only pure-Rust deps.

Currently checked: `model` `auth` `qr` `config` `naming` `store` `vfs` `fuse_fs`
`mount`. `auth.rs`'s `write_private`/`harden` are `#[cfg(unix)]` — **only the cross
check compiles them**, so run it after touching credential persistence; the host
`cargo build` knows nothing about that branch.

Two gotchas:

- **Warnings replay from the incremental cache.** Seeing warnings on code you
  didn't touch: `cargo clean -p linuxcheck` and re-run; if they vanish it was a
  stale diagnostic (seen with `mount.rs`'s `unused import: Path`). Conversely, on a
  cache hit **nothing is printed**, so "fast and silent" is not proof of clean.
- **`cargo test` does not produce `target/debug/musicm.exe`.** Run `cargo build`
  before CLI smoke tests or you're testing the previous binary (symptoms look like
  "unrecognized subcommand"). Also, PowerShell's `& exe args *> file` writes
  **UTF-16LE** — transcode before reading.

## Web admin UI (`serve`, M7) — implemented 2026-09-17

- `musicm serve [--listen 127.0.0.1:8765] [--token T] [--no-auth]`. Everything the
  CLI can do is exposed over HTTP: search, login, scan, download, mount, jobs.
- **HTTP server is hand-written on `std::net` — zero new dependencies.** `axum`
  would drag in tokio/hyper/tower (expensive first build on aarch64); `tiny_http`
  is light but 200 lines of `std` buys the same thing given we only serve our own
  front end. Deliberate simplifications, each documented at the implementation:
  `Connection: close` only, `Content-Length` bodies only (no chunked), no HTTPS
  (put a reverse proxy in front).
- Front end is `src/web/{index.html,app.css,app.js}`, embedded with `include_str!`
  so the NAS binary is self-contained (no missing-asset white screens). No CDN, no
  framework, vanilla JS.
- **Auth rule: non-loopback listen requires a token.** Omitted → one is generated
  and printed as a ready-to-click URL; `--no-auth` opts out with a warning. The UI
  can change credentials and write to the music library, so an unauthenticated LAN
  port is not acceptable. Token is checked on `/api/*` only — static assets must
  load first so the user can even type a token.
- **`src/web.rs` must NOT be added to `linuxcheck`.** It depends on
  `netease`/`fetch` (→ `ureq`/`ring`), and `linuxcheck` deliberately keeps only
  pure-Rust deps. It's platform-independent, so host `cargo build` covers it — which
  is exactly why all platform-specific bits live in `mount.rs` instead.
- Long operations (scan / download) run as **background jobs** with an append-only
  log the UI polls (`/api/jobs`, `/api/jobs/:id`), because a 200-track download
  can't block an HTTP request. Each job reloads `Config`/`Index` itself rather than
  sharing mutable state — `Index` isn't `Clone` and a long-held lock would block
  every other request. Same reason `Shared` re-`Config::load`s per request: it picks
  up hand-edited `config.json` immediately.
- `/api/play` writes back to the index (same path as `musicm play`, incl.
  `remember_track` for tracks not yet indexed); `/api/fetch` deliberately does not
  (matches `daily --fetch`, so daily shoves don't pollute the index).

## Credentials & login (M5a)

- ⚠️ **QR login is blocked by Netease risk control; never make it the main path.**
  After a successful scan it returns `code=8821` ("行为验证码"). It arrives *after*
  the scan succeeds, so it looks like slowness — it can never succeed; re-scanning
  or changing `type` doesn't help. See `netease-music-api` skill §9.
- **Pasting a cookie is the reliable path**: `login --cookie '<str>'` /
  `--cookie-file <path>` / stdin. The value must contain `MUSIC_U` (only `MUSIC_A`
  is not a login).
- **`8821` must be terminal.** It once shared `QrState::Unknown` with "sidecar
  returned `{}`", so users polished away 5 minutes before erroring. Now three
  states: `Empty` (no code, keep polling) / `Blocked` (8821, stop now) /
  `Unrecognized` (print raw code+message, keep polling), arbitrated by
  `QrState::is_terminal()`, with a `debug_assert!` invariant in the poll loop.
- Cookie lives **only in `<data_dir>/cookie.txt`**, mode 600 on Unix, never written
  back to `config.json`. That's why `Config::cookie` uses
  `#[serde(default, skip_serializing)]` — **not**
  `skip_serializing_if = "Option::is_none"` (that only skips `None`, so plaintext
  still lands on disk; learned the hard way).
- Report where credentials came from (`auth::Origin`: env / file / migrated) so
  users know what to clear when they expire.
- `--cookie` belongs to `login` only — no global same-named flag (it once wrote
  twice per invocation).
- Accept three paste shapes: bare `MUSIC_U=...`, labelled `Cookie: MUSIC_U=...`
  (DevTools header line), a whole "copy as cURL". **The label is only recognised at
  the start** — a legitimate value containing `cookie:` (`MUSIC_U=abc; note=cookie:x`)
  must not be truncated; two tests guard each other.
- ⚠️ `ureq` reads `HTTP_PROXY`/`ALL_PROXY`. **Running through a proxy easily trips
  risk control** (datacenter egress IP) and also breaks `127.0.0.1` test servers
  with `CONNECT proxy failed` — for local smoke tests, clear the proxy env vars.

## Daily recommendations (`daily`)

- ⚠️ **This API fails as a silent empty array**: both "not logged in" and "cookie
  expired" return `code=200` + `recommend: []` with **no error code** (like
  `account`'s `profile:null`). So `netease.rs` uses a three-state `flex::MaybeList`
  (Missing / Empty / Items) to separate "field absent = API changed" from "field
  present but empty = not logged in" — collapsing to a `Vec` makes it
  unexplainable. The CLI then picks wording based on whether a cookie exists locally.
- **Both response shapes must parse**: plain `/api/` gives `{code, recommend:[...]}`,
  weapi/newer sidecars give `{code, data:{dailySongs:[...]}}`; App-style fields
  `ar`/`al`/`dt` are recognised via `#[serde(alias)]` on `RawTrack`. Sidecar path is
  `/recommend/songs` (not `/v1/discovery/...`) and needs `timestamp=` to break cache.
- **Deliberately not indexed**: daily changes every day; making it a playlist would
  pile one-off tracks into the index. `--fetch N` only writes files to
  `网易云/每日推荐/`. "Already landed" = check the index, then the directory's stems.
- Tell users the cost honestly: these files don't show in `musicm info` stats, and
  `play <id>` won't recognise the landed copy (it fetches a new one into
  `网易云/单曲/`). FUSE can see them from disk, but they belong to no playlist.

## Search / account playlists / artists

- API details and three response-shape traps live in the `netease-music-api` skill
  §11 — read it before touching these commands.
- The `--type` → numeric type mapping (1 / 1000 / 100) **fails silently, you just
  never find anything**, so the `search_type_maps_to_the_api_kinds` test is a rivet.
  Don't delete it.
- `/user/playlist`'s `playlist` is **top-level** (not under `result`); `more` means
  another page; anonymous sees only public playlists. `artist/top/song`'s `song` is
  top-level and **`limit` does not work** (fixed 50) — truncate yourself.
- Keywords must be percent-encoded (`encode_query`, hand-rolled, no dep): Chinese,
  spaces, `&`, `#` all occur and raw concatenation shreds the query string.
- The three result kinds lead to different next actions (songs can land /
  playlists need scan / artists need another query), so print them in three blocks
  with the next command at the end of each; mark 已索引 / 已落地 from the index.

## "Empty results must be explainable" — one consistent rule

List-shaped APIs keep tripping the same trap, so the rule is fixed: **empty array
vs missing field must be distinguished.**

- `DailyShape`: field present but empty = not logged in; field absent = API changed.
- `ListShape` (search / account playlists): **on no match the array field vanishes
  entirely**, leaving `{"result":{"playlistCount":0}}`. The rule is "array absent
  **and** count field absent" = changed; read the count with `flex::as_opt_u64`
  (`#[serde(default)]` would collapse 0 and absent, destroying the evidence). The
  self-reported total (`songCount=336`) is usually far larger than the page size —
  display uses it.
- Copy this pattern for new list APIs; never just print "0 条".

Supporting `flex` helpers: `as_opt_u64` (0 vs absent), `as_opt_bool` (`null` =
unknown, distinct from `false` — `/user/playlist`'s `subscribed` is `null` when
anonymous), `MaybeList::is_present/into_vec`.

## One rule for landing paths, many entry points

`search` / `artist` / `daily` / `play` all produce tracks that belong to no
playlist; they share `fetch_many(tracks, group, count, label)`: directory from
`naming::group_rel` (`None` → `<音源>/单曲`; daily passes `Some("每日推荐")`), stem
from `naming::unique_stems`. **New entry points must reuse it** — hand-built paths
store the same song in two directories. It also checks `index.file_of` first to
skip already-landed tracks.

Search results **must not be landed directly**: the search API's `album` has only
`picId`, no `picUrl` (verified), so call `songs_detail(&ids)` first or the cover is
lost — and fnOS scraping depends on it.

## CLI surface

`scan <歌单id>` · `playlists [--remote [--uid N] [--limit N]]` ·
`tracks <歌单id> [--limit]` · `daily [--limit N] [--fetch N]` ·
`search <关键词> [--type all|song|playlist|artist] [--limit N] [--offset N] [--fetch N]` ·
`artist <歌手id> [--limit N] [--fetch N]` · `url <曲目id>` · `play <曲目id> [--force]` ·
`mount <挂载点> [--fuse-mode ondemand|cached] [--allow-other] [--threads N]` ·
`login [--qr] [--cookie <串> | --cookie-file <路径>]` · `logout` · `whoami` · `info` ·
`serve [--listen ADDR] [--token T] [--no-auth]`
Global flags `--data-dir / --out / --quality`; **given ones are written to config
and reused**.

- `playlists` reads the index by default; `--remote` lists the account's playlists
  (self by default, needs login; `--uid` allows anonymous lookup of someone else's
  public playlists, but say that private ones are invisible).
- `play <id>` **accepts bare numeric ids not in the index** (fetches metadata via
  `songs_detail` on the spot), which is what makes ids from `search`/`artist` useful.
- `whoami` really hits the account API (distinguishes "no login" from "login
  expired"), so it fails offline — deliberately; it's a probe.

## Milestone status

- ✅ M0 source access (direct anonymous API)
- ✅ M1 playlist scan + index persistence
- ✅ M2 single-track landing (resolve → download → tag → lyrics/cover)
- ✅ M4a FUSE export (read-only, on-demand, tree verified with real data)
- ✅ M5a credentials: QR + credential file + Direct/Sidecar modes
- ✅ M7 Web admin UI (`serve`: search / login / scan / download / mount)
- ⬜ M3 cache quota & prefetch queue (ondemand FUXE experience depends on it)
- ⬜ M4b WebDAV export (fnOS remote mount, no root needed — higher priority)
- ⬜ M5b real lossless/master landing (login works; VIP resolve rate unverified)
- ⬜ M6 QQ Music source + cross-source dedup
