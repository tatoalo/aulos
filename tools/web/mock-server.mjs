#!/usr/bin/env node
/**
 * A PROTOCOL v2 mock for the shipped page in `crates/aulos-api/web/`.
 *
 * It serves the real assets with the real substitutions and headers the server contract
 * specifies, and implements enough of PROTOCOL §4 and §5 to drive every code path in the UI:
 * a snapshot with one of each interesting row, deltas every 250 ms, adds, actions, paging,
 * the per-URL catalog, custom dirs, and a token mode that answers 401 until the bearer is sent.
 *
 *   node tools/web/mock-server.mjs [--port N] [--prefix /metube/] [--theme auto|light|dark]
 *                                  [--token SECRET] [--freeze] [--big-group] [--flap]
 *
 * `--port 0` picks an ephemeral port; the chosen one is printed as `LISTENING <port>`.
 * `--freeze` stops the delta timer so screenshots are byte-stable.
 */

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '../../crates/aulos-api/web');

/* ------------------------------------------------------------------ args */

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : fallback;
};
const flag = (name) => argv.includes(`--${name}`);

const PORT = Number(arg('port', '0'));
const THEME = arg('theme', 'auto');
const TOKEN = arg('token', '');
const FREEZE = flag('freeze');
const BIG_GROUP = flag('big-group');
/* Accept every upgrade and close it at once with 1013 — §5.1's "back off and reconnect". */
const FLAP = flag('flap');
const PREFIX = (() => {
  let p = arg('prefix', '/');
  if (!p.startsWith('/')) p = '/' + p;
  if (!p.endsWith('/')) p += '/';
  return p;
})();

const CSP = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
  "connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri 'none'; " +
  "form-action 'none'; frame-ancestors 'none'";

/* -------------------------------------------------------------- fixtures */

const ULID = (s) => (s + '00000000000000000000000000').slice(0, 26);
const now = Date.now();
let BOOT = ULID('BOOT');

const SELECTION = { download_type: 'video', codec: 'auto', format: 'mp4', quality: '1080' };
const REQUEST = {
  custom_name_prefix: '', playlist_item_limit: 0, auto_start: true, split_by_chapters: false,
  chapter_template: '%(title)s - %(section_number)02d - %(section_title)s.%(ext)s',
  subtitle_language: 'en', subtitle_mode: 'prefer_manual', ytdl_options_presets: [], ytdl_options_overrides: {},
};

function item(over) {
  return Object.assign({
    id: ULID('X'), kind: 'item', ord: 0, group_id: null, group_index: null,
    url: 'https://example.test/watch', title: 'Untitled', status: 'queued', auto_start: true, provider: 'ytdlp',
    percent: 0, speed: null, eta: null, downloaded_bytes: null, total_bytes: null, total_bytes_estimate: null,
    fragment_index: null, fragment_count: null, phase: null, phase_percent: null,
    msg: null, error: null, filename: null, size: null, download_url: null,
    chapter_files: [], subtitle_files: [],
    selection: { ...SELECTION }, folder: null, request: { ...REQUEST },
    created_at: now - 600000, started_at: null, finished_at: null, attempt: 0,
    source: { kind: 'api_v2', ref: null },
    children_total: null, children_done: null, children_error: null, children_active: null, children_inline: null,
  }, over);
}

const IDS = {
  dl: ULID('DL'), group: ULID('GRP'), c1: ULID('C1'), c2: ULID('C2'), c3: ULID('C3'),
  pp: ULID('PP'), resolving: ULID('RES'), waiting: ULID('WAIT'), fin: ULID('FIN'), err: ULID('ERR'),
  big: ULID('BIG'),
};

/* With --big-group: a group whose `children_inline` is false, so its children are absent from
   the snapshot and the page has to fetch them with GET api/v2/items?group_id= (§2.3, §5.3). */
const BIG_KIDS = Array.from({ length: 4 }, (_, i) => ({
  index: i,
  make: () => item({
    id: ULID(`BKID${String(i).padStart(2, '0')}Z`), ord: 300 + i, group_id: IDS.big, group_index: i + 1,
    title: `Episode ${i + 1} of a large season`, status: i === 0 ? 'downloading' : 'queued',
    percent: i === 0 ? 22 : 0, provider: 'ytdlp',
  }),
}));

/** id → Item. Live state; the actions and the delta ticker mutate it. */
const items = new Map();
let seq = 10000;
let generation = 47;
const log = [];   // every mutating request, for the smoke's "posts the right body" assertions

function seed() {
  items.clear();
  const put = (it) => items.set(it.id, it);
  put(item({
    id: IDS.dl, ord: 100, url: 'https://www.youtube.com/watch?v=dQw4w9WgXcQ',
    title: 'Le incredibili elezioni del 2000', status: 'downloading', provider: 'ytdlp',
    percent: 50, speed: 2202009, eta: 68, downloaded_bytes: 148897792, total_bytes: 298844160,
    started_at: now - 70000,
  }));
  put(item({
    id: IDS.group, kind: 'group', ord: 110, url: 'https://streamingcommunity.test/titles/42-series',
    title: '[Series title] — Season 2', status: 'downloading', provider: 'streamingcommunity',
    percent: 37, speed: 1048576, eta: 1200, downloaded_bytes: 4294967296, total_bytes_estimate: 11811160064,
    children_total: 12, children_done: 4, children_error: 0, children_active: 1, children_inline: true,
    started_at: now - 900000,
  }));
  put(item({
    id: IDS.c1, ord: 111, group_id: IDS.group, group_index: 4, title: 'S02E04 · [Episode title]',
    status: 'finished', percent: 100, size: 1288490188, downloaded_bytes: 1288490188, total_bytes: 1288490188,
    provider: 'streamingcommunity', filename: 'Series/S02E04.mp4', download_url: 'download/Series/S02E04.mp4',
    finished_at: now - 300000,
  }));
  put(item({
    id: IDS.c2, ord: 112, group_id: IDS.group, group_index: 5, title: 'S02E05 · [Episode title]',
    status: 'downloading', percent: 62, eta: 40, speed: 1048576, downloaded_bytes: 799014912,
    total_bytes: 1288490188, provider: 'streamingcommunity',
  }));
  put(item({
    id: IDS.c3, ord: 113, group_id: IDS.group, group_index: 6, title: 'S02E06 · [Episode title]',
    status: 'queued', provider: 'streamingcommunity',
  }));
  put(item({
    id: IDS.pp, ord: 120, url: 'https://www.youtube.com/watch?v=pp', title: '[Video title]',
    status: 'postprocessing', percent: 100, phase: 'audio_sync', phase_percent: 48, msg: 'Merging formats',
    selection: { ...SELECTION, quality: 'best_remux' }, started_at: now - 400000,
  }));
  put(item({
    id: IDS.resolving, ord: 130, url: 'https://www.youtube.com/playlist?list=PL123',
    title: 'https://www.youtube.com/playlist?list=PL123', status: 'resolving', provider: null, percent: 0,
  }));
  put(item({
    id: IDS.waiting, ord: 140, url: 'https://www.youtube.com/watch?v=w', title: '[Audio title]',
    status: 'queued', auto_start: false, selection: { download_type: 'audio', codec: 'auto', format: 'm4a', quality: 'best' },
  }));
  put(item({
    id: IDS.fin, ord: 90, url: 'https://www.youtube.com/watch?v=f', title: '[Video title]',
    status: 'finished', percent: 100, size: 298844160, downloaded_bytes: 298844160, total_bytes: 298844160,
    filename: 'My Video.mp4', download_url: 'download/My%20Video.mp4', finished_at: now - 12 * 60000,
  }));
  put(item({
    id: IDS.err, ord: 80, url: 'https://www.youtube.com/watch?v=e', title: '[Video title]',
    status: 'error', percent: 12, finished_at: now - 3600000,
    error: { code: 'no_format', message: 'No video formats found', field: null, provider: 'ytdlp', provider_code: 'ExtractorError' },
  }));
  if (BIG_GROUP) {
    put(item({
      id: IDS.big, kind: 'group', ord: 150, url: 'https://www.youtube.com/playlist?list=BIG',
      title: 'A 480-episode season', status: 'downloading', provider: 'ytdlp', percent: 4,
      children_total: 480, children_done: 12, children_error: 0, children_active: 1,
      children_inline: false,
    }));
  }
}
seed();

/** Terminal rows outside the snapshot window, for `Show older`. */
const OLDER = Array.from({ length: 12 }, (_, i) => item({
  id: ULID(`OLD${String(i).padStart(2, '0')}Z`), ord: 10 + i, title: `Older download ${i + 1}`, status: 'finished', percent: 100,
  size: 40000000 + i * 1000, filename: `old-${i}.mp4`, download_url: `download/old-${i}.mp4`,
  finished_at: now - (i + 2) * 86400000,
}));

const CAPABILITIES = {
  version: '2026.09.05', yt_dlp: '2026.8.30.232658.dev0', url_prefix: PREFIX, boot_id: BOOT,
  protocol: {
    v2: true, v1_shim: true, socketio: false, ws_path: 'ws', ws_subprotocol: 'aulos.v2',
    batch_ms: 250, urgent_ms: 25, delta_semantics: 'absent-key-means-unchanged',
  },
  features: ['async_add', 'stable_ids', 'deltas', 'since_resume', 'etag', 'retry', 'cancel',
    'cancel_resolve', 'groups', 'subscriptions', 'file_serving', 'batch_add',
    'postprocessing_status', 'per_url_catalog'],
  actions: ['start', 'pause', 'cancel', 'retry', 'delete'],
  formats: [
    { id: 'any', text: 'Any', download_type: 'video', qualities: q(['best', 'Best'], ['2160', '2160p'], ['1440', '1440p'], ['1080', '1080p'], ['720', '720p'], ['480', '480p'], ['360', '360p'], ['240', '240p'], ['worst', 'Worst']) },
    { id: 'mp4', text: 'MP4', download_type: 'video', qualities: q(['best', 'Best'], ['best_remux', 'Best (remux)'], ['2160', '2160p'], ['1440', '1440p'], ['1080', '1080p'], ['720', '720p'], ['480', '480p'], ['360', '360p'], ['240', '240p'], ['worst', 'Worst']) },
    { id: 'ios', text: 'iOS', download_type: 'video', qualities: q(['best', 'Best'], ['2160', '2160p'], ['1440', '1440p'], ['1080', '1080p'], ['720', '720p'], ['480', '480p'], ['360', '360p'], ['240', '240p'], ['worst', 'Worst']) },
    { id: 'm4a', text: 'M4A', download_type: 'audio', qualities: q(['best', 'Best'], ['192', '192 kbps'], ['128', '128 kbps']) },
    { id: 'mp3', text: 'MP3', download_type: 'audio', qualities: q(['best', 'Best'], ['320', '320 kbps'], ['192', '192 kbps'], ['128', '128 kbps']) },
    { id: 'opus', text: 'Opus', download_type: 'audio', qualities: q(['best', 'Best']) },
    { id: 'wav', text: 'WAV', download_type: 'audio', qualities: q(['best', 'Best']) },
    { id: 'flac', text: 'FLAC', download_type: 'audio', qualities: q(['best', 'Best']) },
    { id: 'srt', text: 'SRT', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'txt', text: 'Text', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'vtt', text: 'VTT', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'ttml', text: 'TTML', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'sbv', text: 'SBV', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'scc', text: 'SCC', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'dfxp', text: 'DFXP', download_type: 'captions', qualities: q(['best', 'Best']) },
    { id: 'jpg', text: 'Thumbnail', download_type: 'thumbnail', qualities: q(['best', 'Best']) },
  ],
  download_types: ['video', 'audio', 'captions', 'thumbnail'],
  codecs: ['auto', 'h264', 'h265', 'av1', 'vp9'],
  subtitle_modes: ['auto_only', 'manual_only', 'prefer_manual', 'prefer_auto'],
  presets: ['sponsorblock', 'archive'],
  providers: [{ id: 'ytdlp', state: 'ready', fallback: true }, { id: 'streamingcommunity', state: 'ready', slots: 1 }],
  config: {
    custom_dirs: true, create_custom_dirs: true, allow_ytdl_options_overrides: false,
    default_option_playlist_item_limit: 0, subscription_default_check_interval: 60,
    output_template_chapter: '%(title)s - %(section_number)02d - %(section_title)s.%(ext)s',
    public_host_url: 'download/', public_host_audio_url: 'audio_download/', default_theme: THEME,
    max_concurrent_downloads: 3, delete_file_on_trashcan: false, clear_completed_after: 0,
    default_download_type: 'video', default_format: 'mp4', default_quality: 'best',
  },
};

function q(...pairs) { return pairs.map(([id, text]) => ({ id, text })); }

function catalogFor(url) {
  const sc = /streamingcommunity/i.test(url || '');
  if (sc) {
    return {
      etag: 'sc0001', provider: 'streamingcommunity', match: { score: 200, reason: 'host_contains' },
      runner_up: { provider: 'ytdlp', score: 1 }, naming: 'provider',
      download_types: [{
        id: 'video', label: 'Video', default_format: 'mp4', options: [],
        formats: [{
          id: 'mp4', label: 'MP4', default_quality: 'best',
          notice: 'StreamingCommunity serves one source rendition; quality is ignored.',
          flags: { advisory: true, requires_ffmpeg: true, lossy_remux: false, slow: false },
          qualities: [{ id: 'best', label: 'Source', notice: null }], codecs: [],
        }],
      }],
    };
  }
  const heights = [['best', 'Best'], ['2160', '2160p'], ['1440', '1440p'], ['1080', '1080p'], ['720', '720p'], ['480', '480p'], ['360', '360p'], ['240', '240p'], ['worst', 'Worst']];
  const q1 = (pair) => ({ id: pair[0], label: pair[1], notice: pair[0] === 'worst' ? 'This selector currently resolves to the best available stream' : null });
  // `best` first, then any format-specific extra, then the height ladder — the §8 order.
  const qs = (extra = []) => [q1(heights[0])].concat(extra, heights.slice(1).map(q1));
  const flags = (over = {}) => Object.assign({ advisory: false, requires_ffmpeg: true, lossy_remux: false, slow: false }, over);
  const codecs = [{ id: 'auto', label: 'Auto' }, { id: 'h264', label: 'H.264' }, { id: 'h265', label: 'H.265' }, { id: 'av1', label: 'AV1' }, { id: 'vp9', label: 'VP9' }];
  return {
    etag: '9f2b41c0d7e5a318', provider: url ? 'ytdlp' : 'merged',
    match: url ? { score: 1, reason: 'fallback' } : null,
    runner_up: null, naming: 'template',
    download_types: [
      {
        id: 'video', label: 'Video', default_format: 'mp4', options: [],
        formats: [
          { id: 'any', label: 'Any', default_quality: 'best', notice: null, flags: flags(), qualities: qs(), codecs },
          { id: 'mp4', label: 'MP4', default_quality: 'best', notice: null, flags: flags(), codecs, qualities: qs([{ id: 'best_remux', label: 'Best (remux)', notice: 'Re-encodes audio after download (slower; fixes SponsorBlock drift)' }]) },
          { id: 'ios', label: 'iOS', default_quality: 'best', notice: null, flags: flags(), qualities: qs(), codecs },
        ],
      },
      {
        id: 'audio', label: 'Audio', default_format: 'm4a', options: [],
        formats: [
          { id: 'm4a', label: 'M4A', default_quality: 'best', notice: null, flags: flags(), codecs: [], qualities: [{ id: 'best', label: 'Best', notice: null }, { id: '192', label: '192 kbps', notice: null }, { id: '128', label: '128 kbps', notice: null }] },
          { id: 'mp3', label: 'MP3', default_quality: 'best', notice: null, flags: flags(), codecs: [], qualities: [{ id: 'best', label: 'Best', notice: null }, { id: '320', label: '320 kbps', notice: null }, { id: '192', label: '192 kbps', notice: null }, { id: '128', label: '128 kbps', notice: null }] },
        ],
      },
      { id: 'captions', label: 'Captions', default_format: 'srt', options: [], formats: [{ id: 'srt', label: 'SRT', default_quality: 'best', notice: null, flags: flags(), codecs: [], qualities: [{ id: 'best', label: 'Best', notice: null }] }] },
      { id: 'thumbnail', label: 'Thumbnail', default_format: 'jpg', options: [], formats: [{ id: 'jpg', label: 'Thumbnail', default_quality: 'best', notice: null, flags: flags(), codecs: [], qualities: [{ id: 'best', label: 'Best', notice: null }] }] },
    ],
  };
}

/* ---------------------------------------------------------- ws plumbing */

const sockets = new Set();

/* The replay window §6.2 talks about: the last `REPLAY` frames, so a reconnect carrying
   `?since=&boot=` can be answered with `resume` + a fold instead of a whole new snapshot. */
const REPLAY = 512;
const ring = [];

function send(frame) {
  frame.seq = ++seq;
  ring.push(frame);
  if (ring.length > REPLAY) ring.shift();
  const text = JSON.stringify(frame);
  for (const s of sockets) { if (s.readyState === 1) s.send(text); }
  return frame.seq;
}

/**
 * §6.3: fold everything after `since` into at most one `added`, one `completed`, one `removed`
 * per reason and one `delta`, in that order. Last value wins per `(id, field)`; an `added` that
 * was later removed is dropped; a `completed` supersedes an earlier delta for the same id.
 */
function foldSince(since) {
  const added = new Map(), completed = new Map(), removed = new Map(), delta = new Map();
  for (const f of ring) {
    if (f.seq <= since) continue;
    if (f.t === 'added') for (const it of f.items) { added.set(it.id, it); delta.delete(it.id); }
    else if (f.t === 'completed') for (const it of f.items) { completed.set(it.id, it); added.delete(it.id); delta.delete(it.id); }
    else if (f.t === 'removed') {
      const bucket = removed.get(f.reason) || [];
      for (const id of f.ids) { bucket.push(id); added.delete(id); completed.delete(id); delta.delete(id); }
      removed.set(f.reason, bucket);
    } else if (f.t === 'delta') {
      for (const patch of f.items) {
        if (added.has(patch.id) || completed.has(patch.id)) { Object.assign(added.get(patch.id) || completed.get(patch.id), patch); continue; }
        delta.set(patch.id, Object.assign(delta.get(patch.id) || { id: patch.id }, patch));
      }
    }
  }
  const removedIds = [...removed.values()].reduce((n, ids) => n + ids.length, 0);
  return { added, completed, removed, delta, removedIds };
}

/** Answers one upgrade: `resume` + the fold when the cursor is resumable, else a snapshot. */
function greet(sock, since, boot) {
  const resumable = boot === BOOT && Number.isFinite(since) && since > 0
    && since <= seq && (!ring.length || since >= ring[0].seq - 1);
  if (!resumable) { sock.send(JSON.stringify(snapshot())); return 'snapshot'; }
  const f = foldSince(since);
  const out = [{
    t: 'resume', seq: ++seq, from: since, to: seq,
    merged: { added: f.added.size, completed: f.completed.size, removed: f.removedIds, delta_items: f.delta.size },
  }];
  if (f.added.size) out.push({ t: 'added', seq: ++seq, reason: 'created', items: [...f.added.values()] });
  if (f.completed.size) out.push({ t: 'completed', seq: ++seq, items: [...f.completed.values()] });
  for (const reason of ['deleted', 'cleared', 'auto_cleared', 'group_cascade']) {
    if (f.removed.has(reason)) out.push({ t: 'removed', seq: ++seq, ids: f.removed.get(reason), reason });
  }
  if (f.delta.size) out.push({ t: 'delta', seq: ++seq, ts: Date.now(), items: [...f.delta.values()] });
  out[0].to = seq;
  for (const frame of out) sock.send(JSON.stringify(frame));
  return 'resume';
}

function snapshot() {
  const all = [...items.values()].sort((a, b) => a.ord - b.ord);
  const live = all.filter((i) => !['finished', 'error', 'canceled'].includes(i.status) || i.group_id);
  const done = all.filter((i) => ['finished', 'error', 'canceled'].includes(i.status) && !i.group_id);
  const counts = {};
  for (const s of ['queued', 'resolving', 'preparing', 'downloading', 'postprocessing', 'finished', 'error', 'canceled']) {
    counts[s] = all.filter((i) => i.status === s).length;
  }
  return {
    t: 'snapshot', seq: ++seq, boot_id: BOOT, server_time: Date.now(),
    server: { version: CAPABILITIES.version, yt_dlp: CAPABILITIES.yt_dlp, url_prefix: PREFIX, started_at: now - 86400000 },
    protocol: { batch_ms: 250, urgent_ms: 25, replay_frames: 512, delta_semantics: 'absent-key-means-unchanged' },
    counts, done_total: done.length + OLDER.length,
    truncated: { done: true, groups: all.filter((i) => i.children_inline === false).map((i) => i.id) },
    items: live, done, subscriptions: [],
    ytdl_options: { ok: true, msg: '', update_time: now / 1000 },
    health: { status: 'ok', components: { pot: 'ok', store: 'ok', ytdl_options: 'ok' } },
  };
}

/* Scripted progress: one delta every 250 ms, exactly the server's default cadence. */
let tick = 0;
function advance() {
  tick++;
  const patch = [];
  const dl = items.get(IDS.dl);
  if (dl && dl.status === 'downloading') {
    dl.percent = Math.min(99.4, dl.percent + 0.7);
    dl.downloaded_bytes = Math.round(dl.total_bytes * dl.percent / 100);
    dl.speed = 2000000 + (tick % 7) * 60000;
    dl.eta = Math.max(1, Math.round((dl.total_bytes - dl.downloaded_bytes) / dl.speed));
    patch.push({ id: dl.id, percent: dl.percent, speed: dl.speed, eta: dl.eta, downloaded_bytes: dl.downloaded_bytes });
  }
  const c2 = items.get(IDS.c2);
  if (c2 && c2.status === 'downloading') {
    c2.percent = Math.min(99.5, c2.percent + 0.9);
    patch.push({ id: c2.id, percent: c2.percent });
  }
  const g = items.get(IDS.group);
  if (g && g.status === 'downloading') {
    g.percent = Math.min(99, g.percent + 0.2);
    patch.push({ id: g.id, percent: g.percent, children_active: g.children_active });
  }
  const pp = items.get(IDS.pp);
  if (pp && pp.status === 'postprocessing') {
    pp.phase_percent = (pp.phase_percent + 1.5) % 100;
    patch.push({ id: pp.id, phase_percent: pp.phase_percent });
  }
  if (patch.length) send({ t: 'delta', ts: Date.now(), items: patch });

  // The resolving row gets its real title a beat later, as a prompt text delta (§5.4).
  const r = items.get(IDS.resolving);
  if (r && tick === 3 && r.status === 'resolving') {
    r.title = 'Lo-fi beats — resolved';
    send({ t: 'delta', ts: Date.now(), items: [{ id: r.id, title: r.title }] });
  }
  // …and a beat after that it resolves into a group: §5.5's in-place promotion. One `added` with
  // `reason: "expanded"`, the same id and the same `ord`, `kind` flipped, and NO `removed`.
  if (r && tick === 8 && r.status === 'resolving') expand(r);
}

/** §5.5: promote `g` to a group and deliver it with its first children in one `added` frame. */
function expand(g) {
  Object.assign(g, {
    kind: 'group', status: 'downloading', provider: 'ytdlp', percent: 0,
    children_total: 3, children_done: 0, children_error: 0, children_active: 1, children_inline: true,
  });
  const kids = Array.from({ length: 3 }, (_, i) => item({
    id: ULID(`PKID${String(i).padStart(2, '0')}Z`), ord: g.ord + 1 + i, group_id: g.id, group_index: i + 1,
    title: `Track ${i + 1} — Lo-fi beats`, status: i === 0 ? 'downloading' : 'queued',
    percent: i === 0 ? 15 : 0, provider: 'ytdlp',
  }));
  for (const k of kids) items.set(k.id, k);
  send({ t: 'added', reason: 'expanded', items: [g, ...kids] });
}

/* --------------------------------------------------------------- routes */

const sha = (b) => createHash('sha256').update(b).digest('hex');

const FILES = {
  'assets/app.css': ['text/css; charset=utf-8', () => readFileSync(join(WEB, 'app.css'))],
  'assets/app.js': ['text/javascript; charset=utf-8', () => readFileSync(join(WEB, 'app.js'))],
  'assets/icon.svg': ['image/svg+xml', () => readFileSync(join(WEB, 'icon.svg'))],
  'assets/icon-180.png': ['image/png', () => readFileSync(join(WEB, 'icon-180.png'))],
  'manifest.webmanifest': ['application/manifest+json', () => readFileSync(join(WEB, 'manifest.webmanifest'))],
};

function indexHtml() {
  return Buffer.from(
    readFileSync(join(WEB, 'index.html'), 'utf8')
      .split('{{PREFIX}}').join(PREFIX)
      .split('{{THEME}}').join(THEME),
    'utf8',
  );
}

function sendStatic(req, res, body, ctype, csp) {
  const etag = `"${sha(body)}"`;
  res.setHeader('Content-Type', ctype);
  res.setHeader('ETag', etag);
  res.setHeader('Cache-Control', 'no-cache');
  res.setHeader('X-Content-Type-Options', 'nosniff');
  res.setHeader('Referrer-Policy', 'no-referrer');
  if (csp) res.setHeader('Content-Security-Policy', CSP);
  if (req.headers['if-none-match'] === etag) { res.writeHead(304); res.end(); return; }
  res.writeHead(200);
  res.end(req.method === 'HEAD' ? undefined : body);
}

function json(res, code, body, extra) {
  const text = JSON.stringify(body);
  res.writeHead(code, Object.assign({
    'Content-Type': 'application/json; charset=utf-8',
    'X-Request-Id': ULID('REQ'),
    'X-Aulos-Seq': String(seq),
  }, extra));
  res.end(text);
}

function fail(res, code, wire) {
  json(res, code, { error: Object.assign({ code: 'bad_request', message: '', field: null, provider: null, provider_code: null, request_id: ULID('REQ') }, wire) });
}

function authed(req) {
  if (!TOKEN) return true;
  const h = req.headers.authorization || '';
  if (h === `Bearer ${TOKEN}`) return true;
  // §1.4: on the upgrade the token may also ride in the subprotocol list as `bearer.<token>`.
  const offered = (req.headers['sec-websocket-protocol'] || '').split(',').map((p) => p.trim());
  if (offered.includes(`bearer.${TOKEN}`)) return true;
  const u = new URL(req.url, 'http://x');
  return u.searchParams.get('token') === TOKEN;
}

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  if (!chunks.length) return {};
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { return {}; }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  if (!url.pathname.startsWith(PREFIX)) { fail(res, 404, { code: 'not_found', message: 'no route' }); return; }
  const path = url.pathname.slice(PREFIX.length);

  // The UI, its assets and the manifest are served without auth, always.
  if (path === '') {
    if ((req.headers.accept || '').includes('text/html')) { sendStatic(req, res, indexHtml(), 'text/html; charset=utf-8', true); return; }
    json(res, 200, { name: 'aulos-server', version: CAPABILITIES.version, url_prefix: PREFIX, protocol: 'v2' });
    return;
  }
  if (FILES[path]) { const [ctype, load] = FILES[path]; sendStatic(req, res, load(), ctype, false); return; }

  // Drops every socket without stopping the server, so the page reconnects with ?since=&boot=
  // and the two §6.2 outcomes can both be exercised. `?reboot=1` rotates `boot_id` first, which
  // is what forces the snapshot fallback. A change lands during the gap so the fold is non-empty.
  if (path === '__test/kick') {
    if (url.searchParams.get('reboot')) { BOOT = ULID('BOO2'); ring.length = 0; CAPABILITIES.boot_id = BOOT; }
    for (const sock of sockets) sock.close(1001, 'kicked');
    setTimeout(() => {
      const dl = items.get(IDS.dl);
      if (dl) { dl.title = 'Changed while away'; send({ t: 'delta', ts: Date.now(), items: [{ id: dl.id, title: dl.title }] }); }
      const err = items.get(IDS.err);
      if (err) { items.delete(err.id); send({ t: 'removed', ids: [err.id], reason: 'cleared' }); }
    }, 120);
    json(res, 200, { ok: true, boot_id: BOOT, seq });
    return;
  }

  if (path === '__test/log') { json(res, 200, log); return; }
  if (path === '__test/reset') { log.length = 0; seed(); json(res, 200, { ok: true }); return; }

  if (!authed(req)) { fail(res, 401, { code: 'unauthorized', message: 'authentication required' }); return; }

  if (req.method === 'GET' && path === 'api/v2/capabilities') { json(res, 200, CAPABILITIES); return; }
  if (req.method === 'GET' && path === 'api/v2/custom-dirs') {
    json(res, 200, { download_dir: ['', 'Music', 'Series/Archive'], audio_download_dir: ['', 'Podcasts'] });
    return;
  }
  if (req.method === 'GET' && path === 'api/v2/catalog') { json(res, 200, catalogFor(url.searchParams.get('url'))); return; }

  if (req.method === 'GET' && path === 'api/v2/items') {
    const groupId = url.searchParams.get('group_id');
    const status = (url.searchParams.get('status') || '').split(',').filter(Boolean);
    let list;
    if (groupId === IDS.big) list = BIG_KIDS.map((k) => k.make());
    else if (groupId) list = [...items.values()].filter((i) => i.group_id === groupId);
    else {
      const from = Number(url.searchParams.get('cursor') || 0);
      list = OLDER.filter((i) => !status.length || status.includes(i.status)).slice(from, from + 6);
      const nextFrom = from + 6;
      json(res, 200, { items: list, next_cursor: nextFrom < OLDER.length ? String(nextFrom) : null, total: OLDER.length, seq });
      return;
    }
    json(res, 200, { items: list.sort((a, b) => a.ord - b.ord), next_cursor: null, total: list.length, seq });
    return;
  }

  if (req.method === 'POST' && path === 'api/v2/downloads') {
    const body = await readBody(req);
    log.push({ method: 'POST', path, body });
    if (!body.url || !/^https?:\/\//i.test(String(body.url))) {
      fail(res, 400, { code: 'validation_failed', message: 'url must be an absolute http(s) URL', field: 'url' });
      return;
    }
    const id = ULID(`ADD${String(log.length).padStart(3, '0')}Z`);
    const it = item({
      id, ord: 200 + log.length, url: body.url, title: body.url, status: 'resolving',
      auto_start: body.auto_start !== false, provider: null,
      selection: {
        download_type: body.download_type || 'video', codec: body.codec || 'auto',
        format: body.format || 'mp4', quality: body.quality || 'best',
      },
      folder: body.folder || null, created_at: Date.now(),
    });
    items.set(id, it);
    const s = send({ t: 'added', reason: 'created', items: [it] });
    json(res, 202, { id, ids: [id], generation: ++generation, seq: s, duplicates: [], warnings: [] });
    return;
  }

  if (req.method === 'POST' && path === 'api/v2/items/actions') {
    const body = await readBody(req);
    log.push({ method: 'POST', path, body });
    const ids = Array.isArray(body.ids) ? body.ids : [];
    const applied = [], skipped = [], patches = [], completed = [], removed = [], retried = [];
    for (const id of ids) {
      const it = items.get(id);
      if (!it) { skipped.push({ id, reason: 'not_found' }); continue; }
      const terminal = ['finished', 'error', 'canceled'].includes(it.status);
      switch (body.action) {
        case 'start':
        case 'retry':
          if (terminal) {
            it.status = 'queued'; it.auto_start = true; it.attempt += 1; it.error = null; it.finished_at = null;
            retried.push(it);                     // §5.5: a requeue is an `added`, not a delta
          } else {
            it.auto_start = true;
            patches.push({ id, status: it.status, auto_start: true, error: it.error });
          }
          applied.push(id);
          break;
        case 'pause':
          if (it.status === 'resolving' || terminal) { skipped.push({ id, reason: 'not_pausable' }); break; }
          it.status = 'queued'; it.auto_start = false; it.speed = null; it.eta = null;
          patches.push({ id, status: 'queued', auto_start: false, speed: null, eta: null });
          applied.push(id);
          break;
        case 'cancel':
          if (terminal) { skipped.push({ id, reason: 'already_terminal' }); break; }
          it.status = 'canceled'; it.speed = null; it.eta = null; it.finished_at = Date.now();
          completed.push(it);
          applied.push(id);
          break;
        case 'delete':
          items.delete(id);
          removed.push(id);
          applied.push(id);
          break;
        default:
          skipped.push({ id, reason: 'not_found' });
      }
    }
    // §6.3's flush order: added, completed, removed, delta.
    if (retried.length) send({ t: 'added', reason: 'retried', items: retried });
    if (completed.length) send({ t: 'completed', items: completed });
    if (removed.length) send({ t: 'removed', ids: removed, reason: 'deleted' });
    if (patches.length) send({ t: 'delta', ts: Date.now(), items: patches });
    json(res, 200, { applied, skipped, seq });
    return;
  }

  fail(res, 404, { code: 'not_found', message: `no route for ${req.method} ${url.pathname}` });
});

/* ----------------------------------------------------------- ws upgrade */

const wss = new WebSocketServer({ noServer: true, handleProtocols: (protocols) => (protocols.has('aulos.v2') ? 'aulos.v2' : false) });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname !== `${PREFIX}ws`) { socket.destroy(); return; }
  if (!authed(req)) {
    socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n');
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (sock) => {
    sockets.add(sock);
    sock.on('close', () => sockets.delete(sock));
    sock.on('message', (raw) => {
      let f;
      try { f = JSON.parse(raw.toString()); } catch { return; }
      if (f.t === 'ping') sock.send(JSON.stringify({ t: 'pong', seq: ++seq, server_time: Date.now(), c: f.c }));
    });
    if (FLAP) { sock.close(1013, 'too many clients'); return; }
    greet(sock, Number(url.searchParams.get('since')), url.searchParams.get('boot'));
  });
});

if (!FREEZE) setInterval(advance, 250).unref?.();

server.listen(PORT, '127.0.0.1', () => {
  const p = server.address().port;
  process.stdout.write(`LISTENING ${p}\n`);
});
