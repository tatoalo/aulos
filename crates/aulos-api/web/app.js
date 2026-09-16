/* Aulos web UI — PROTOCOL v2 client.
 *
 * No framework, no bundler, no dependency. The page runs under
 *   default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'
 * so there is no inline script, no inline <style> and no `setAttribute('style', …)` anywhere:
 * every visual state is a class and every measured value goes through the CSSOM.
 *
 * Layout of this file:
 *   1  environment (prefix, theme)      6  websocket + frame application (§5, §6, §7)
 *   2  icons                            7  rendering (reconcile by id, one rAF per tick)
 *   3  formatting                       8  row actions (§4.2)
 *   4  state                            9  the add flow (§4.1, §4.5, §4.6)
 *   5  REST (§1.4 auth, §1.5 errors)   10  subscriptions (§4.7, §5.9, §9)
 *                                      11  chrome: theme, toasts, menus, sheets, boot
 */

/* ------------------------------------------------------------------ 1. env */

const $ = (id) => document.getElementById(id);
const meta = (name) => document.querySelector(`meta[name="${name}"]`);

/** `URL_PREFIX`, always starting and ending with `/` (PROTOCOL §1.1). */
const PREFIX = (() => {
  let p = meta('aulos-prefix')?.content || '/';
  if (p[0] === '{') p = '/';   // the template was opened unsubstituted
  if (!p.startsWith('/')) p = '/' + p;
  if (!p.endsWith('/')) p += '/';
  return p;
})();

const DEFAULT_THEME = (() => {
  const t = meta('aulos-theme')?.content || 'auto';
  return ['auto', 'light', 'dark'].includes(t) ? t : 'auto';
})();

const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
  del(k) { try { localStorage.removeItem(k); } catch { /* private mode */ } },
};

const PHONE = window.matchMedia('(max-width: 640px)');
const isPhone = () => PHONE.matches;

/** PROTOCOL §2.3: absolute (has a scheme) → open as-is, otherwise resolve against origin + prefix. */
const ABSOLUTE = /^[a-z][a-z0-9+.-]*:/i;
function resolveUrl(u) {
  if (!u) return null;
  return ABSOLUTE.test(u) ? u : new URL(PREFIX + u.replace(/^\//, ''), location.origin).href;
}

/** `window.open`, but only on http(s): an imported `url` can carry any scheme. */
function openExternal(u) {
  let p;
  try { p = new URL(u, location.href); } catch { /* not a URL */ }
  if (p && (p.protocol === 'http:' || p.protocol === 'https:')) window.open(p.href, '_blank', 'noopener');
  else toast('error', 'That link is not a web address.');
}

/* --------------------------------------------------------------- 2. icons */

const P = {
  link: '<path d="M10 13a5 5 0 0 0 7.1 0l3-3a5 5 0 0 0-7.1-7.1l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.1 0l-3 3a5 5 0 0 0 7.1 7.1l1.7-1.7"/>',
  chevron: '<path d="M6 9l6 6 6-6"/>',
  right: '<path d="M9 6l6 6-6 6"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  auto: '<circle cx="12" cy="12" r="9"/><path d="M12 3v18" /><path d="M12 3a9 9 0 0 1 0 18" fill="currentColor"/>',
  sun: '<circle cx="12" cy="12" r="3.5"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
  moon: '<path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5z"/>',
  download: '<path d="M12 4v12"/><path d="M6 10l6 6 6-6"/><path d="M5 20h14"/>',
  list: '<path d="M4 6h16M4 12h16M4 18h10"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9L7 7M17 17l2.1 2.1M4.9 19.1L7 17M17 7l2.1-2.1"/>',
  clock: '<circle cx="12" cy="12" r="8"/><path d="M12 8v4l3 2"/>',
  check: '<path d="M5 12l5 5L20 7"/>',
  cross: '<path d="M6 6l12 12M18 6L6 18"/>',
  pause: '<rect x="6" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none"/><rect x="14" y="5" width="4" height="14" rx="1" fill="currentColor" stroke="none"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor" stroke="none"/>',
  play: '<path d="M7 5v14l12-7z" fill="currentColor" stroke="none"/>',
  screen: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M10 9l5 3-5 3z" fill="currentColor"/>',
  dots: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
  retry: '<path d="M20 12a8 8 0 1 1-2.3-5.7"/><path d="M20 4v5h-5"/>',
  external: '<path d="M7 17L17 7M9 7h8v8"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h8"/>',
  rss: '<circle cx="5.5" cy="18.5" r="1.7" fill="currentColor" stroke="none"/><path d="M4 11a9 9 0 0 1 9 9"/><path d="M4 4a16 16 0 0 1 16 16"/>',
  edit: '<path d="M11 4H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h13a2 2 0 0 0 2-2v-6"/><path d="M18.5 2.5a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4z"/>',
};

function icon(name, size = 16, stroke = 2) {
  return `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${P[name] || ''}</svg>`;
}

/* ---------------------------------------------------------- 3. formatting */

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];
function bytes(n) {
  if (n === null || n === undefined || !isFinite(n)) return '';
  let v = Math.max(0, n), i = 0;
  while (v >= 1024 && i < UNITS.length - 1) { v /= 1024; i++; }
  return `${i === 0 ? Math.round(v) : v.toFixed(1)} ${UNITS[i]}`;
}

function speed(bps) {
  if (!bps || !isFinite(bps)) return '';
  return `${(bps / (1024 * 1024)).toFixed(1)} MB/s`;
}

function dur(secs) {
  if (secs === null || secs === undefined || !isFinite(secs) || secs < 0) return '';
  const s = Math.round(secs), h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
  const pad = (n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(r)}` : `${m}:${pad(r)}`;
}

function rel(ms) {
  if (!ms) return '';
  const d = Math.round((Date.now() - ms) / 1000);
  if (d < 60) return 'just now';
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) { const h = Math.floor(d / 3600); return `${h} hour${h === 1 ? '' : 's'} ago`; }
  if (d < 172800) return 'yesterday';
  return `${Math.floor(d / 86400)} days ago`;
}

/** `rel`'s forward-facing twin, for a subscription's `next_due` (§9). */
function until(ms) {
  if (!ms) return '';
  const d = Math.round((ms - Date.now()) / 1000);
  if (d <= 0) return 'due now';
  if (d < 60) return 'due in under a minute';
  if (d < 3600) return `due in ${Math.floor(d / 60)} min`;
  if (d < 86400) { const h = Math.round(d / 3600); return `due in ${h} hour${h === 1 ? '' : 's'}`; }
  return `due in ${Math.round(d / 86400)} days`;
}

const cap1 = (s) => (s ? s[0].toUpperCase() + s.slice(1) : '');

/* ------------------------------------------------------------- 4. state */

const state = {
  items: new Map(),
  subs: new Map(),
  health: new Map(),
  subsOk: false,
  seq: 0,
  bootId: null,
  caps: null,
  doneTotal: 0,
  cursor: null,
  hasOlder: false,
  conn: 'connecting',
};

const expanded = new Set();     // group ids whose children are shown
const kidLimit = new Map();     // group id → how many children are rendered
const kidsFetched = new Set();  // groups whose children we pulled with GET items?group_id=

let token = store.get('aulos.token', '') || '';

const ACTIVE = new Set(['resolving', 'preparing', 'downloading', 'postprocessing']);
const TERMINAL = new Set(['finished', 'error', 'canceled']);
/** §3.1's closed vocabulary; anything else renders inert. */
const KNOWN = new Set([...ACTIVE, 'queued', ...TERMINAL]);
/** Completed rows kept in the DOM; `Show older` raises it. */
let doneLimit = 200;

/* --------------------------------------------------------------- 5. REST */

class ApiError extends Error {
  constructor(status, wire) {
    super((wire && wire.message) || `HTTP ${status}`);
    this.status = status;
    this.code = (wire && wire.code) || 'internal';
    this.field = (wire && wire.field) || null;
  }
}

async function api(path, opts = {}) {
  const headers = Object.assign({}, opts.headers);
  if (token) headers.Authorization = `Bearer ${token}`;
  if (opts.body !== undefined) headers['Content-Type'] = 'application/json';
  const init = { method: opts.method || 'GET', credentials: 'same-origin', headers };
  if (opts.body !== undefined) init.body = JSON.stringify(opts.body);
  const res = await fetch(PREFIX + path, init);
  if (res.status === 401 || res.status === 403) { needAuth(); throw new ApiError(res.status, { code: 'unauthorized', message: 'authentication required' }); }
  if (res.status === 204) return null;
  let body = null;
  try { body = await res.json(); } catch { /* empty or non-JSON */ }
  if (!res.ok) throw new ApiError(res.status, body && body.error);
  return body;
}

/* ---------------------------------------------------------- 6. websocket */

let ws = null;
let backoff = 500;
let reconnectTimer = null;
let sawFrame = false;
let probedAuth = false;

function wsUrl() {
  const path = state.caps?.protocol?.ws_path || 'ws';
  const u = new URL(PREFIX + path.replace(/^\//, ''), location.href);
  u.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  if (state.seq && state.bootId) { u.searchParams.set('since', String(state.seq)); u.searchParams.set('boot', state.bootId); }
  return u.toString();
}

// §1.4 takes the token as `bearer.<token>` in the subprotocol list, which — unlike
// `?token=` — stays out of the proxy access log.
const wsProtocols = () => (token ? ['aulos.v2', `bearer.${token}`] : ['aulos.v2']);

function connect() {
  clearTimeout(reconnectTimer);
  reconnectTimer = null;
  if (ws) { try { ws.onclose = null; ws.close(); } catch { /* already gone */ } }
  sawFrame = false;
  setConn(state.seq ? 'reconnecting' : 'connecting');
  let sock;
  try { sock = new WebSocket(wsUrl(), wsProtocols()); } catch { scheduleReconnect(); return; }
  ws = sock;
  // No client `ping` timer: §5.11 makes every client → server frame optional, and the server
  // already pings at the WebSocket level every 20 s and closes a socket that stops answering.
  // NOT `backoff = 500` here: an accepted upgrade is no working session — a server closing with
  // 1013 (§5.1: back off) would pin this to a ~500 ms hot loop. A frame is what proves it.
  sock.onopen = () => setConn('live');
  sock.onmessage = (ev) => {
    if (!sawFrame) { sawFrame = true; backoff = 500; probedAuth = false; }
    setConn('live');
    let frame;
    try { frame = JSON.parse(ev.data); } catch { return; }
    applyFrame(frame);
  };
  sock.onerror = () => { /* close follows */ };
  sock.onclose = () => {
    if (ws !== sock) return;
    ws = null;
    scheduleReconnect();
    // An auth-rejected upgrade closes with no frame and hides its code, so ask REST — once per
    // run of failures, not once per close, or a flap becomes a REST flood.
    if (!sawFrame && !probedAuth) {
      probedAuth = true;
      api('api/v2/capabilities').catch(() => { /* needAuth already ran on 401 */ });
    }
  };
}

function scheduleReconnect() {
  if (reconnectTimer) return;
  setConn(navigator.onLine === false ? 'offline' : 'reconnecting');
  const wait = Math.min(10000, backoff) * (0.7 + Math.random() * 0.6);
  backoff = Math.min(10000, backoff * 2);
  reconnectTimer = setTimeout(() => { reconnectTimer = null; connect(); }, wait);
}

function applyHealth(component, status, detail) {
  const previous = state.health.get(component);
  state.health.set(component, status);
  if (previous !== status && (status === 'degraded' || status === 'down')) {
    toast('warning', `${component} is ${status}${detail ? ` — ${detail}` : ''}`);
  }
}

/** PROTOCOL §7 — the complete apply algorithm. */
function applyFrame(f) {
  switch (f.t) {
    case 'snapshot': {
      state.items = new Map();
      for (const it of (f.items || []).concat(f.done || [])) state.items.set(it.id, it);
      state.subs = new Map((f.subscriptions || []).map((s) => [s.id, s]));
      for (const id of [...subEditing]) if (!state.subs.has(id)) subEditing.delete(id);
      state.bootId = f.boot_id;
      const health = Object.entries(f.health?.components || {});
      for (const [component, status] of health) applyHealth(component, status);
      state.health = new Map(health);
      state.doneTotal = f.done_total || 0;
      state.hasOlder = !!(f.truncated && f.truncated.done);
      state.cursor = null;
      kidsFetched.clear();
      // §5.11: a fresh snapshot means re-asking a large group for its children. Open groups stay
      // open — collapsing them every reconnect is its own bug — so re-fetch instead.
      for (const gid of [...expanded]) {
        const g = state.items.get(gid);
        if (!g) expanded.delete(gid);
        else if (g.children_inline === false) fetchKids(gid);
      }
      break;
    }
    case 'resume':
      state.seq = f.to || f.seq;
      markDirty();
      return;
    case 'added':
    case 'completed':
      for (const it of f.items || []) state.items.set(it.id, it);
      break;
    case 'removed':
      for (const id of f.ids || []) { state.items.delete(id); expanded.delete(id); kidsFetched.delete(id); }
      break;
    case 'delta':
      for (const patch of f.items || []) {
        const cur = state.items.get(patch.id);
        if (!cur) continue;                         // never create from a delta (§5.4)
        for (const k of Object.keys(patch)) { if (k !== 'id') cur[k] = patch[k]; }
      }
      break;
    // §5.9: one frame type carries a creation, an edit and the result of every check, and it is
    // an upsert on `subscription.id` — never merge it into a delta.
    case 'subscription':
      if (f.subscription && f.subscription.id) state.subs.set(f.subscription.id, f.subscription);
      break;
    case 'subscription_removed':
      for (const id of f.ids || []) { state.subs.delete(id); subEditing.delete(id); }
      break;
    case 'notice':
      toast(f.level || 'info', f.message || '');
      break;
    case 'health':
      for (const c of f.changed || []) {
        applyHealth(c.component, c.to, c.detail);
      }
      break;
    case 'providers':
      // §5.9: capabilities *and* catalog — capabilities alone replaces a URL-refined picker
      // mid-add with the generic ladder and loses the §4.6 notices.
      loadCapabilities().then(() => { if (add.url.trim()) loadCatalog(); }).catch(() => { /* keep the picker */ });
      break;
    case 'error':
      toast('error', f.message || 'Protocol error');
      break;
    default:
      break;                                        // ytdl_options, pong: no UI
  }
  if (typeof f.seq === 'number') state.seq = f.seq;
  markDirty();
}

/* ------------------------------------------------------------ 7. render */

let rafId = 0;
const rows = new Map();     // id → {el, refs, v}

function markDirty() { if (!rafId) rafId = requestAnimationFrame(flush); }

function sorted(list) {
  return list.sort((a, b) => (a.ord - b.ord) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

function flush() {
  rafId = 0;
  const all = sorted([...state.items.values()]);
  const top = all.filter((i) => !i.group_id);

  // §3.1: an unknown status renders inert, not nowhere — `subFor`'s default branch labels it.
  const active = top.filter((i) => ACTIVE.has(i.status) || (i.status === 'queued' && i.auto_start) || !KNOWN.has(i.status));
  const waiting = top.filter((i) => i.status === 'queued' && !i.auto_start);
  // ord-ascending, so the tail is newest: render a window of the completed history and evict the
  // rest — which `Show older` pages back — or the DOM and the maps grow without bound.
  const allDone = top.filter((i) => TERMINAL.has(i.status));
  const done = allDone.slice(Math.max(0, allDone.length - doneLimit));
  for (let i = 0; i < allDone.length - done.length; i++) { state.items.delete(allDone[i].id); state.hasOlder = true; }

  reconcile($('rows-active'), active);
  reconcile($('rows-waiting'), waiting);
  reconcile($('rows-done'), done);

  $('sec-active').hidden = active.length === 0;
  $('sec-waiting').hidden = waiting.length === 0;
  $('sec-done').hidden = done.length === 0;
  $('empty').hidden = all.length > 0;

  // Sum only top-level rows: a group's `speed` is already the sum over its children (§3.3).
  let bps = 0;
  for (const i of top) if (i.speed) bps += i.speed;
  const meta = [`${active.length} item${active.length === 1 ? '' : 's'}`];
  if (bps > 0) meta.push(speed(bps));
  $('active-meta').textContent = meta.join(' · ');

  const nActive = top.filter((i) => ACTIVE.has(i.status)).length;
  const nDone = Math.max(done.length, state.doneTotal);
  const bits = [`${nActive} active`];
  if (waiting.length) bits.push(`${waiting.length} waiting`);
  bits.push(`${nDone} done`);
  $('summary').textContent = bits.join(' · ');

  $('show-older').hidden = !(state.hasOlder || state.cursor);

  for (const id of [...rows.keys()]) {
    if (!state.items.has(id)) { rows.get(id).el.remove(); rows.delete(id); }
  }

  // Alphabetical, with the id as the tie-break: a subscription list has no `ord` and its order
  // must not move under the pointer when a check bumps `last_checked`.
  const subs = [...state.subs.values()]
    .sort((a, b) => (a.name || a.url).localeCompare(b.name || b.url) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  reconcileSubs($('rows-subs'), subs);
  $('sec-subs').hidden = !state.subsOk;
  $('rows-subs').hidden = subs.length === 0;
  $('subs-empty').hidden = subs.length > 0;
  $('subs-check').hidden = subs.length === 0;
}

/** Reconcile by id: a row node is created once and then patched, never rebuilt (§2.4). */
function reconcile(container, list) {
  let node = container.firstElementChild;
  for (const it of list) {
    const r = rowFor(it.id);
    if (node === r.el) node = node.nextElementSibling;
    else container.insertBefore(r.el, node);
    patchRow(r, it);
  }
  while (node) { const next = node.nextElementSibling; node.remove(); node = next; }
}

function rowFor(id) {
  let r = rows.get(id);
  if (r) return r;
  const el = document.createElement('div');
  el.className = 'row';
  el.dataset.id = id;
  el.innerHTML =
    '<div class="row-main">' +
      `<button class="chev" type="button" aria-expanded="false" hidden>${icon('right', 12, 2.5)}</button>` +
      '<div class="disc"></div>' +
      '<div class="row-body"><div class="row-title"></div><div class="row-sub"><span class="st"></span><span class="rest"></span></div></div>' +
      '<div class="row-acts"></div>' +
    '</div>' +
    '<div class="bars" hidden><div class="bar"><div class="fill"></div></div><div class="bar ph" hidden><div class="fill phase"></div></div></div>' +
    '<div class="kids" hidden></div>';
  const refs = {
    chev: el.querySelector('.chev'),
    disc: el.querySelector('.disc'),
    title: el.querySelector('.row-title'),
    st: el.querySelector('.st'),
    rest: el.querySelector('.rest'),
    acts: el.querySelector('.row-acts'),
    bars: el.querySelector('.bars'),
    fill: el.querySelector('.bar .fill'),
    phBar: el.querySelector('.bar.ph'),
    phFill: el.querySelector('.fill.phase'),
    kids: el.querySelector('.kids'),
  };
  refs.chev.addEventListener('click', () => toggleGroup(id));
  r = { el, refs, v: {} };
  rows.set(id, r);
  return r;
}

const DISC = {
  resolving: ['spin', null],
  queued: ['', 'clock'],
  preparing: ['dl', 'download'],
  downloading: ['dl', 'download'],
  postprocessing: ['pp', 'gear'],
  finished: ['ok', 'check'],
  error: ['err', 'cross'],
  canceled: ['', 'cross'],
};

function patchRow(r, it) {
  const { refs, v } = r;
  const group = it.kind === 'group';

  if (v.title !== it.title) { refs.title.textContent = it.title || it.url || ''; v.title = it.title; }

  const sub = subFor(it);
  if (v.word !== sub.word || v.wordCls !== sub.cls) {
    refs.st.textContent = sub.word;
    refs.st.className = `st${sub.cls ? ' ' + sub.cls : ''}`;
    v.word = sub.word; v.wordCls = sub.cls;
  }
  if (v.rest !== sub.rest || v.restErr !== sub.restErr) {
    refs.rest.textContent = sub.rest;
    refs.rest.className = `rest${sub.restErr ? ' err' : ''}`;
    v.rest = sub.rest; v.restErr = sub.restErr;
  }

  const [discCls, discIcon] = DISC[it.status] || ['', 'clock'];
  const wantIcon = group ? 'list' : discIcon;
  const discKey = `${discCls}|${wantIcon}`;
  if (v.disc !== discKey) {
    refs.disc.className = `disc${discCls ? ' ' + discCls : ''}`;
    refs.disc.innerHTML = wantIcon ? icon(wantIcon, 16, wantIcon === 'check' || wantIcon === 'cross' ? 3 : 2.2) : '';
    v.disc = discKey;
  }

  const showBars = !TERMINAL.has(it.status) && it.status !== 'resolving' && (group || it.status !== 'queued' || it.percent > 0);
  if (v.bars !== showBars) { refs.bars.hidden = !showBars; v.bars = showBars; }
  if (showBars) {
    const pct = Math.max(0, Math.min(100, Number(it.percent) || 0));
    const w = `${pct.toFixed(1)}%`;
    if (v.fill !== w) { refs.fill.style.width = w; v.fill = w; }
    const ph = it.status === 'postprocessing' && it.phase_percent !== null && it.phase_percent !== undefined;
    if (v.ph !== ph) { refs.phBar.hidden = !ph; v.ph = ph; }
    if (ph) {
      const pw = `${Math.max(0, Math.min(100, Number(it.phase_percent) || 0)).toFixed(1)}%`;
      if (v.phw !== pw) { refs.phFill.style.width = pw; v.phw = pw; }
    }
  }

  if (v.chev !== group) { refs.chev.hidden = !group; v.chev = group; }
  const open = group && expanded.has(it.id);
  if (v.open !== open) {
    refs.chev.setAttribute('aria-expanded', open ? 'true' : 'false');
    refs.kids.hidden = !open;
    v.open = open;
  }
  if (open) renderKids(refs.kids, it);
  else if (v.kidsSig) { refs.kids.replaceChildren(); v.kidsSig = ''; }

  const acts = actsFor(it);
  const sig = acts.inline.map((a) => a.id + a.icon).join(',') + '|' + (acts.more ? 1 : 0);
  if (v.acts !== sig) {
    refs.acts.replaceChildren();
    acts.inline.forEach((a, i) => refs.acts.appendChild(actButton(it.id, a, i === 0)));
    if (acts.more) {
      const b = actButton(it.id, { id: 'more', label: 'More actions', icon: 'dots' }, false);
      b.classList.add('act-more');
      refs.acts.appendChild(b);
    }
    v.acts = sig;
  }
  // `.more` is what reveals the ⋯ button on desktop; on a phone the CSS always shows it.
  r.el.classList.toggle('more', !!acts.more);
}

function actButton(id, a, primary) {
  const b = document.createElement('button');
  b.type = 'button';
  b.className = `iconbtn${a.cls ? ' ' + a.cls : ''}${primary ? ' act-primary' : ''}`;
  b.setAttribute('aria-label', a.label);
  b.title = a.label;
  b.dataset.action = a.id;
  b.innerHTML = icon(a.icon, 16, a.icon === 'retry' ? 2.2 : 2);
  b.addEventListener('click', (e) => {
    e.stopPropagation();
    if (a.id === 'more') openMenu(b, id);
    else runAction(id, a.id);
  });
  b.addEventListener('contextmenu', (e) => { e.preventDefault(); openMenu(b, id); });
  return b;
}

function selText(it) {
  const s = it.selection;
  if (!s) return '';
  return [qualityLabel(s.format, s.quality), s.format].filter(Boolean).join(' · ');
}

function subFor(it) {
  const S = it.status;
  const none = { word: '', cls: '', rest: '', restErr: false };
  if (it.kind === 'group') {
    const parts = [`${it.children_done || 0} of ${it.children_total || 0} done`];
    if (it.children_active) parts.push(`${it.children_active} active`);
    if (it.children_error) parts.push(`${it.children_error} failed`);
    if (it.provider) parts.push(it.provider);
    const w = S === 'downloading' ? 'Downloading' : cap1(S);
    return { word: w, cls: S === 'downloading' ? 'dl' : S === 'error' ? 'err' : S === 'finished' ? 'ok' : '', rest: parts.join(' · '), restErr: false };
  }
  switch (S) {
    case 'resolving':
      // §2.4: the title *is* the URL until resolution lands, so repeating it would be noise.
      return { ...none, word: 'Resolving', rest: it.title === it.url ? '' : it.url || '' };
    case 'queued': {
      const nyl = it.error && it.error.code === 'not_yet_live';
      return { ...none, word: nyl ? 'Scheduled' : it.auto_start ? 'Waiting' : 'Paused', rest: nyl ? it.error.message : selText(it) };
    }
    case 'preparing':
      return { word: 'Preparing', cls: 'dl', rest: [selText(it), it.msg].filter(Boolean).join(' · '), restErr: false };
    case 'downloading': {
      const parts = [selText(it)];
      if (it.speed) parts.push(speed(it.speed));
      const total = it.total_bytes ?? it.total_bytes_estimate;
      if (it.downloaded_bytes != null) parts.push(total ? `${bytes(it.downloaded_bytes)} of ${bytes(total)}` : bytes(it.downloaded_bytes));
      if (it.eta != null) parts.push(`${dur(it.eta)} left`);
      return { word: 'Downloading', cls: 'dl', rest: parts.filter(Boolean).join(' · '), restErr: false };
    }
    case 'postprocessing': {
      const parts = [];
      if (it.msg) parts.push(it.msg);
      if (it.phase) parts.push(it.phase_percent != null ? `${it.phase} ${Math.round(it.phase_percent)}%` : it.phase);
      if (!parts.length) parts.push(selText(it));
      return { word: 'Post-processing', cls: 'pp', rest: parts.join(' · '), restErr: false };
    }
    case 'finished': {
      const parts = [];
      if (it.size != null) parts.push(bytes(it.size));
      if (it.selection) parts.push(it.selection.format);
      if (it.finished_at) parts.push(rel(it.finished_at));
      return { ...none, rest: parts.filter(Boolean).join(' · ') };
    }
    case 'error':
      return { ...none, rest: (it.error && it.error.message) || 'Download failed', restErr: true };
    case 'canceled':
      return { ...none, word: 'Canceled', rest: it.msg || '' };
    default:
      return { ...none, word: cap1(S) };
  }
}

/* ---------------------------------------------------- group children */

/** §5.3: a `children_inline: false` group has no children in the snapshot. */
function fetchKids(id) {
  if (kidsFetched.has(id)) return;
  kidsFetched.add(id);
  api(`api/v2/items?group_id=${encodeURIComponent(id)}&limit=200`)
    .then((r) => { for (const it of r.items || []) state.items.set(it.id, it); markDirty(); })
    .catch((e) => { kidsFetched.delete(id); if (e.code !== 'unauthorized') toast('error', e.message); });
}

function toggleGroup(id) {
  if (expanded.has(id)) expanded.delete(id);
  else {
    expanded.add(id);
    const g = state.items.get(id);
    if (g && g.children_inline === false) fetchKids(id);
  }
  markDirty();
}

function renderKids(box, group) {
  const kids = sorted([...state.items.values()].filter((i) => i.group_id === group.id));
  const limit = kidLimit.get(group.id) || 6;
  const shown = kids.slice(0, limit);
  const sig = `${shown.length}/${kids.length}:` + shown.map((k) => `${k.id}${k.status}${Math.round(k.percent || 0)}`).join(',');
  const r = rows.get(group.id);
  if (r.v.kidsSig === sig) return;
  r.v.kidsSig = sig;

  box.replaceChildren();
  if (!kids.length) {
    const p = document.createElement('div');
    p.className = 'kid';
    p.innerHTML = '<div class="kid-title dim">No children yet</div>';
    box.appendChild(p);
    return;
  }
  for (const k of shown) {
    const el = document.createElement('div');
    el.className = 'kid';
    const cls = k.status === 'finished' ? 'ok' : k.status === 'error' ? 'err' : k.status === 'downloading' || k.status === 'preparing' ? 'dl' : '';
    const running = cls === 'dl';
    el.innerHTML =
      `<div class="kid-disc${cls ? ' ' + cls : ''}">${running ? '' : icon(k.status === 'finished' ? 'check' : k.status === 'error' ? 'cross' : 'clock', 12, 2.6)}</div>` +
      '<div class="kid-body"><div class="kid-title"></div></div>' +
      '<div class="kid-meta"></div>';
    el.querySelector('.kid-title').textContent = k.title || k.url;
    let metaTxt = '';
    if (k.status === 'finished') metaTxt = bytes(k.size);
    else if (k.status === 'error') metaTxt = 'Failed';
    else if (running) {
      metaTxt = [`${Math.round(k.percent || 0)}%`, k.eta != null ? `${dur(k.eta)} left` : ''].filter(Boolean).join(' · ');
      const body = el.querySelector('.kid-body');
      const bar = document.createElement('div');
      bar.className = 'bar';
      bar.innerHTML = '<div class="fill"></div>';
      bar.firstElementChild.style.width = `${Math.max(0, Math.min(100, k.percent || 0)).toFixed(1)}%`;
      body.appendChild(bar);
    } else metaTxt = cap1(k.status);
    el.querySelector('.kid-meta').textContent = metaTxt;
    if (!running && k.status !== 'finished' && k.status !== 'error') el.querySelector('.kid-title').classList.add('dim');
    box.appendChild(el);
  }
  if (kids.length > shown.length) {
    const more = document.createElement('div');
    more.className = 'kid-more';
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'link';
    b.textContent = `Show ${kids.length - shown.length} more`;
    b.addEventListener('click', () => { kidLimit.set(group.id, limit + 20); rows.get(group.id).v.kidsSig = ''; markDirty(); });
    more.appendChild(b);
    box.appendChild(more);
  }
}

/* ------------------------------------------------------- 8. row actions */

function actsFor(it) {
  const A = state.caps?.actions || ['start', 'pause', 'cancel', 'retry', 'delete'];
  const has = (a) => A.includes(a);
  const S = it.status;
  const start = { id: 'start', label: 'Start', icon: 'play', cls: 'primary' };
  // §4.2: `start` on a terminal item *is* the retry.
  const retry = { id: 'start', label: 'Retry', icon: 'retry', cls: 'primary' };
  const pause = { id: 'pause', label: 'Pause', icon: 'pause' };
  const cancel = { id: 'cancel', label: 'Cancel', icon: 'stop' };
  const del = { id: 'delete', label: 'Delete', icon: 'trash', cls: 'danger' };
  const open = { id: 'open', label: 'Open file', icon: 'screen', cls: 'primary' };
  const source = { id: 'source', label: 'Open source', icon: 'external' };
  const copy = { id: 'copy', label: 'Copy link', icon: 'copy' };

  // `capabilities.actions` is the authority on which of the five mutations exist (§4.2).
  const keep = (list) => list.filter((a) => (['start', 'pause', 'cancel', 'delete'].includes(a.id) ? has(a.id) : true));

  if (S === 'finished') return { inline: keep([open, source, del]), menu: keep([open, source, copy, del]), more: false };
  if (S === 'error' || S === 'canceled') return { inline: keep([retry, del]), menu: keep([retry, source, copy, del]), more: false };
  if (S === 'queued' && !it.auto_start) return { inline: keep([start, del]), menu: keep([start, cancel, source, copy, del]), more: false };
  if (S === 'resolving') return { inline: keep([cancel]), menu: keep([cancel, source, copy, del]), more: true };
  return { inline: keep([pause, cancel]), menu: keep([pause, cancel, source, copy, del]), more: true };
}

async function runAction(id, action) {
  const it = state.items.get(id);
  if (!it) return;
  if (action === 'open') { const u = resolveUrl(it.download_url); if (u) openExternal(u); return; }
  if (action === 'source') { openExternal(it.url); return; }
  if (action === 'copy') {
    try { await navigator.clipboard.writeText(it.url); toast('info', 'Link copied'); }
    catch { toast('error', 'Could not copy the link'); }
    return;
  }
  await post(action, [id]);
}

async function post(action, ids, extra) {
  if (!ids.length) return;
  const body = Object.assign({ action, ids }, extra);
  // Deleting a row is a list operation, never a disk operation: the page says so explicitly
  // rather than inheriting whatever `DELETE_FILE_ON_TRASHCAN` happens to be (§4.2).
  if (action === 'delete') body.delete_file = false;
  try {
    const r = await api('api/v2/items/actions', { method: 'POST', body });
    const skipped = (r && r.skipped) || [];
    if (skipped.length && skipped.length === ids.length) toast('warning', `${action}: ${skipped[0].reason.replace(/_/g, ' ')}`);
  } catch (e) {
    if (e.code !== 'unauthorized') toast('error', e.message);
  }
}

/* --------------------------------------------------------- 9. add flow */

const add = {
  url: '',
  download_type: 'video',
  format: 'mp4',
  quality: 'best',
  codec: 'auto',
  folder: '',
  auto_start: store.get('aulos.autostart', 'true') !== 'false',
  custom_name_prefix: '',
};

let picker = { types: [] };
let defaultsApplied = false;
let dirs = null;
let catalogTimer = null;
let catalogSeq = 0;

function pickerFromCapabilities(caps) {
  const order = caps.download_types || [];
  const byType = new Map();
  for (const f of caps.formats || []) {
    const t = f.download_type;
    if (!byType.has(t)) byType.set(t, { id: t, label: cap1(t), default_format: null, formats: [] });
    byType.get(t).formats.push({
      id: f.id,
      label: f.text || f.id,
      qualities: (f.qualities || []).map((q) => ({ id: q.id, label: q.text || q.id, notice: null })),
      codecs: t === 'video' ? (caps.codecs || []).map((c) => ({ id: c, label: cap1(c) })) : [],
      notice: null,
    });
  }
  const types = [...byType.values()];
  types.sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id));
  return { types, provider: null, notice: null };
}

function pickerFromCatalog(cat) {
  return {
    types: (cat.download_types || []).map((d) => ({
      id: d.id,
      label: d.label || cap1(d.id),
      default_format: d.default_format,
      formats: (d.formats || []).map((f) => ({
        id: f.id,
        label: f.label || f.id,
        default_quality: f.default_quality,
        qualities: (f.qualities || []).map((q) => ({ id: q.id, label: q.label || q.id, notice: q.notice })),
        codecs: f.codecs || [],
        notice: f.notice || null,
      })),
    })),
    provider: cat.provider && cat.provider !== 'merged' ? cat.provider : null,
    notice: null,
  };
}

/* A picker and a selection are a pair, and there are two of them: the add bar's (refined per URL
   by the catalog) and the subscription form's (the generic capabilities ladder). */
function typeIn(p, sel) { return p.types.find((t) => t.id === sel.download_type) || p.types[0]; }
// §4.6: honour the type's `default_format`; `formats[0]` is `Any` where it said `MP4`.
function formatIn(p, sel) {
  const t = typeIn(p, sel);
  const at = (id) => t.formats.find((f) => f.id === id);
  return t && (at(sel.format) || at(t.default_format) || t.formats[0]);
}
const currentType = () => typeIn(picker, add);
const currentFormat = () => formatIn(picker, add);

/** Clamp `sel` to what `p` actually offers, and answer with the format it settled on. */
function clampSel(p, sel) {
  if (!p.types.length) return null;
  const t = typeIn(p, sel);
  sel.download_type = t.id;
  const f = formatIn(p, sel);
  if (f) sel.format = f.id;
  if (f && !f.qualities.some((q) => q.id === sel.quality)) sel.quality = f.default_quality || (f.qualities[0] && f.qualities[0].id) || 'best';
  if (!f || !f.codecs.length) sel.codec = 'auto';
  return f;
}

function qualityLabel(formatId, qualityId) {
  for (const t of picker.types) {
    for (const f of t.formats) {
      if (f.id !== formatId) continue;
      const q = f.qualities.find((x) => x.id === qualityId);
      if (q) return q.label;
    }
  }
  return qualityId;
}

/** Clamp the selection to what the current picker actually offers, then repaint the controls. */
function syncPicker() {
  if (!clampSel(picker, add)) return;
  renderPicker();
}

function fillSelect(sel, options, value) {
  const sig = options.map((o) => o.id + o.label).join('|') + '=' + value;
  if (sel.dataset.sig === sig) return;
  sel.dataset.sig = sig;
  sel.replaceChildren();
  for (const o of options) {
    const opt = document.createElement('option');
    opt.value = o.id;
    opt.textContent = o.label;
    sel.appendChild(opt);
  }
  sel.value = value;
}

function renderPicker() {
  const t = currentType();
  const f = currentFormat();
  if (!t || !f) return;
  const types = picker.types.map((x) => ({ id: x.id, label: x.label }));
  const formats = t.formats.map((x) => ({ id: x.id, label: x.label }));
  const qualities = f.qualities.map((x) => ({ id: x.id, label: x.label }));
  const codecs = f.codecs.map((x) => ({ id: x.id, label: x.label }));

  fillSelect($('type'), types, add.download_type);
  fillSelect($('quality'), qualities, add.quality);
  fillSelect($('format'), formats, add.format);
  $('codec-x').hidden = codecs.length === 0;
  if (codecs.length) fillSelect($('codec'), codecs, add.codec);
  fillSelect($('sheet-format'), formats, add.format);
  $('sheet-codec-wrap').hidden = codecs.length === 0;
  if (codecs.length) fillSelect($('sheet-codec'), codecs, add.codec);

  const seg = $('sheet-type');
  const segSig = types.map((x) => x.id).join(',');
  if (seg.dataset.sig !== segSig) {
    seg.dataset.sig = segSig;
    seg.replaceChildren();
    for (const x of types) {
      const b = document.createElement('button');
      b.type = 'button';
      b.textContent = x.label;
      b.dataset.id = x.id;
      b.addEventListener('click', () => { add.download_type = x.id; add.format = ''; syncPicker(); refreshDirs(); });
      seg.appendChild(b);
    }
  }
  for (const b of seg.children) b.setAttribute('aria-pressed', b.dataset.id === add.download_type ? 'true' : 'false');

  const chips = $('sheet-quality');
  const chipSig = qualities.map((x) => x.id).join(',');
  if (chips.dataset.sig !== chipSig) {
    chips.dataset.sig = chipSig;
    chips.replaceChildren();
    for (const q of qualities) {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = 'chip';
      b.textContent = q.label;
      b.dataset.id = q.id;
      b.addEventListener('click', () => { add.quality = q.id; renderPicker(); });
      chips.appendChild(b);
    }
  }
  for (const b of chips.children) b.setAttribute('aria-pressed', b.dataset.id === add.quality ? 'true' : 'false');

  $('autostart').setAttribute('aria-checked', add.auto_start ? 'true' : 'false');
  $('sheet-autostart').setAttribute('aria-checked', add.auto_start ? 'true' : 'false');
  $('prefix').value = add.custom_name_prefix;

  const notice = f.notice || (f.qualities.find((q) => q.id === add.quality) || {}).notice;
  renderProv(picker.provider, notice);
}

function renderProv(provider, notice) {
  const txt = [];
  if (provider) txt.push(`Handled by ${provider}`);
  if (notice) txt.push(notice);
  for (const el of [$('prov'), $('sheet-prov')]) {
    if (!txt.length) { el.hidden = true; el.replaceChildren(); continue; }
    el.hidden = false;
    el.replaceChildren();
    const dot = document.createElement('span');
    dot.className = 'ok-dot';
    dot.innerHTML = icon('check', 10, 3.5);
    el.appendChild(dot);
    const span = document.createElement('span');
    if (provider) {
      span.append('Handled by ');
      const b = document.createElement('b');
      b.textContent = provider;
      span.appendChild(b);
      if (notice) span.append(` · ${notice}`);
    } else span.textContent = notice;
    el.appendChild(span);
  }
}

/** The custom-dirs list for one download type, `Base` first. `null` when the server has none. */
async function folderOptions(type) {
  if (!state.caps || !state.caps.config || !state.caps.config.custom_dirs) return null;
  if (!dirs) {
    try { dirs = await api('api/v2/custom-dirs'); }
    catch { return null; }
  }
  const list = (type === 'audio' ? dirs.audio_download_dir : dirs.download_dir) || [];
  return [{ id: '', label: 'Base' }].concat(list.filter(Boolean).map((d) => ({ id: d, label: d })));
}

async function refreshDirs() {
  const options = await folderOptions(add.download_type);
  if (!options) return;
  if (!options.some((o) => o.id === add.folder)) add.folder = '';
  fillSelect($('folder'), options, add.folder);
  fillSelect($('sheet-folder'), options, add.folder);
  $('folder-wrap').hidden = false;
  $('folder-sep').hidden = false;
  $('sheet-folder-row').hidden = false;
}

function setUrl(u, from) {
  add.url = u;
  if (from !== 'bar') $('url').value = u;
  if (from !== 'sheet') $('sheet-url').value = u;
  clearAddErr();
  clearTimeout(catalogTimer);
  catalogTimer = setTimeout(loadCatalog, 300);
}

async function loadCatalog() {
  const u = add.url.trim();
  if (!u || !/^https?:\/\//i.test(u)) {
    if (state.caps) { picker = pickerFromCapabilities(state.caps); syncPicker(); }
    return;
  }
  const mine = ++catalogSeq;
  try {
    const cat = await api(`api/v2/catalog?url=${encodeURIComponent(u)}`);
    if (mine !== catalogSeq) return;
    picker = pickerFromCatalog(cat);
    syncPicker();
  } catch { /* keep the capabilities picker */ }
}

function showAddErr(msg, field) {
  for (const el of [$('add-err'), $('sheet-err')]) { el.textContent = msg; el.hidden = false; }
  $('url').parentElement.classList.toggle('bad', field === 'url');
  if (field && field !== 'url') toast('error', `${field}: ${msg}`);
}

function clearAddErr() {
  for (const el of [$('add-err'), $('sheet-err')]) { el.hidden = true; el.textContent = ''; }
  $('url').parentElement.classList.remove('bad');
}

async function submitAdd() {
  const url = add.url.trim();
  if (!url) { showAddErr('Paste a link first.', 'url'); return; }
  const body = {
    url,
    download_type: add.download_type,
    format: add.format,
    quality: add.quality,
    codec: add.codec,
    folder: add.folder || null,
    auto_start: add.auto_start,
  };
  if (add.custom_name_prefix) body.custom_name_prefix = add.custom_name_prefix;
  const btns = [$('add-btn'), $('sheet-add')];
  btns.forEach((b) => { b.disabled = true; });
  try {
    const r = await api('api/v2/downloads', { method: 'POST', body });
    clearAddErr();
    setUrl('');
    closeSheet();
    for (const w of (r && r.warnings) || []) toast('warning', w);
    if (r && r.duplicates && r.duplicates.length && !(r.ids || []).length) toast('info', 'Already in the queue.');
  } catch (e) {
    if (e.code !== 'unauthorized') showAddErr(e.message, e.field);
  } finally {
    btns.forEach((b) => { b.disabled = false; });
  }
}

/* ----------------------------------------------------- 10. subscriptions */

/* PROTOCOL §9's object, §4.7's routes and §5.9's two frames. The snapshot seeds the list; a
   `subscription` frame upserts a row (a check moves `checking`, `last_checked`, `seen_count`,
   `error`) and `subscription_removed` takes it away. The REST answers are applied too, so a
   toggle or a rename lands before the frame does. */

/** The form's own selection. Its picker is the generic capabilities ladder rather than the
 *  URL-refined catalog: a subscription is a standing order, not one download. */
const subAdd = { download_type: 'video', format: 'mp4', quality: 'best', codec: 'auto', folder: '' };
let subPicker = { types: [] };
let subInterval = 60;
const subRows = new Map();      // id → {el, refs, v}
const subEditing = new Set();   // ids whose inline editor is open

const subPath = (id) => `api/v2/subscriptions/${encodeURIComponent(id)}`;

function subHost(u) {
  try { return new URL(u).host.replace(/^www\./, ''); } catch { return u || ''; }
}

/** `check_interval_minutes` in the largest unit it divides cleanly: 360 reads as 6 hours. */
function every(m) {
  if (m % 1440 === 0) { const d = m / 1440; return `every ${d === 1 ? 'day' : `${d} days`}`; }
  if (m % 60 === 0) { const h = m / 60; return `every ${h === 1 ? 'hour' : `${h} hours`}`; }
  return `every ${m} min`;
}

function subMeta(s) {
  const parts = [subHost(s.url), every(s.check_interval_minutes)];
  parts.push(s.last_checked ? `checked ${rel(s.last_checked)}` : 'never checked');
  if (s.enabled && !s.checking) parts.push(until(s.next_due));
  parts.push(`${s.seen_count || 0} seen`);
  return parts.filter(Boolean).join(' · ');
}

function subWord(s) {
  if (s.checking) return { word: 'Checking…', cls: 'dl' };
  if (s.consecutive_failures > 0) return { word: `Failed ×${s.consecutive_failures}`, cls: 'err' };
  return s.enabled ? { word: 'Active', cls: 'ok' } : { word: 'Paused', cls: '' };
}

/** Reconcile by id, like the queue's rows: a node is created once and then patched. */
function reconcileSubs(container, list) {
  let node = container.firstElementChild;
  for (const s of list) {
    const r = subRowFor(s.id);
    if (node === r.el) node = node.nextElementSibling;
    else container.insertBefore(r.el, node);
    patchSub(r, s);
  }
  while (node) { const next = node.nextElementSibling; node.remove(); node = next; }
  for (const id of [...subRows.keys()]) if (!state.subs.has(id)) subRows.delete(id);
}

function subRowFor(id) {
  let r = subRows.get(id);
  if (r) return r;
  const el = document.createElement('div');
  el.className = 'row sub';
  el.dataset.sub = id;
  el.innerHTML =
    '<div class="row-main">' +
      '<div class="disc"></div>' +
      '<div class="row-body"><div class="row-title"></div></div>' +
      '<div class="row-acts">' +
        '<button class="sw sw-sm" type="button" role="switch" aria-checked="true" aria-label="Enabled"></button>' +
        `<button class="iconbtn" type="button" data-sact="check" aria-label="Check now" title="Check now">${icon('retry', 16, 2.2)}</button>` +
        `<button class="iconbtn" type="button" data-sact="edit" aria-label="Edit" title="Edit">${icon('edit', 16, 2)}</button>` +
        `<button class="iconbtn danger" type="button" data-sact="delete" aria-label="Delete" title="Delete">${icon('trash', 16, 2)}</button>` +
      '</div>' +
    '</div>' +
    // Outside `.row-main`, so the meta gets the whole row width instead of what four 44 px
    // controls leave of it on a phone. `.sub-line` indents it back under the title.
    '<div class="row-sub sub-line"><span class="st"></span><span class="rest"></span></div>' +
    '<div class="sub-err" hidden></div>' +
    '<div class="subedit" hidden>' +
      '<label class="xfield"><span class="label">Name</span><span class="field"><input class="e-name" type="text" autocomplete="off" aria-label="Name"></span></label>' +
      '<label class="xfield"><span class="label">Check every</span><span class="field"><input class="e-every" type="number" min="1" step="1" inputmode="numeric" aria-label="Check interval in minutes"><span class="unit">min</span></span></label>' +
      '<div class="edit-acts"><button class="link e-cancel" type="button">Cancel</button><button class="btn-sm e-save" type="button">Save</button></div>' +
    '</div>';
  const refs = {
    disc: el.querySelector('.disc'),
    title: el.querySelector('.row-title'),
    st: el.querySelector('.st'),
    rest: el.querySelector('.rest'),
    err: el.querySelector('.sub-err'),
    sw: el.querySelector('.sw'),
    check: el.querySelector('[data-sact="check"]'),
    edit: el.querySelector('.subedit'),
    name: el.querySelector('.e-name'),
    every: el.querySelector('.e-every'),
  };
  refs.sw.addEventListener('click', () => subToggle(id));
  refs.check.addEventListener('click', () => subCheck(id));
  el.querySelector('[data-sact="edit"]').addEventListener('click', () => subEdit(id, true));
  el.querySelector('[data-sact="delete"]').addEventListener('click', () => subDelete(id));
  el.querySelector('.e-cancel').addEventListener('click', () => subEdit(id, false));
  el.querySelector('.e-save').addEventListener('click', () => subSave(id));
  r = { el, refs, v: {} };
  subRows.set(id, r);
  return r;
}

function patchSub(r, s) {
  const { refs, v } = r;

  const title = s.name || subHost(s.url);
  if (v.title !== title) { refs.title.textContent = title; v.title = title; }

  const w = subWord(s);
  if (v.word !== w.word || v.cls !== w.cls) {
    refs.st.textContent = w.word;
    refs.st.className = `st${w.cls ? ' ' + w.cls : ''}`;
    v.word = w.word; v.cls = w.cls;
  }
  const meta = subMeta(s);
  if (v.meta !== meta) { refs.rest.textContent = meta; v.meta = meta; }

  const err = s.error || '';
  if (v.err !== err) { refs.err.textContent = err; refs.err.hidden = !err; refs.err.title = err; v.err = err; }

  const disc = s.checking ? 'spin' : s.consecutive_failures > 0 ? 'err' : s.enabled ? 'dl' : '';
  if (v.disc !== disc) {
    refs.disc.className = `disc${disc ? ' ' + disc : ''}`;
    refs.disc.innerHTML = disc === 'spin' ? '' : icon('rss', 16, 2.2);
    v.disc = disc;
  }

  const on = s.enabled ? 'true' : 'false';
  if (v.on !== on) { refs.sw.setAttribute('aria-checked', on); v.on = on; }
  const checking = !!s.checking;
  if (v.checking !== checking) { refs.check.disabled = checking; v.checking = checking; }

  // The editor's inputs are only written when it opens, so a frame arriving mid-edit cannot
  // overwrite what is being typed.
  const editing = subEditing.has(s.id);
  if (v.editing !== editing) { refs.edit.hidden = !editing; v.editing = editing; }
}

/** One REST call, reported rather than thrown: `{ok}` says whether the caller may act on it. */
async function subCall(path, init) {
  try { return { ok: true, body: await api(path, init) }; }
  catch (e) { if (e.code !== 'unauthorized') toast('error', e.message); return { ok: false }; }
}

/** Apply the `Subscription` a POST/PATCH answered with, so the row moves before the frame lands. */
function subUpsert(s) { if (s && s.id) state.subs.set(s.id, s); markDirty(); }

async function subToggle(id) {
  const s = state.subs.get(id);
  if (!s) return;
  s.enabled = !s.enabled;                 // optimistic; the answer and the frame are the authority
  markDirty();
  const r = await subCall(subPath(id), { method: 'PATCH', body: { enabled: s.enabled } });
  if (r.ok) subUpsert(r.body);
  else { s.enabled = !s.enabled; markDirty(); }
}

function subCheck(id) { subCall(`${subPath(id)}/check`, { method: 'POST' }); }

async function subCheckAll() {
  const r = await subCall('api/v2/subscriptions/check', { method: 'POST', body: {} });
  if (r.ok && r.body) toast('info', `Checking ${r.body.count} subscription${r.body.count === 1 ? '' : 's'}…`);
}

async function subDelete(id) {
  const s = state.subs.get(id);
  if (!s) return;
  if (!window.confirm(`Stop watching “${s.name || subHost(s.url)}”? Downloads already made are kept.`)) return;
  const r = await subCall(subPath(id), { method: 'DELETE' });
  if (!r.ok) return;
  state.subs.delete(id);
  subEditing.delete(id);
  markDirty();
}

function subEdit(id, open) {
  const r = subRows.get(id), s = state.subs.get(id);
  if (!r || !s) return;
  if (open) {
    r.refs.name.value = s.name || '';
    r.refs.every.value = String(s.check_interval_minutes);
    subEditing.add(id);
  } else subEditing.delete(id);
  r.refs.edit.hidden = !open;
  r.v.editing = open;
  if (open) r.refs.name.focus();
}

async function subSave(id) {
  const r = subRows.get(id);
  if (!r) return;
  const body = {
    name: r.refs.name.value.trim(),
    check_interval_minutes: minutes(r.refs.every.value),
  };
  const out = await subCall(subPath(id), { method: 'PATCH', body });
  if (!out.ok) return;
  subEdit(id, false);
  subUpsert(out.body);
}

/** §9: `check_interval_minutes` has a minimum of 1, and an empty box means the server default. */
function minutes(raw) {
  const n = Math.round(Number(raw));
  return isFinite(n) && n > 0 ? n : subInterval;
}

/* ---------------------------------------------------------- the add form */

function subFormOpen(open) {
  $('sub-form').hidden = !open;
  $('subs-new').setAttribute('aria-expanded', open ? 'true' : 'false');
  if (!open) return;
  if (!$('sub-every').value) $('sub-every').value = String(subInterval);
  renderSubForm();
  refreshSubDirs();
  $('sub-url').focus();
}

function subErr(msg) {
  const el = $('sub-err');
  el.textContent = msg || '';
  el.hidden = !msg;
  $('sub-url').parentElement.classList.toggle('bad', !!msg);
}

function renderSubForm() {
  const f = clampSel(subPicker, subAdd);
  if (!f) return;
  const t = typeIn(subPicker, subAdd);
  const opt = (x) => ({ id: x.id, label: x.label });
  fillSelect($('sub-type'), subPicker.types.map(opt), subAdd.download_type);
  fillSelect($('sub-format'), t.formats.map(opt), subAdd.format);
  fillSelect($('sub-quality'), f.qualities.map(opt), subAdd.quality);
  $('sub-codec-x').hidden = f.codecs.length === 0;
  if (f.codecs.length) fillSelect($('sub-codec'), f.codecs.map(opt), subAdd.codec);
}

async function refreshSubDirs() {
  const options = await folderOptions(subAdd.download_type);
  if (!options) return;
  if (!options.some((o) => o.id === subAdd.folder)) subAdd.folder = '';
  fillSelect($('sub-folder'), options, subAdd.folder);
  $('sub-folder-x').hidden = false;
}

async function submitSub() {
  const url = $('sub-url').value.trim();
  if (!url) { subErr('Paste a channel or playlist link first.'); return; }
  const body = {
    url,
    download_type: subAdd.download_type,
    format: subAdd.format,
    quality: subAdd.quality,
    codec: subAdd.codec,
    folder: subAdd.folder || null,
    check_interval_minutes: minutes($('sub-every').value),
  };
  const name = $('sub-name').value.trim();
  if (name) body.name = name;
  const btn = $('sub-save');
  btn.disabled = true;
  try {
    const s = await api('api/v2/subscriptions', { method: 'POST', body });
    subErr('');
    $('sub-url').value = '';
    $('sub-name').value = '';
    subFormOpen(false);
    subUpsert(s);
  } catch (e) {
    // §9: a single-video URL is `400 validation_failed` and an existing one `409 conflict`; both
    // carry the sentence to show, so show it rather than inventing one.
    if (e.code !== 'unauthorized') subErr(e.message);
  } finally { btn.disabled = false; }
}

/* ------------------------------------------------------------ 11. chrome */

/* theme */

let theme = store.get('aulos.theme', '') || DEFAULT_THEME;

function applyTheme() {
  const dark = theme === 'dark' || (theme === 'auto' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.dataset.mode = dark ? 'dark' : 'light';
  // Two media-scoped `theme-color` tags ship in the document so an installed PWA paints its
  // toolbar right before this module runs. After it, the resolved theme is the answer.
  for (const tc of document.querySelectorAll('meta[name="theme-color"]')) {
    tc.removeAttribute('media');
    tc.content = dark ? '#000000' : '#F2F2F7';
  }
  const btn = $('theme-btn');
  btn.innerHTML = icon(theme === 'auto' ? 'auto' : theme === 'light' ? 'sun' : 'moon', 20, 1.8);
  btn.setAttribute('aria-label', `Theme: ${theme === 'auto' ? 'system' : theme}`);
}

/* connection pill */

const CONN = {
  connecting: ['wait', 'Connecting…'],
  live: ['live', 'Live'],
  reconnecting: ['wait', 'Reconnecting…'],
  offline: ['off', 'Offline'],
  auth: ['auth', 'Sign in'],
};

function setConn(kind) {
  if (state.conn === kind) return;
  state.conn = kind;
  const [cls, text] = CONN[kind] || CONN.offline;
  $('conn').className = `pill ${cls}`;
  $('conn-text').textContent = text;
}

/* toasts */

function toast(level, message) {
  if (!message) return;
  const box = $('toasts');
  const el = document.createElement('div');
  el.className = `toast ${level}`;
  el.setAttribute('role', level === 'error' ? 'alert' : 'status');
  const msg = document.createElement('div');
  msg.className = 'msg';
  msg.textContent = message;
  el.appendChild(msg);
  const close = document.createElement('button');
  close.type = 'button';
  close.className = 'iconbtn';
  close.setAttribute('aria-label', 'Dismiss');
  close.innerHTML = icon('cross', 14, 2.4);
  close.addEventListener('click', () => el.remove());
  el.appendChild(close);
  box.appendChild(el);
  if (level !== 'error') setTimeout(() => el.remove(), 6000);
}

/* the row overflow menu */

function openMenu(anchor, id) {
  const it = state.items.get(id);
  if (!it) return;
  const m = $('menu');
  m.replaceChildren();
  for (const a of actsFor(it).menu) {
    const b = document.createElement('button');
    b.type = 'button';
    b.setAttribute('role', 'menuitem');
    if (a.cls === 'danger') b.classList.add('danger');
    b.innerHTML = `<span class="ico">${icon(a.icon, 16, 2)}</span>`;
    const s = document.createElement('span');
    s.textContent = a.label;
    b.appendChild(s);
    b.addEventListener('click', () => { closeMenu(); runAction(id, a.id); });
    m.appendChild(b);
  }
  m.hidden = false;
  const r = anchor.getBoundingClientRect();
  const w = m.offsetWidth, h = m.offsetHeight;
  m.style.left = `${Math.max(8, Math.min(window.innerWidth - w - 8, r.right - w))}px`;
  m.style.top = `${r.bottom + h + 8 > window.innerHeight ? Math.max(8, r.top - h - 4) : r.bottom + 4}px`;
  setTimeout(() => document.addEventListener('pointerdown', onDocDown, { once: true }), 0);
}

function onDocDown(e) { if (!$('menu').contains(e.target)) closeMenu(); }
function closeMenu() { $('menu').hidden = true; }

/* sheets — `aria-modal` is a promise: inert behind, Tab cycles inside, focus returns. */

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
let sheetReturn = null;

/** The sheet on top (the token sheet stacks over the add sheet), or null. */
const topSheet = () => (!$('token-sheet').hidden ? $('token-sheet') : $('sheet').hidden ? null : $('sheet'));

function inertBg(on) {
  for (const bg of [$('hdr'), $('wrap')]) {
    bg.inert = on;
    if (on) bg.setAttribute('aria-hidden', 'true'); else bg.removeAttribute('aria-hidden');
  }
}

function openOverlay(el, focus) {
  if (!topSheet()) sheetReturn = document.activeElement;
  $('scrim').hidden = false;
  el.hidden = false;
  inertBg(true);
  focus.focus();
}

function closeOverlay(el) {
  el.hidden = true;
  if (topSheet()) return;
  $('scrim').hidden = true;
  inertBg(false);
  const back = sheetReturn;
  sheetReturn = null;
  if (back && back.isConnected) back.focus();
}

function trapTab(e) {
  const sheet = e.key === 'Tab' && topSheet();
  if (!sheet) return;
  const items = [...sheet.querySelectorAll(FOCUSABLE)].filter((el) => el.offsetParent !== null);
  if (!items.length) return;
  const at = document.activeElement;
  if (at === (e.shiftKey ? items[0] : items[items.length - 1]) || !sheet.contains(at)) {
    e.preventDefault();
    (e.shiftKey ? items[items.length - 1] : items[0]).focus();
  }
}

function openSheet() { openOverlay($('sheet'), $('sheet-url')); }
function closeSheet() { closeOverlay($('sheet')); }
function closeToken() { closeOverlay($('token-sheet')); }

function needAuth() {
  setConn('auth');
  if (!$('token-sheet').hidden) return;
  $('token-input').value = token;
  openOverlay($('token-sheet'), $('token-input'));
}

/* boot */

async function loadCapabilities() {
  const caps = await api('api/v2/capabilities');
  state.caps = caps;
  const c = caps.config || {};
  if (!defaultsApplied) {
    defaultsApplied = true;
    if (c.default_download_type) add.download_type = c.default_download_type;
    if (c.default_format) add.format = c.default_format;
    if (c.default_quality) add.quality = c.default_quality;
    if (c.subscription_default_check_interval) subInterval = c.subscription_default_check_interval;
    Object.assign(subAdd, { download_type: add.download_type, format: add.format, quality: add.quality });
  }
  // `features` is the authority on whether the surface exists at all (§4.7); an older server that
  // does not list it gets no panel rather than four routes that 404.
  state.subsOk = !caps.features || caps.features.includes('subscriptions');
  picker = pickerFromCapabilities(caps);
  subPicker = pickerFromCapabilities(caps);
  syncPicker();
  renderSubForm();
  refreshDirs();
  markDirty();
  return caps;
}

function wire() {
  $('url-ico').innerHTML = icon('link', 18, 1.8);
  $('sub-url-ico').innerHTML = icon('rss', 18, 1.8);
  $('empty-ico').innerHTML = icon('download', 26, 1.6);
  $('add-btn').firstElementChild.innerHTML = icon('plus', 18, 2.2);
  $('clear').firstElementChild.innerHTML = icon('trash', 12, 2.2);
  for (const el of document.querySelectorAll('.chevron')) el.innerHTML = icon('chevron', 14, 2);

  $('theme-btn').addEventListener('click', () => {
    theme = theme === 'auto' ? 'light' : theme === 'light' ? 'dark' : 'auto';
    store.set('aulos.theme', theme);
    applyTheme();
  });
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => { if (theme === 'auto') applyTheme(); });

  $('url').addEventListener('input', (e) => setUrl(e.target.value, 'bar'));
  $('url').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); submitAdd(); } });
  $('url').addEventListener('focus', () => { if (isPhone()) { $('url').blur(); openSheet(); } });
  $('sheet-url').addEventListener('input', (e) => setUrl(e.target.value, 'sheet'));
  $('sheet-url').addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); submitAdd(); } });

  $('add-btn').addEventListener('click', () => { if (isPhone()) openSheet(); else submitAdd(); });
  $('sheet-add').addEventListener('click', submitAdd);
  $('sheet-cancel').addEventListener('click', closeSheet);
  $('scrim').addEventListener('click', () => { closeSheet(); closeToken(); });

  $('type').addEventListener('change', (e) => { add.download_type = e.target.value; add.format = ''; syncPicker(); refreshDirs(); });
  $('quality').addEventListener('change', (e) => { add.quality = e.target.value; renderPicker(); });
  $('format').addEventListener('change', (e) => { add.format = e.target.value; add.quality = ''; syncPicker(); });
  $('codec').addEventListener('change', (e) => { add.codec = e.target.value; });
  $('sheet-format').addEventListener('change', (e) => { add.format = e.target.value; add.quality = ''; syncPicker(); });
  $('sheet-codec').addEventListener('change', (e) => { add.codec = e.target.value; });
  $('prefix').addEventListener('input', (e) => { add.custom_name_prefix = e.target.value; });
  $('folder').addEventListener('change', (e) => { add.folder = e.target.value; $('sheet-folder').value = e.target.value; });
  $('sheet-folder').addEventListener('change', (e) => { add.folder = e.target.value; $('folder').value = e.target.value; });

  for (const el of [$('autostart'), $('sheet-autostart')]) {
    el.addEventListener('click', () => {
      add.auto_start = !add.auto_start;
      store.set('aulos.autostart', String(add.auto_start));
      $('autostart').setAttribute('aria-checked', add.auto_start ? 'true' : 'false');
      $('sheet-autostart').setAttribute('aria-checked', add.auto_start ? 'true' : 'false');
    });
  }

  $('more-opts').addEventListener('click', () => {
    const x = $('add-extra');
    x.hidden = !x.hidden;
    $('more-opts').setAttribute('aria-expanded', x.hidden ? 'false' : 'true');
  });

  $('start-all').addEventListener('click', () => {
    const ids = [...state.items.values()].filter((i) => !i.group_id && i.status === 'queued' && !i.auto_start).map((i) => i.id);
    post('start', ids);
  });

  // §4.7's one-call history clear, with `delete_file` pinned to false for the same reason the
  // row delete pins it: the shipped page never removes a file from disk.
  $('clear').addEventListener('click', async () => {
    const n = [...state.items.values()].filter((i) => !i.group_id && TERMINAL.has(i.status)).length;
    if (!n) return;
    if (!window.confirm(`Remove ${n} completed item${n === 1 ? '' : 's'} from the list?`)) return;
    try { await api('api/v2/items/clear', { method: 'POST', body: { where: 'done', delete_file: false } }); }
    catch (e) { if (e.code !== 'unauthorized') toast('error', e.message); }
  });

  $('subs-new').addEventListener('click', () => subFormOpen($('sub-form').hidden));
  $('subs-check').addEventListener('click', subCheckAll);
  $('sub-save').addEventListener('click', submitSub);
  $('sub-url').addEventListener('input', () => subErr(''));
  $('sub-url').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); submitSub(); } });
  $('sub-type').addEventListener('change', (e) => { subAdd.download_type = e.target.value; subAdd.format = ''; renderSubForm(); refreshSubDirs(); });
  $('sub-format').addEventListener('change', (e) => { subAdd.format = e.target.value; subAdd.quality = ''; renderSubForm(); });
  $('sub-quality').addEventListener('change', (e) => { subAdd.quality = e.target.value; });
  $('sub-codec').addEventListener('change', (e) => { subAdd.codec = e.target.value; });
  $('sub-folder').addEventListener('change', (e) => { subAdd.folder = e.target.value; });

  // "3 min ago" and "due in 12 min" go stale with no frame to announce it. One slow timer keeps
  // every relative time honest; the render is diffed, so a tick with nothing to say writes no DOM.
  setInterval(markDirty, 30000);

  $('show-older').addEventListener('click', async () => {
    const btn = $('show-older');
    btn.disabled = true;
    try {
      const q = new URLSearchParams({ status: 'finished,error,canceled', order: 'ord', limit: '50' });
      if (state.cursor) q.set('cursor', state.cursor);
      const r = await api(`api/v2/items?${q}`);
      const fetched = r.items || [];
      for (const it of fetched) if (!state.items.has(it.id)) state.items.set(it.id, it);
      // Grow the cap by what was revealed, or the window drops what the user just asked for.
      doneLimit += fetched.length;
      state.cursor = r.next_cursor || null;
      state.hasOlder = !!r.next_cursor;
      markDirty();
    } catch (e) {
      if (e.code !== 'unauthorized') toast('error', e.message);
    } finally { btn.disabled = false; }
  });

  $('token-save').addEventListener('click', () => {
    token = $('token-input').value.trim();
    if (token) store.set('aulos.token', token); else store.del('aulos.token');
    closeToken();
    backoff = 500;
    probedAuth = false;
    boot();
  });
  $('token-cancel').addEventListener('click', closeToken);
  $('token-input').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('token-save').click(); });

  document.addEventListener('keydown', trapTab);

  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    closeMenu();
    if (!$('token-sheet').hidden) closeToken();
    else if (!$('sheet').hidden) closeSheet();
  });

  // Paste anywhere outside a field → focus the URL field and fill it.
  document.addEventListener('paste', (e) => {
    const t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    const text = ((e.clipboardData && e.clipboardData.getData('text')) || '').trim();
    if (!/^https?:\/\/\S+$/i.test(text)) return;
    e.preventDefault();
    setUrl(text);
    if (isPhone()) openSheet();
    else $('url').focus();
  });

  window.addEventListener('online', () => { backoff = 500; connect(); });
  window.addEventListener('offline', () => setConn('offline'));
  window.addEventListener('resize', closeMenu);
}

async function boot() {
  try {
    await loadCapabilities();
  } catch (e) {
    if (e.code === 'unauthorized') return;    // the token sheet is already up
    toast('error', `Could not reach the server: ${e.message}`);
  }
  connect();
}

applyTheme();
wire();
markDirty();
boot();

// Test seam: the smoke suite reads these to assert state without scraping the DOM, and calls
// `flushNow` to time one render tick without waiting on the frame clock.
window.__aulos = {
  state, rows, add, subRows, subAdd, applyFrame, PREFIX,
  flushNow() { if (rafId) { cancelAnimationFrame(rafId); rafId = 0; } flush(); },
};
