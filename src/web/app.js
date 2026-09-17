/* musicm 管理界面 —— 原生 JS，无框架、无外部依赖。 */
(() => {
  'use strict';

  const TOKEN_KEY = 'musicm.token';

  const state = {
    token: null,
    status: null,
    view: 'dashboard',
    jobs: [],
    search: null,
    busy: null,          // 正在搜索 / 加载中的提示
    library: { playlists: [], open: null, tracks: null, mine: null },
    daily: null,
  };

  const $ = (s) => document.querySelector(s);
  const $$ = (s) => Array.from(document.querySelectorAll(s));

  const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

  function fmtBytes(n) {
    if (!n) return '0';
    const mb = n / 1024 / 1024;
    if (mb >= 1024) return (mb / 1024).toFixed(2) + ' GB';
    if (mb >= 1) return mb.toFixed(1) + ' MB';
    return Math.max(1, Math.round(n / 1024)) + ' KB';
  }

  function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts * 1000);
    const p = (x) => String(x).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  }

  let toastTimer = null;
  function toast(msg, isErr) {
    const el = $('#toast');
    el.textContent = msg;
    el.className = 'toast' + (isErr ? ' err' : '');
    el.classList.remove('hidden');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.add('hidden'), isErr ? 6000 : 3000);
  }

  function banner(msg, kind) {
    const el = $('#banner');
    if (!msg) { el.classList.add('hidden'); return; }
    el.textContent = msg;
    el.className = 'banner ' + (kind || '');
    el.classList.remove('hidden');
  }

  // ---------- 网络 ----------

  async function request(method, path, payload) {
    const headers = {};
    if (state.token) headers['X-Musicm-Token'] = state.token;
    if (payload !== undefined) headers['Content-Type'] = 'application/json';

    let r;
    try {
      r = await fetch(path, {
        method,
        headers,
        body: payload === undefined ? undefined : JSON.stringify(payload),
      });
    } catch (e) {
      throw new Error('连不上 musicm：' + e.message);
    }

    if (r.status === 401) {
      showGate();
      throw new Error('访问令牌不对');
    }
    const text = await r.text();
    let data = null;
    if (text) {
      try { data = JSON.parse(text); } catch (_) { throw new Error('响应不是合法 JSON'); }
    }
    if (!r.ok) throw new Error((data && data.error) || ('HTTP ' + r.status));
    return data;
  }

  const get = (p) => request('GET', p);
  const post = (p, b) => request('POST', p, b || {});

  function showGate() {
    $('#app').classList.add('hidden');
    $('#gate').classList.remove('hidden');
    $('#gate-token').focus();
  }

  // ---------- 渲染：概览 ----------

  function renderDashboard() {
    const s = state.status;
    if (!s) return;
    const ix = s.index;

    $('#dash-cards').innerHTML = [
      ['歌单', ix.playlists, ''],
      ['曲目', ix.tracks, `${ix.vip_only} 首需会员`],
      ['已落地', ix.files, fmtBytes(ix.cached_bytes)],
      ['音质', s.quality, '请求档位'],
    ].map(([k, v, sub]) => `<div class="card"><div class="k">${esc(k)}</div><div class="v">${esc(v)}</div>${sub ? `<div class="s">${esc(sub)}</div>` : ''}</div>`).join('');

    $('#dash-paths').innerHTML = [
      ['数据目录', s.data_dir],
      ['音乐目录', s.out_root + (s.out_root_exists ? '' : '（还不存在，下载时会自动建）')],
      ['配置文件', s.config_path],
      ['索引文件', s.index_path],
      ['音源', s.api],
      ['凭据来源', s.login.origin],
      ['平台', s.platform + ' · musicm ' + s.version],
    ].map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('');

    $('#dash-mount').innerHTML = '<h2>挂载</h2>' + mountBody(s.mount);

    const dot = $('#live-dot');
    dot.className = 'dot' + (s.login.has_login ? ' on' : '');
    $('#login-chip').textContent = s.login.has_login ? '已登录' : (s.login.has_cookie ? '有凭据但未登录' : '未登录');
    $('#login-chip').className = 'chip' + (s.login.has_login ? ' on' : '');
    $('#platform-chip').textContent = s.platform + (s.fuse_supported ? ' · 可挂载' : ' · 不可挂载');

    banner(s.fuse_supported ? '' : '当前平台不是 Linux，挂载功能不可用；其余功能（搜索、扫描、下载、凭据）照常可用。', 'warn');
  }

  // ---------- 渲染：搜索 ----------

  function renderSearch() {
    const box = $('#search-result');
    if (state.busy) { box.innerHTML = `<div class="empty-note">${esc(state.busy)}</div>`; return; }
    const r = state.search;
    if (!r) { box.innerHTML = '<div class="empty-note">输入关键词，搜到单曲可以直接下载，搜到歌单可以扫描进库。</div>'; return; }
    if (!r.groups.length) { box.innerHTML = '<div class="empty-note">没有结果。</div>'; return; }

    box.innerHTML = r.groups.map((g) => {
      const head = `<div class="group-title">${esc(g.label)}<span class="dim"> · 接口报 ${g.total} 条</span></div>`;
      if (g.empty) return head + `<div class="empty-note">${esc(g.empty)}</div>`;
      if (!g.items.length) return head + '<div class="empty-note">本次没有取到条目。</div>';
      return head + '<table>' + rowsFor(g) + '</table>';
    }).join('');
  }

  function rowsFor(g) {
    if (g.kind === 'song') {
      return `<thead><tr><th>歌名</th><th>歌手</th><th>专辑</th><th class="num">时长</th><th>状态</th><th></th></tr></thead><tbody>` +
        g.items.map((t) => `<tr>
          <td>${esc(t.name)}</td>
          <td class="dim">${esc(t.artist)}</td>
          <td class="dim">${esc(t.album)}</td>
          <td class="num dim">${esc(t.duration)}</td>
          <td>${t.cached ? '<span class="tag ok">已下载</span>' : (t.vip ? '<span class="tag warn">需会员</span>' : '<span class="tag">未下载</span>')}</td>
          <td class="nowrap"><button class="tiny" data-act="play" data-id="${t.id}">下载</button></td>
        </tr>`).join('') + '</tbody>';
    }
    if (g.kind === 'playlist') {
      return `<thead><tr><th>歌单</th><th>创建者</th><th class="num">曲目</th><th></th></tr></thead><tbody>` +
        g.items.map((p) => `<tr>
          <td>${esc(p.name)}</td>
          <td class="dim">${esc(p.creator)}</td>
          <td class="num dim">${p.count}</td>
          <td class="nowrap">${p.indexed ? '<span class="tag ok">已入库</span>' : `<button class="tiny" data-act="scan" data-id="${p.id}">扫描</button>`}</td>
        </tr>`).join('') + '</tbody>';
    }
    return `<thead><tr><th>歌手</th><th>别名</th><th class="num">曲目</th><th></th></tr></thead><tbody>` +
      g.items.map((a) => `<tr>
        <td>${esc(a.name)}</td>
        <td class="dim">${esc(a.alias)}</td>
        <td class="num dim">${a.count}</td>
        <td class="nowrap"><button class="tiny" data-act="artist" data-id="${a.id}">热门歌曲</button></td>
      </tr>`).join('') + '</tbody>';
  }

  // ---------- 渲染：音乐库 ----------

  function renderLibrary() {
    const box = $('#library-result');
    const lib = state.library;
    let html = '';

    if (lib.mine) {
      html += `<div class="group-title">我的歌单<span class="dim"> · uid ${lib.mine.uid}</span></div>`;
      html += lib.mine.empty ? `<div class="empty-note">${esc(lib.mine.empty)}</div>`
        : '<table><thead><tr><th>歌单</th><th class="num">曲目</th><th></th></tr></thead><tbody>' +
          lib.mine.playlists.map((p) => `<tr>
            <td>${esc(p.name)}${p.favorite ? ' <span class="tag">我喜欢的</span>' : ''}${p.private ? ' <span class="tag warn">私密</span>' : ''}</td>
            <td class="num dim">${p.count}</td>
            <td class="nowrap">${p.indexed ? '<span class="tag ok">已入库</span>' : `<button class="tiny" data-act="scan" data-id="${p.id}">扫描</button>`}</td>
          </tr>`).join('') + '</tbody></table>';
    }

    html += '<div class="group-title" style="margin-top:14px">已入库歌单</div>';
    if (!lib.playlists.length) {
      html += '<div class="empty-note">索引是空的。在上面填歌单 id 扫描一张，或者在「搜索」里搜歌单。</div>';
    } else {
      html += '<table><thead><tr><th>歌单</th><th>创建者</th><th class="num">曲目</th><th class="num">已落地</th><th></th></tr></thead><tbody>' +
        lib.playlists.map((p) => `<tr class="clickable" data-act="open" data-id="${p.id}">
          <td>${esc(p.name)}</td>
          <td class="dim">${esc(p.creator)}</td>
          <td class="num dim">${p.tracks}</td>
          <td class="num dim">${p.landed}</td>
          <td class="nowrap">${lib.open === String(p.id) ? '<span class="tag">展开中</span>' : '<span class="tag">查看</span>'}</td>
        </tr>`).join('') + '</tbody></table>';
    }

    if (lib.tracks) {
      const t = lib.tracks;
      html += `<div class="group-title" style="margin-top:14px">《${esc(t.playlist.name)}》<span class="dim"> · ${t.shown}/${t.total} 首</span></div>`;
      html += '<table><thead><tr><th class="num">#</th><th>歌名</th><th>歌手</th><th>专辑</th><th class="num">时长</th><th>状态</th><th></th></tr></thead><tbody>' +
        t.tracks.map((x, i) => `<tr>
          <td class="num dim">${i + 1}</td>
          <td>${esc(x.name)}</td>
          <td class="dim">${esc(x.artist)}</td>
          <td class="dim">${esc(x.album)}</td>
          <td class="num dim">${esc(x.duration)}</td>
          <td>${x.cached ? `<span class="tag ok">已下载</span>` : (x.vip ? '<span class="tag warn">需会员</span>' : '<span class="tag">未下载</span>')}</td>
          <td class="nowrap"><button class="tiny" data-act="play" data-id="${x.id}">${x.cached ? '重新下载' : '下载'}</button></td>
        </tr>`).join('') + '</tbody></table>';
    }

    box.innerHTML = html;
  }

  // ---------- 渲染：每日推荐 ----------

  function renderDaily() {
    const box = $('#daily-result');
    if (state.busy) { box.innerHTML = `<div class="empty-note">${esc(state.busy)}</div>`; return; }
    const d = state.daily;
    if (!d) { box.innerHTML = '<div class="empty-note">点「获取今日推荐」。</div>'; return; }
    if (d.empty) { box.innerHTML = `<div class="empty-note">${esc(d.empty)}</div>`; return; }

    $('#daily-note').textContent = `落地目录：${d.dir}（这些曲目不进索引）`;
    box.innerHTML = '<table><thead><tr><th></th><th class="num">#</th><th>歌名</th><th>歌手</th><th>专辑</th><th class="num">时长</th><th>状态</th></tr></thead><tbody>' +
      d.tracks.map((t, i) => `<tr>
        <td><input type="checkbox" data-daily="${t.id}" ${t.cached ? '' : 'checked'}></td>
        <td class="num dim">${i + 1}</td>
        <td>${esc(t.name)}</td>
        <td class="dim">${esc(t.artist)}</td>
        <td class="dim">${esc(t.album)}</td>
        <td class="num dim">${esc(t.duration)}</td>
        <td>${t.cached ? '<span class="tag ok">已下载</span>' : (t.vip ? '<span class="tag warn">需会员</span>' : '<span class="tag">未下载</span>')}</td>
      </tr>`).join('') + '</tbody></table>';
    $('#daily-download').disabled = false;
  }

  // ---------- 渲染：账号 ----------

  function renderAccount() {
    const s = state.status;
    if (!s) return;
    const l = s.login;
    const tag = l.has_login
      ? '<span class="tag ok">已登录</span>'
      : (l.has_cookie ? '<span class="tag warn">有 cookie 但没有登录态</span>' : '<span class="tag">未登录</span>');
    $('#account-state').innerHTML =
      '<h2>当前状态</h2>' +
      `<div>${tag}</div>` +
      `<dl class="kv" style="margin-top:8px">
        <dt>凭据来源</dt><dd>${esc(l.origin)}</dd>
        <dt>凭据内容</dt><dd>${esc(l.masked)}</dd>
      </dl>`;
  }

  // ---------- 渲染：挂载 ----------

  function mountBody(m) {
    if (!m.supported) {
      return '<p class="dim">FUSE 只在 Linux 上可用（飞牛 fnOS 就是）。当前平台上挂载不可用，其余功能照常能用。</p>';
    }
    if (!m.mountpoint) return '<p class="dim">还没有挂载过。</p>';
    const tag = m.mounted ? '<span class="tag ok">已挂载</span>'
      : (m.running ? '<span class="tag warn">启动中</span>' : '<span class="tag">未挂载</span>');
    return `<div>${tag} <code>${esc(m.mountpoint)}</code> · 模式 ${esc(m.mode || '—')}</div>` +
      (m.fstype ? `<div class="dim">内核里是 <code>${esc(m.fstype)}</code></div>` : '') +
      (m.message ? `<div class="dim">${esc(m.message)}</div>` : '') +
      (m.started_at ? `<div class="dim">启动于 ${fmtTime(m.started_at)}</div>` : '');
  }

  function renderMount() {
    const s = state.status;
    if (!s) return;
    $('#mount-state').innerHTML = '<h2>当前状态</h2>' + mountBody(s.mount);
    $('#mount-go').disabled = !s.mount.supported || s.mount.mounted || s.mount.running;
    $('#unmount-go').disabled = !s.mount.supported || !s.mount.mounted;
    if (!$('#mount-point').value && s.out_root) {
      // 给个常见的飞牛挂载点作为默认值，用户改一下就行
      $('#mount-point').placeholder = '/vol1/1000/music';
    }
  }

  // ---------- 渲染：任务 ----------

  function renderJobs() {
    const list = $('#jobs-list');
    if (!state.jobs.length) { list.innerHTML = '<div class="dim">还没有任务。</div>'; return; }
    list.innerHTML = state.jobs.slice().reverse().map((j) => {
      const tag = j.running ? '<span class="tag warn">进行中</span>'
        : (j.error ? '<span class="tag err">失败</span>' : '<span class="tag ok">完成</span>');
      return `<div class="job" data-job="${j.id}">
        <div class="head"><span>${esc(j.title)}</span><span>${tag}</span></div>
        <div class="dim" style="font-size:12px">#${j.id} · ${fmtTime(j.started_at)}${j.finished_at ? ' → ' + fmtTime(j.finished_at) : ''}</div>
        ${j.error ? `<div class="tag err">${esc(j.error)}</div>` : ''}
        <div class="log" data-log="${j.id}"></div>
      </div>`;
    }).join('');
    // 日志单独拉，避免列表接口把日志塞满
    state.jobs.filter((j) => true).forEach(async (j) => {
      try {
        const d = await get('/api/jobs/' + j.id);
        const el = list.querySelector(`[data-log="${j.id}"]`);
        if (el) el.textContent = (d.log || []).join('\n');
      } catch (_) { /* 列表刷新竞态，忽略 */ }
    });
  }

  // ---------- 动作 ----------

  async function doSearch() {
    const q = $('#search-input').value.trim();
    if (!q) { toast('先输入关键词'); return; }
    const type = $('#search-type').value;
    state.busy = '搜索中…';
    renderSearch();
    try {
      state.search = await get(`/api/search?q=${encodeURIComponent(q)}&type=${encodeURIComponent(type)}&limit=30`);
    } catch (e) {
      state.search = null;
      toast(e.message, true);
    }
    state.busy = null;
    renderSearch();
  }

  async function doScan(id) {
    try {
      const r = await post('/api/scan', { playlist_id: Number(id) });
      toast('已开始扫描，见右上角「任务」');
      openJobs();
      watchJob(r.job, () => { refreshLibrary(); });
    } catch (e) { toast(e.message, true); }
  }

  async function doPlay(ids) {
    try {
      const r = await post('/api/play', { ids: ids });
      toast(`已开始下载 ${ids.length} 首，见右上角「任务」`);
      openJobs();
      watchJob(r.job, () => { refreshStatus(); refreshLibraryTracks(); });
    } catch (e) { toast(e.message, true); }
  }

  async function doArtist(id) {
    state.busy = '取热门歌曲…';
    try {
      const r = await get(`/api/artist/${encodeURIComponent(id)}/songs?limit=50`);
      state.search = {
        query: '',
        groups: [{ kind: 'song', label: '热门歌曲', total: r.total, items: r.tracks, empty: r.tracks.length ? null : '这位歌手没有返回热门歌曲。' }],
      };
      switchView('search');
    } catch (e) { toast(e.message, true); }
    state.busy = null;
    renderSearch();
  }

  async function doDaily() {
    state.busy = '取每日推荐…';
    state.daily = null;
    renderDaily();
    try {
      state.daily = await get('/api/daily?limit=30');
    } catch (e) { state.daily = null; toast(e.message, true); }
    state.busy = null;
    renderDaily();
  }

  async function doDailyDownload() {
    const ids = $$('[data-daily]').filter((c) => c.checked).map((c) => Number(c.dataset.daily));
    if (!ids.length) { toast('先勾选要下载的曲目'); return; }
    try {
      const r = await post('/api/fetch', { ids, group: '每日推荐' });
      toast(`已开始下载 ${ids.length} 首，见右上角「任务」`);
      openJobs();
      watchJob(r.job, () => doDaily());
    } catch (e) { toast(e.message, true); }
  }

  async function doLogin() {
    const cookie = $('#cookie-input').value.trim();
    if (!cookie) { toast('先粘贴 cookie'); return; }
    try {
      const r = await post('/api/login', { cookie });
      toast(r.note || '已保存');
      if (r.dropped && r.dropped.length) toast('重名 cookie 只保留了第一条：' + r.dropped.join(', '));
      $('#cookie-input').value = '';
      await refreshStatus();
    } catch (e) { toast(e.message, true); }
  }

  async function doWhoami() {
    try {
      const r = await post('/api/whoami');
      toast(r.note || '');
      await refreshStatus();
    } catch (e) { toast(e.message, true); }
  }

  async function doLogout() {
    try {
      const r = await post('/api/logout');
      toast(r.note || '已清除');
      await refreshStatus();
    } catch (e) { toast(e.message, true); }
  }

  async function doMount() {
    const mountpoint = $('#mount-point').value.trim();
    if (!mountpoint) { toast('先填挂载点'); return; }
    try {
      const r = await post('/api/mount', {
        mountpoint,
        mode: $('#mount-mode').value,
        allow_other: $('#mount-allow-other').checked,
        threads: Number($('#mount-threads').value) || 4,
      });
      toast(r.note || '挂载线程已启动');
      await refreshStatus();
    } catch (e) { toast(e.message, true); }
  }

  async function doUnmount() {
    try {
      const r = await post('/api/unmount', {});
      toast('已卸载 ' + (r.unmounted || ''));
      await refreshStatus();
    } catch (e) { toast(e.message, true); }
  }

  async function doMine() {
    state.busy = '取我的歌单…';
    try {
      state.library.mine = await get('/api/my-playlists?limit=50');
    } catch (e) { state.library.mine = null; toast(e.message, true); }
    state.busy = null;
    renderLibrary();
  }

  async function doOpen(id) {
    const key = String(id);
    if (state.library.open === key) { state.library.open = null; state.library.tracks = null; renderLibrary(); return; }
    try {
      state.library.tracks = await get(`/api/playlists/${encodeURIComponent(id)}/tracks?limit=2000`);
      state.library.open = key;
    } catch (e) { toast(e.message, true); }
    renderLibrary();
  }

  /// 轮询某个任务直到结束，然后跑一次回调（比如刷新列表）
  async function watchJob(id, done) {
    for (let i = 0; i < 300; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      let d;
      try { d = await get('/api/jobs/' + id); } catch (_) { return; }
      if (!d.running) {
        if (d.error) toast(d.error, true);
        if (done) await done();
        return;
      }
    }
  }

  // ---------- 数据刷新 ----------

  async function refreshStatus() {
    try { state.status = await get('/api/status'); } catch (_) { return; }
    renderDashboard();
    renderAccount();
    renderMount();
  }

  async function refreshLibrary() {
    try {
      const r = await get('/api/playlists');
      state.library.playlists = r.playlists;
    } catch (_) {}
    renderLibrary();
  }

  async function refreshLibraryTracks() {
    await refreshLibrary();
    if (state.library.open) {
      try {
        state.library.tracks = await get(`/api/playlists/${state.library.open}/tracks?limit=2000`);
      } catch (_) {}
      renderLibrary();
    }
  }

  async function refreshJobs() {
    let r;
    try { r = await get('/api/jobs'); } catch (_) { return; }
    const before = state.jobs.filter((j) => j.running).length;
    state.jobs = r.jobs;
    const running = r.jobs.filter((j) => j.running).length;
    $('#jobs-badge').textContent = running;
    if (!$('#jobs-drawer').classList.contains('hidden')) renderJobs();
    if (before > 0 && running === 0) {
      await refreshStatus();
      await refreshLibraryTracks();
    }
  }

  function refreshCurrentView() {
    const v = state.view;
    if (v === 'library') refreshLibrary();
    else if (v === 'daily' && state.daily) doDaily();
  }

  // ---------- 视图切换 ----------

  function switchView(v) {
    state.view = v;
    $$('.nav').forEach((b) => b.classList.toggle('active', b.dataset.view === v));
    $$('.view').forEach((s) => s.classList.toggle('active', s.dataset.view === v));
    refreshCurrentView();
  }

  function openJobs() {
    $('#jobs-drawer').classList.remove('hidden');
    renderJobs();
  }

  // ---------- 启动 ----------

  function wire() {
    $$('.nav').forEach((b) => b.addEventListener('click', () => switchView(b.dataset.view)));

    $('#search-go').addEventListener('click', doSearch);
    $('#search-input').addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch(); });

    $('#scan-go').addEventListener('click', () => {
      const id = $('#scan-input').value.trim();
      if (!id) { toast('先填歌单 id'); return; }
      doScan(id);
    });
    $('#mine-go').addEventListener('click', doMine);

    $('#daily-go').addEventListener('click', doDaily);
    $('#daily-download').addEventListener('click', doDailyDownload);

    $('#login-go').addEventListener('click', doLogin);
    $('#whoami-go').addEventListener('click', doWhoami);
    $('#logout-go').addEventListener('click', doLogout);

    $('#mount-go').addEventListener('click', doMount);
    $('#unmount-go').addEventListener('click', doUnmount);

    $('#jobs-toggle').addEventListener('click', () => {
      const d = $('#jobs-drawer');
      if (d.classList.contains('hidden')) openJobs(); else d.classList.add('hidden');
    });
    $('#jobs-close').addEventListener('click', () => $('#jobs-drawer').classList.add('hidden'));

    // 表格里的按钮统一走事件委托：列表经常重绘，逐个绑会漏
    document.addEventListener('click', (e) => {
      const el = e.target.closest('[data-act]');
      if (!el) return;
      const id = el.dataset.id;
      switch (el.dataset.act) {
        case 'play': doPlay([String(id)]); break;
        case 'scan': doScan(id); break;
        case 'artist': doArtist(id); break;
        case 'open': doOpen(id); break;
      }
    });

    $('#gate-ok').addEventListener('click', () => {
      const t = $('#gate-token').value.trim();
      if (!t) { $('#gate-error').textContent = '令牌不能为空'; return; }
      state.token = t;
      localStorage.setItem(TOKEN_KEY, t);
      $('#gate-error').textContent = '';
      enter();
    });
  }

  async function enter() {
    try {
      state.status = await get('/api/status');
    } catch (e) {
      // 401 已经把门禁弹出来了，这里不用再说一遍
      if (!/令牌/.test(e.message)) toast(e.message, true);
      return;
    }
    $('#gate').classList.add('hidden');
    $('#app').classList.remove('hidden');
    renderDashboard();
    renderAccount();
    renderMount();
    renderSearch();
    renderLibrary();
    renderDaily();
    refreshLibrary();
    refreshJobs();
    setInterval(refreshStatus, 5000);
    setInterval(refreshJobs, 1000);
  }

  function boot() {
    const params = new URLSearchParams(location.search);
    const inUrl = params.get('token');
    if (inUrl) {
      // 地址栏里的令牌存起来就抹掉，免得被随手复制出去
      state.token = inUrl;
      try { localStorage.setItem(TOKEN_KEY, inUrl); } catch (_) {}
      history.replaceState({}, '', location.pathname);
    } else {
      try { state.token = localStorage.getItem(TOKEN_KEY); } catch (_) { state.token = null; }
    }
    wire();
    enter();
  }

  boot();
})();
