/**
 * Playwright smoke for the page in `crates/aulos-api/web/`, driven by `mock-server.mjs`.
 *
 *   npm ci && npx playwright install chromium && npx playwright test
 *
 * Every test spawns its own mock on an ephemeral port, so options (prefix, token, frozen
 * progress) are per-test and the suite is order-independent.
 */

import { spawn } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { expect, test } from '@playwright/test';

const HERE = dirname(fileURLToPath(import.meta.url));
const SHOTS = join(HERE, 'screenshots');

/**
 * `AULOS_WEB_BASE` points the suite at a REAL `aulos-server` instead of the mock, e.g.
 *
 *   AULOS_WEB_BASE=http://127.0.0.1:8091/ npx playwright test
 *   AULOS_WEB_BASE=http://127.0.0.1:8091/metube/ npx playwright test
 *
 * A real server has no scripted queue, so every test that asserts on the mock's fixed rows,
 * deltas, request log or `__test/` routes is skipped; what runs is the static/serving subset —
 * the routes, headers, ETag/304, manifest, the 404 envelope, the identity document — plus the
 * two real-server screenshots. The value is a base URL including the URL_PREFIX; a missing
 * trailing `/` is added.
 */
const REAL_BASE = process.env.AULOS_WEB_BASE
  ? process.env.AULOS_WEB_BASE.replace(/\/*$/, '/')
  : '';

/** The reason string a mock-only test is skipped with when `AULOS_WEB_BASE` is set. */
const MOCK_ONLY = 'mock-only: needs the scripted queue of mock-server.mjs';

const ID = (s) => (s + '00000000000000000000000000').slice(0, 26);
const IDS = {
  dl: ID('DL'), group: ID('GRP'), c1: ID('C1'), c2: ID('C2'), c3: ID('C3'),
  pp: ID('PP'), resolving: ID('RES'), waiting: ID('WAIT'), fin: ID('FIN'), err: ID('ERR'),
};
/** The mock's three seeded subscriptions, plus the one its ticker scripts into existence. */
const SUBS = { a: ID('SUBA'), b: ID('SUBB'), c: ID('SUBC'), scripted: ID('SUBS') };

async function startMock(opts = {}) {
  const args = ['mock-server.mjs', '--port', String(opts.port ?? 0)];
  if (opts.prefix) args.push('--prefix', opts.prefix);
  if (opts.theme) args.push('--theme', opts.theme);
  if (opts.token) args.push('--token', opts.token);
  if (opts.freeze) args.push('--freeze');
  if (opts['big-group']) args.push('--big-group');
  if (opts.flap) args.push('--flap');
  const child = spawn(process.execPath, args, { cwd: HERE, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stderr.on('data', (d) => process.stderr.write(`[mock] ${d}`));
  const port = await new Promise((resolve, reject) => {
    let buf = '';
    const timer = setTimeout(() => reject(new Error('mock server did not start in 10s')), 10_000);
    child.stdout.on('data', (d) => {
      buf += d.toString();
      const m = /LISTENING (\d+)/.exec(buf);
      if (m) { clearTimeout(timer); resolve(Number(m[1])); }
    });
    child.once('exit', (code) => { clearTimeout(timer); reject(new Error(`mock exited with ${code}`)); });
  });
  const prefix = opts.prefix || '/';
  return {
    port,
    prefix,
    base: `http://127.0.0.1:${port}${prefix}`,
    async log(request) { return (await request.get(`http://127.0.0.1:${port}${prefix}__test/log`)).json(); },
    /** Drop every socket (optionally rotating `boot_id`) without stopping the server. */
    async kick(request, { reboot = false } = {}) {
      const q = reboot ? '?reboot=1' : '';
      return (await request.get(`http://127.0.0.1:${port}${prefix}__test/kick${q}`)).json();
    },
    stop() { child.kill('SIGKILL'); },
  };
}

/** Spawn a mock, run `body`, always kill the child. */
function withMock(opts, body) {
  return async ({ page, request }) => {
    test.skip(REAL_BASE !== '', MOCK_ONLY);
    const mock = await startMock(opts);
    try { await body({ page, request, mock }); } finally { mock.stop(); }
  };
}

/** Record the `t` of every frame the page receives, in order, across every socket it opens. */
async function recordFrames(page) {
  const frames = [];
  page.on('websocket', (ws) => {
    ws.on('framereceived', (d) => {
      try { frames.push(JSON.parse(d.payload).t); } catch { /* binary or noise */ }
    });
  });
  return frames;
}

async function open(page, mock) {
  await page.addInitScript(() => {
    window.__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => window.__csp.push(`${e.violatedDirective} ${e.blockedURI}`));
  });
  await page.goto(mock.base);
  await expect(page.locator('#conn-text')).toHaveText('Live');
  await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toBeVisible();
}

/* ------------------------------------------------------------------ tests */

test('snapshot renders the expected sections and rows', withMock({}, async ({ page, mock }) => {
  await open(page, mock);

  await expect(page.locator('#sec-active')).toBeVisible();
  await expect(page.locator('#sec-waiting')).toBeVisible();
  await expect(page.locator('#sec-done')).toBeVisible();

  // In progress: the downloading item, the group, the postprocessing item, the resolving item.
  await expect(page.locator('#rows-active > .row')).toHaveCount(4);
  await expect(page.locator('#rows-waiting > .row')).toHaveCount(1);
  await expect(page.locator('#rows-done > .row')).toHaveCount(2);

  const dl = page.locator(`.row[data-id="${IDS.dl}"]`);
  await expect(dl.locator('.row-title')).toHaveText('Le incredibili elezioni del 2000');
  await expect(dl.locator('.st')).toHaveText('Downloading');
  await expect(dl.locator('.rest')).toContainText('MB/s');
  await expect(dl.locator('.rest')).toContainText('left');

  // The waiting row is the paused one, and it offers Start.
  const wait = page.locator(`.row[data-id="${IDS.waiting}"]`);
  await expect(wait.locator('.st')).toHaveText('Paused');
  await expect(wait.locator('button[data-action="start"]')).toBeVisible();

  // The error row shows error.message in the error colour, and offers a retry.
  const err = page.locator(`.row[data-id="${IDS.err}"]`);
  await expect(err.locator('.rest')).toHaveText('No video formats found');
  await expect(err.locator('.rest')).toHaveClass(/err/);

  // postprocessing shows the primary bar at 100 plus the gold phase bar.
  const pp = page.locator(`.row[data-id="${IDS.pp}"]`);
  await expect(pp.locator('.st')).toHaveText('Post-processing');
  await expect(pp.locator('.bar.ph')).toBeVisible();
  expect(await pp.locator('.bar .fill').first().evaluate((e) => e.style.width)).toBe('100%');

  // The group carries the server-computed counters and expands into inline children.
  const group = page.locator(`.row[data-id="${IDS.group}"]`);
  await expect(group.locator('.rest')).toContainText('4 of 12 done');
  await group.locator('.chev').click();
  await expect(group.locator('.kids .kid')).toHaveCount(3);
  await expect(group.locator('.kids .kid').first()).toContainText('S02E04');

  expect(await page.evaluate(() => window.__csp)).toEqual([]);
}));

test('a delta patches the row in place and never replaces the element', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const row = page.locator(`.row[data-id="${IDS.dl}"]`);
  const before = await row.elementHandle();
  const width0 = await row.locator('.bar .fill').first().evaluate((e) => e.style.width);
  const rest0 = await row.locator('.rest').textContent();

  await page.waitForFunction(
    ([id, w]) => document.querySelector(`.row[data-id="${id}"] .fill`).style.width !== w,
    [IDS.dl, width0],
    { timeout: 10_000 },
  );

  const after = await row.elementHandle();
  expect(await before.evaluate((el, other) => el === other, after)).toBe(true);
  expect(await row.locator('.rest').textContent()).not.toBe(rest0);
  await expect(row.locator('.st')).toHaveText('Downloading');
}));

test('the resolving row gets its title from a delta', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const row = page.locator(`.row[data-id="${IDS.resolving}"]`);
  await expect(row.locator('.st')).toHaveText('Resolving');
  await expect(row.locator('.row-title')).toHaveText('Lo-fi beats — resolved', { timeout: 10_000 });
}));

test('add posts the §4.1 body and the row appears from the added frame', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);
  const url = 'https://www.youtube.com/watch?v=NEWONE';
  await page.fill('#url', url);
  await page.waitForTimeout(500);          // let the debounced catalog refine the picker first
  await page.click('#add-btn');

  await expect.poll(async () => (await mock.log(request)).length).toBeGreaterThan(0);
  const [entry] = await mock.log(request);
  expect(entry.path).toBe('api/v2/downloads');
  expect(entry.body).toEqual({
    url,
    download_type: 'video',
    format: 'mp4',
    quality: 'best',
    codec: 'auto',
    folder: null,
    auto_start: true,
  });

  await expect(page.locator('#rows-active > .row').filter({ hasText: url })).toBeVisible();
  await expect(page.locator('#url')).toHaveValue('');
}));

test('every action button posts the action PROTOCOL §4.2 names', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);

  await page.click(`.row[data-id="${IDS.dl}"] button[data-action="pause"]`);
  await page.click(`.row[data-id="${IDS.waiting}"] button[data-action="start"]`);
  // error → the Retry button, which §4.2 says is `start` on a terminal item.
  await page.click(`.row[data-id="${IDS.err}"] button[data-action="start"]`);
  await page.click(`.row[data-id="${IDS.fin}"] button[data-action="delete"]`);

  await expect.poll(async () => (await mock.log(request)).length).toBe(4);
  const log = await mock.log(request);
  expect(log.map((e) => [e.path, e.body.action, e.body.ids])).toEqual([
    ['api/v2/items/actions', 'pause', [IDS.dl]],
    ['api/v2/items/actions', 'start', [IDS.waiting]],
    ['api/v2/items/actions', 'start', [IDS.err]],
    ['api/v2/items/actions', 'delete', [IDS.fin]],
  ]);
  // D3: the shipped page never removes a file from disk, and says so rather than inheriting
  // whatever `DELETE_FILE_ON_TRASHCAN` is set to. Only `delete` carries the key.
  expect(log[3].body).toEqual({ action: 'delete', ids: [IDS.fin], delete_file: false });
  for (const e of log.slice(0, 3)) expect(e.body.delete_file).toBeUndefined();

  // The removed frame took the finished row out; the paused one moved to "Waiting for you".
  await expect(page.locator(`.row[data-id="${IDS.fin}"]`)).toHaveCount(0);
  await expect(page.locator(`#rows-waiting .row[data-id="${IDS.dl}"]`)).toBeVisible();
}));

test('Clear posts §4.7 items/clear with delete_file: false, after a confirm', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);
  page.once('dialog', (d) => d.accept());
  await page.click('#clear');

  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
  const [entry] = await mock.log(request);
  expect(entry.method).toBe('POST');
  expect(entry.path).toBe('api/v2/items/clear');
  expect(entry.body).toEqual({ where: 'done', delete_file: false });
  await expect(page.locator('#sec-done')).toBeHidden();
}));

test('Clear asks first, and a dismissed confirm sends nothing', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);
  page.once('dialog', (d) => d.dismiss());
  await page.click('#clear');
  await page.waitForTimeout(300);
  expect(await mock.log(request)).toEqual([]);
  await expect(page.locator('#sec-done')).toBeVisible();
}));

test('the more menu reaches the actions the row does not show inline', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);
  await page.click(`.row[data-id="${IDS.dl}"] .act-more`);
  await expect(page.locator('#menu')).toBeVisible();
  await page.locator('#menu button', { hasText: 'Delete' }).click();

  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
  const [entry] = await mock.log(request);
  expect(entry.body).toMatchObject({ action: 'delete', ids: [IDS.dl] });
}));

test('Show older pages the completed history', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  await expect(page.locator('#show-older')).toBeVisible();
  await page.click('#show-older');
  await expect(page.locator('#rows-done > .row')).toHaveCount(8);   // 2 in the snapshot + 6 paged
  await page.click('#show-older');
  await expect(page.locator('#rows-done > .row')).toHaveCount(14);
}));

test('a 401 shows the token sheet, and the token is then used on REST and the WS', withMock({ token: 's3cret' }, async ({ page, mock, request }) => {
  const wsUrls = [];
  page.on('websocket', (ws) => wsUrls.push(ws.url()));
  await page.goto(mock.base);
  await expect(page.locator('#token-sheet')).toBeVisible();
  await expect(page.locator('#conn-text')).toHaveText('Sign in');
  await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toHaveCount(0);

  await page.fill('#token-input', 's3cret');
  await page.click('#token-save');

  // The WS upgrade carried the token — in the subprotocol list, never in the query string, so a
  // proxy access log never sees it — and the snapshot arrived.
  expect(wsUrls.every((u) => !u.includes('token='))).toBe(true);
  await expect(page.locator('#conn-text')).toHaveText('Live');
  await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toBeVisible();

  // … and REST carries the bearer header, or this mutation would have been a 401.
  await page.click(`.row[data-id="${IDS.dl}"] button[data-action="pause"]`);
  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
  expect((await mock.log(request))[0].body.action).toBe('pause');

  // The token survives a reload.
  await page.reload();
  await expect(page.locator('#conn-text')).toHaveText('Live');
  await expect(page.locator('#token-sheet')).toBeHidden();
}));

test('a non-root URL_PREFIX works for the assets, the API and the WebSocket', withMock({ prefix: '/metube/' }, async ({ page, mock, request }) => {
  const assets = [];
  page.on('response', (r) => { if (r.url().includes('/assets/')) assets.push(new URL(r.url()).pathname); });

  await open(page, mock);
  expect(assets).toContain('/metube/assets/app.css');
  expect(assets).toContain('/metube/assets/app.js');
  expect(await page.evaluate(() => window.__aulos.PREFIX)).toBe('/metube/');

  const manifest = await request.get(`${mock.base}manifest.webmanifest`);
  expect(manifest.headers()['content-type']).toContain('application/manifest+json');

  // The WS is live (the rows are there) and REST paths are prefixed too.
  await page.click(`.row[data-id="${IDS.dl}"] button[data-action="pause"]`);
  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
}));

test('index.html carries the contract headers and answers If-None-Match with 304', withMock({}, async ({ mock, request }) => {
  const res = await request.get(mock.base, { headers: { accept: 'text/html' } });
  const h = res.headers();
  expect(h['content-type']).toContain('text/html');
  expect(h['cache-control']).toBe('no-cache');
  expect(h['x-content-type-options']).toBe('nosniff');
  expect(h['referrer-policy']).toBe('no-referrer');
  expect(h['content-security-policy']).toBe(
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
    "connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri 'none'; " +
    "form-action 'none'; frame-ancestors 'none'",
  );
  expect(h.etag).toMatch(/^"[0-9a-f]{64}"$/);
  const again = await request.get(mock.base, { headers: { accept: 'text/html', 'if-none-match': h.etag } });
  expect(again.status()).toBe(304);

  // Without `Accept: text/html` the same route is still the JSON identity document.
  const identity = await request.get(mock.base, { headers: { accept: 'application/json' } });
  expect((await identity.json()).name).toBe('aulos-server');
}));

test('phone 390x844: no horizontal overflow, the sheet opens, targets are ≥ 44px', withMock({}, async ({ page, mock }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await open(page, mock);

  expect(await page.evaluate(() => document.scrollingElement.scrollWidth)).toBeLessThanOrEqual(390);

  // Only the primary action and the ⋯ button survive the collapse.
  const acts = page.locator(`.row[data-id="${IDS.dl}"] .row-acts .iconbtn:visible`);
  await expect(acts).toHaveCount(2);

  await page.click('#url');
  await expect(page.locator('#sheet')).toBeVisible();
  await expect(page.locator('#sheet-type button')).toHaveCount(4);
  await expect(page.locator('#sheet-quality .chip').first()).toBeVisible();
  expect(await page.evaluate(() => document.scrollingElement.scrollWidth)).toBeLessThanOrEqual(390);

  // Measured *after* the sheet is open, or the chips and segments sit inside `[hidden]` and the
  // assertion is vacuous. The count is pinned for the same reason.
  const targets = await page.evaluate(() => {
    const out = [];
    for (const el of document.querySelectorAll('.iconbtn, .btn-primary, .chip, .seg button, .text-btn, .link, .danger-link, .sw')) {
      const r = el.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) continue;
      out.push({ cls: el.className || el.id, h: Math.round(r.height), w: Math.round(r.width) });
    }
    return out;
  });
  expect(targets.length, JSON.stringify(targets)).toBeGreaterThanOrEqual(20);
  for (const t of targets) expect(t.h, `${t.cls} height`).toBeGreaterThanOrEqual(44);
}));

test('every focusable control shows a focus ring', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const rings = await page.evaluate(() => {
    const out = {};
    // The sheets are display:none until opened, and a hidden control cannot take focus.
    for (const sheet of ['sheet', 'token-sheet']) document.getElementById(sheet).hidden = false;
    for (const id of ['url', 'type', 'quality', 'sheet-url', 'sheet-format', 'token-input']) {
      const el = document.getElementById(id);
      el.focus();
      // The ring may be on the control or on the `.field` wrapper that contains it.
      const own = getComputedStyle(el).outlineStyle;
      const wrap = el.closest('.field');
      out[id] = own !== 'none' ? own : (wrap ? getComputedStyle(wrap).outlineStyle : 'none');
    }
    return out;
  });
  for (const [id, style] of Object.entries(rings)) expect(style, `#${id} focus ring`).not.toBe('none');
}));

test('the add sheet is a real modal: inert behind, Tab trapped, focus restored', withMock({}, async ({ page, mock }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await open(page, mock);

  await page.focus('#add-btn');
  await page.click('#add-btn');
  await expect(page.locator('#sheet')).toBeVisible();
  expect(await page.evaluate(() => document.querySelector('main.wrap').inert)).toBe(true);
  expect(await page.evaluate(() => document.querySelector('header.hdr').getAttribute('aria-hidden'))).toBe('true');

  // Tab all the way round: focus never leaves the sheet.
  for (let i = 0; i < 25; i++) {
    await page.keyboard.press('Tab');
    expect(await page.evaluate(() => !!document.getElementById('sheet').contains(document.activeElement)), `tab ${i}`).toBe(true);
  }
  await page.keyboard.press('Shift+Tab');
  expect(await page.evaluate(() => document.getElementById('sheet').contains(document.activeElement))).toBe(true);

  await page.keyboard.press('Escape');
  await expect(page.locator('#sheet')).toBeHidden();
  expect(await page.evaluate(() => document.querySelector('main.wrap').inert)).toBe(false);
  expect(await page.evaluate(() => document.querySelector('header.hdr').hasAttribute('aria-hidden'))).toBe(false);
  expect(await page.evaluate(() => document.activeElement.id)).toBe('add-btn');
}));

test('320px wide still has no horizontal scroll', withMock({}, async ({ page, mock }) => {
  await page.setViewportSize({ width: 320, height: 700 });
  await open(page, mock);
  expect(await page.evaluate(() => document.scrollingElement.scrollWidth)).toBeLessThanOrEqual(320);
}));

test('50 active rows and 20 deltas stay inside the frame budget', withMock({ freeze: true }, async ({ page, mock }) => {
  await open(page, mock);

  const result = await page.evaluate(async () => {
    const A = window.__aulos;
    const seed = A.state.items.get([...A.state.items.keys()].find((k) => A.state.items.get(k).status === 'downloading'));
    const ids = [];
    const rows = [];
    for (let i = 0; i < 50; i++) {
      const id = `PERF${String(i).padStart(22, '0')}`;
      ids.push(id);
      rows.push({ ...seed, id, ord: 5000 + i, title: `Perf row ${i}`, percent: 0, downloaded_bytes: 0 });
    }
    A.applyFrame({ t: 'added', seq: A.state.seq + 1, reason: 'created', items: rows });
    A.flushNow();

    const longs = [];
    let po = null;
    try {
      po = new PerformanceObserver((list) => { for (const e of list.getEntries()) longs.push(e.duration); });
      po.observe({ entryTypes: ['longtask'] });
    } catch { /* longtask unsupported: the tick timings below still gate the budget */ }

    const first = document.querySelector(`.row[data-id="${ids[0]}"]`);
    const ticks = [];
    for (let k = 1; k <= 20; k++) {
      A.applyFrame({
        t: 'delta',
        seq: A.state.seq + 1,
        items: ids.map((id, i) => ({
          id,
          percent: (k * 4 + i) % 100,
          speed: 1_000_000 + k * 1000,
          eta: 300 - k,
          downloaded_bytes: k * 1_000_000 + i,
        })),
      });
      const t0 = performance.now();
      A.flushNow();
      ticks.push(performance.now() - t0);
    }
    await new Promise((r) => setTimeout(r, 150));
    if (po) po.disconnect();

    return {
      longs,
      maxTick: Math.max(...ticks),
      avgTick: ticks.reduce((a, b) => a + b, 0) / ticks.length,
      sameElement: first === document.querySelector(`.row[data-id="${ids[0]}"]`),
      rendered: document.querySelectorAll('#rows-active > .row').length,
    };
  });

  expect(result.rendered).toBe(54);
  expect(result.sameElement).toBe(true);
  expect(result.longs.filter((d) => d > 50)).toEqual([]);
  expect(result.maxTick).toBeLessThan(50);
  expect(result.avgTick).toBeLessThan(16);
}));

test('a dropped socket reconnects with backoff and the pill tracks it', async ({ page }) => {
  test.skip(REAL_BASE !== '', MOCK_ONLY);
  let mock = await startMock();
  const port = mock.port;
  try {
    await open(page, mock);
    mock.stop();
    await expect(page.locator('#conn-text')).toHaveText('Reconnecting…', { timeout: 15_000 });

    mock = await startMock({ port });                       // same port, a new boot_id
    await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 25_000 });
    await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toBeVisible();
  } finally {
    mock.stop();
  }
});

test('a reconnect inside the replay window resumes instead of re-snapshotting', withMock({}, async ({ page, mock, request }) => {
  const frames = await recordFrames(page);
  await open(page, mock);
  await expect.poll(() => frames.filter((t) => t === 'snapshot').length).toBe(1);

  // The server drops the socket and, while the page is away, changes one row and clears another.
  await mock.kick(request);
  await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 20_000 });

  // §6.2: `since` was inside the window and `boot` matched, so the answer is `resume`, not a
  // second `snapshot` — and §6.3's fold carried the two changes made during the gap.
  await expect.poll(() => frames.filter((t) => t === 'resume').length, { timeout: 20_000 }).toBe(1);
  await expect(page.locator(`.row[data-id="${IDS.dl}"] .row-title`)).toHaveText('Changed while away');
  await expect(page.locator(`.row[data-id="${IDS.err}"]`)).toHaveCount(0);
  expect(frames.filter((t) => t === 'snapshot').length).toBe(1);
}));

test('a boot_id change forces the §6.2 snapshot fallback', withMock({}, async ({ page, mock, request }) => {
  const frames = await recordFrames(page);
  await open(page, mock);

  await mock.kick(request, { reboot: true });
  await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 20_000 });

  await expect.poll(() => frames.filter((t) => t === 'snapshot').length, { timeout: 20_000 }).toBe(2);
  expect(frames.filter((t) => t === 'resume')).toEqual([]);
  await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toBeVisible();
}));

test('a resolving row is promoted to a group in place, without blinking', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const row = page.locator(`.row[data-id="${IDS.resolving}"]`);
  await expect(row.locator('.st')).toHaveText('Resolving');
  const before = await row.elementHandle();
  const ord = await page.evaluate((id) => window.__aulos.state.items.get(id).ord, IDS.resolving);

  // §5.5: one `added` with the same id and the same ord, `kind` flipped, and no `removed`.
  await expect(row.locator('.chev')).toBeVisible({ timeout: 15_000 });
  const after = await row.elementHandle();
  expect(await before.evaluate((el, other) => el === other, after)).toBe(true);
  expect(await page.evaluate((id) => window.__aulos.state.items.get(id).ord, IDS.resolving)).toBe(ord);
  expect(await page.evaluate((id) => window.__aulos.state.items.get(id).kind, IDS.resolving)).toBe('group');
  await expect(row.locator('.rest')).toContainText('0 of 3 done');

  await row.locator('.chev').click();
  await expect(row.locator('.kids .kid')).toHaveCount(3);
  // The top-level list did not gain a row: the children are nested under the group.
  await expect(page.locator('#rows-active > .row')).toHaveCount(4);
}));

test('a socket the server accepts and closes at once backs off instead of hot-looping', withMock({ flap: true }, async ({ page, mock }) => {
  let upgrades = 0;
  page.on('websocket', () => { upgrades++; });
  const caps = [];
  page.on('request', (r) => { if (r.url().includes('api/v2/capabilities')) caps.push(r.url()); });

  await page.goto(mock.base);
  await expect(page.locator('#conn-text')).toHaveText('Reconnecting…', { timeout: 10_000 });
  await page.waitForTimeout(6000);

  // 500/1000/2000/4000 ms with jitter: at most a handful in six seconds. A backoff reset on
  // `onopen` produced ~12 here, and one capabilities GET per close on top of them.
  expect(upgrades, `${upgrades} upgrades in 6 s`).toBeLessThanOrEqual(6);
  expect(caps.length, `${caps.length} capabilities GETs`).toBeLessThanOrEqual(2);
}));

test('an expanded large group re-fetches its children after a snapshot', withMock({ 'big-group': true }, async ({ page, mock, request }) => {
  await open(page, mock);
  const big = page.locator(`.row[data-id="${ID('BIG')}"]`);
  await big.locator('.chev').click();
  await expect(big.locator('.kids .kid')).toHaveCount(4);

  // §5.11: a fresh snapshot wipes `state.items`, so an open group has to ask again — and the
  // group must not silently sit open reading "No children yet".
  await mock.kick(request, { reboot: true });
  await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 20_000 });
  await expect(big.locator('.chev')).toHaveAttribute('aria-expanded', 'true');
  await expect(big.locator('.kids .kid')).toHaveCount(4, { timeout: 15_000 });
  await expect(big.locator('.kids')).not.toContainText('No children yet');
}));

test('an unknown status renders inert instead of disappearing', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const id = 'UNK00000000000000000000000';
  await page.evaluate((newId) => {
    const A = window.__aulos;
    const seed = A.state.items.get([...A.state.items.keys()][0]);
    A.applyFrame({
      t: 'added', seq: A.state.seq + 1, reason: 'created',
      items: [{ ...seed, id: newId, ord: 950, kind: 'item', group_id: null, title: 'A future status', status: 'archiving', percent: 0, speed: null, eta: null }],
    });
  }, id);

  // §3.1: an unknown value is inert, not invisible.
  const row = page.locator(`.row[data-id="${id}"]`);
  await expect(row).toBeVisible();
  await expect(row.locator('.st')).toHaveText('Archiving');
  expect(await page.evaluate((i) => window.__aulos.state.items.has(i), id)).toBe(true);
  await expect(page.locator('#empty')).toBeHidden();
}));

test('a providers frame keeps the URL-refined picker instead of the generic ladder', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  await page.fill('#url', 'https://streamingcommunity.test/titles/42-series');
  // §4.6: one quality labelled Source, plus the provider's notice.
  await expect(page.locator('#prov')).toContainText('streamingcommunity', { timeout: 10_000 });
  await expect(page.locator('#quality option')).toHaveCount(1);

  await page.evaluate(() => window.__aulos.applyFrame({ t: 'providers', seq: window.__aulos.state.seq + 1, reloaded: [], failed: [] }));

  // §5.9 says refetch capabilities *and* the catalog: the refined picker survives.
  await page.waitForTimeout(600);
  await expect(page.locator('#quality option')).toHaveCount(1);
  await expect(page.locator('#prov')).toContainText('streamingcommunity');
}));

test("switching type picks the catalog's default_format, not formats[0]", withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  // Wait for the per-URL catalog: the capabilities picker declares no `default_format` at all,
  // so asserting before it lands would test the wrong picker.
  const catalog = page.waitForResponse((r) => r.url().includes('api/v2/catalog'));
  await page.fill('#url', 'https://www.youtube.com/watch?v=DEFAULTS');
  await catalog;
  await expect(page.locator('#type option')).toHaveCount(4, { timeout: 10_000 });

  await page.selectOption('#type', 'audio');
  expect(await page.evaluate(() => window.__aulos.add.format)).toBe('m4a');
  // …and back to video, whose declared default is mp4 while `formats[0]` is `any`.
  await page.selectOption('#type', 'video');
  expect(await page.evaluate(() => window.__aulos.add.format)).toBe('mp4');
}));

test('the completed list is windowed, so a long-lived tab cannot grow without bound', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const counts = await page.evaluate(() => {
    const A = window.__aulos;
    const seed = { ...A.state.items.get([...A.state.items.keys()][0]), kind: 'item', group_id: null, status: 'finished', percent: 100, speed: null, eta: null };
    const items = [];
    for (let i = 0; i < 400; i++) items.push({ ...seed, id: `BULK${String(i).padStart(22, '0')}`, ord: 1000 + i, title: `Bulk ${i}` });
    A.applyFrame({ t: 'completed', seq: A.state.seq + 1, items });
    A.flushNow();
    A.flushNow();
    return {
      done: document.querySelectorAll('#rows-done > .row').length,
      rows: A.rows.size,
      items: A.state.items.size,
    };
  });
  expect(counts.done).toBe(200);
  expect(counts.rows).toBeLessThanOrEqual(210);
  expect(counts.items).toBeLessThanOrEqual(215);
  await expect(page.locator('#show-older')).toBeVisible();
}));

test('"Open source" refuses a non-http(s) URL instead of navigating to it', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  await page.evaluate((id) => {
    window.__opened = [];
    window.open = (u) => { window.__opened.push(u); return null; };
    // An imported metube record's `url` is a bare `url::Url` parse, so `javascript:` gets through
    // the store; the page is the thing doing the navigating, so the page is where it stops.
    window.__aulos.state.items.get(id).url = 'javascript:alert(1)';
  }, IDS.fin);

  await page.click(`.row[data-id="${IDS.fin}"] button[data-action="source"]`);
  await expect(page.locator('.toast.error')).toContainText('not a web address');
  expect(await page.evaluate(() => window.__opened)).toEqual([]);

  // …and an ordinary https link still opens.
  await page.click(`.row[data-id="${IDS.dl}"] .act-more`);
  await page.locator('#menu button', { hasText: 'Open source' }).click();
  expect(await page.evaluate(() => window.__opened)).toEqual(['https://www.youtube.com/watch?v=dQw4w9WgXcQ']);
}));

test('a queued not_yet_live item reads as scheduled, not as a failure', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const id = 'NYL00000000000000000000000';
  await page.evaluate((newId) => {
    const A = window.__aulos;
    const seed = A.state.items.get([...A.state.items.keys()][0]);
    A.applyFrame({
      t: 'added',
      seq: A.state.seq + 1,
      reason: 'created',
      items: [{
        ...seed,
        id: newId,
        ord: 900,
        title: 'An upcoming stream',
        status: 'queued',
        auto_start: false,
        percent: 0,
        speed: null,
        eta: null,
        error: { code: 'not_yet_live', message: 'Premieres in 2 hours', field: null, provider: 'ytdlp', provider_code: null },
      }],
    });
  }, id);

  const row = page.locator(`.row[data-id="${id}"]`);
  await expect(row).toBeVisible();
  await expect(row.locator('.st')).toHaveText('Scheduled');
  await expect(row.locator('.rest')).toHaveText('Premieres in 2 hours');
  await expect(row.locator('.rest')).not.toHaveClass(/err/);
  await expect(page.locator(`#rows-waiting .row[data-id="${id}"]`)).toBeVisible();
  await expect(row.locator('.disc')).not.toHaveClass(/err/);
}));

test('a collapsed group fetches its children on expand', withMock({ 'big-group': true }, async ({ page, mock }) => {
  await open(page, mock);
  const big = page.locator(`.row[data-id="${ID('BIG')}"]`);
  await expect(big.locator('.rest')).toContainText('12 of 480 done');
  await expect(big.locator('.kids .kid')).toHaveCount(0);

  const fetched = page.waitForResponse((r) => r.url().includes('group_id='));
  await big.locator('.chev').click();
  await fetched;

  await expect(big.locator('.kids .kid')).toHaveCount(4);
  await expect(big.locator('.kids .kid').first()).toContainText('Episode 1 of a large season');
  // The children are keyed under their group, so they never leak into the top-level list.
  await expect(page.locator('#rows-active > .row')).toHaveCount(5);
}));

test('starting health stays neutral while real failures warn', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const health = async (to) => page.evaluate((to) => {
    const A = window.__aulos;
    A.applyFrame({ t: 'health', seq: A.state.seq + 1, status: to === 'degraded' ? 'degraded' : 'ok',
      changed: [{ component: 'pot', from: 'ok', to }] });
  }, to);
  await health('starting');
  await expect(page.locator('.toast')).toHaveCount(0);
  await health('ok');
  await expect(page.locator('.toast')).toHaveCount(0);
  await health('degraded');
  await expect(page.locator('.toast')).toHaveText(/pot.*degraded/);
}));

test('a notice frame becomes a toast, and errors stay until dismissed', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  await page.evaluate(() => {
    const A = window.__aulos;
    A.applyFrame({ t: 'notice', seq: A.state.seq + 1, level: 'error', code: 'stalled', id: null, message: 'No progress for 900s' });
  });
  const toast = page.locator('.toast.error');
  await expect(toast).toHaveText(/No progress for 900s/);
  await page.waitForTimeout(400);
  await expect(toast).toBeVisible();                       // an error toast does not auto-dismiss
  await toast.locator('button').click();
  await expect(toast).toHaveCount(0);
}));

test('the theme button cycles system → light → dark and persists', withMock({ theme: 'auto' }, async ({ page, mock }) => {
  await open(page, mock);
  const mode = () => page.evaluate(() => document.documentElement.dataset.mode);
  const stored = () => page.evaluate(() => localStorage.getItem('aulos.theme'));

  await page.click('#theme-btn');
  expect(await stored()).toBe('light');
  expect(await mode()).toBe('light');

  await page.click('#theme-btn');
  expect(await stored()).toBe('dark');
  expect(await mode()).toBe('dark');
  expect(await page.getAttribute('meta[name="theme-color"]', 'content')).toBe('#000000');

  await page.reload();
  await expect(page.locator('#conn-text')).toHaveText('Live');
  expect(await mode()).toBe('dark');

  await page.click('#theme-btn');
  expect(await stored()).toBe('auto');
}));

test('DEFAULT_THEME=dark paints dark before the module runs', async ({ browser }) => {
  test.skip(REAL_BASE !== '', MOCK_ONLY);
  const mock = await startMock({ theme: 'dark', freeze: true });
  const ctx = await browser.newContext({ javaScriptEnabled: false, colorScheme: 'light' });
  try {
    const page = await ctx.newPage();
    await page.goto(mock.base);
    // The palette is keyed off `<html data-mode>`, so the theme has to be rendered onto the
    // element and not only into a meta tag app.js reads.
    expect(await page.getAttribute('html', 'data-mode')).toBe('dark');
    expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(0, 0, 0)');
    // …and the installed PWA's toolbar has a dark answer available at first paint too.
    expect(await page.locator('meta[name="theme-color"]').count()).toBe(2);
    expect(await page.getAttribute('meta[name="theme-color"][media*="dark"]', 'content')).toBe('#000000');
  } finally {
    await ctx.close();
    mock.stop();
  }
});

test('DEFAULT_THEME=auto still lets the OS preference decide', async ({ browser }) => {
  test.skip(REAL_BASE !== '', MOCK_ONLY);
  const mock = await startMock({ theme: 'auto', freeze: true });
  const ctx = await browser.newContext({ javaScriptEnabled: false, colorScheme: 'dark' });
  try {
    const page = await ctx.newPage();
    await page.goto(mock.base);
    expect(await page.getAttribute('html', 'data-mode')).toBe('auto');
    expect(await page.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe('rgb(0, 0, 0)');
  } finally {
    await ctx.close();
    mock.stop();
  }
});

/* ---------------------------------------------------------- subscriptions */

test('the snapshot seeds the subscriptions panel', withMock({ freeze: true }, async ({ page, mock }) => {
  await open(page, mock);
  await expect(page.locator('#sec-subs')).toBeVisible();
  await expect(page.locator('#subs-empty')).toBeHidden();
  await expect(page.locator('#rows-subs > .row')).toHaveCount(3);

  const a = page.locator(`.row[data-sub="${SUBS.a}"]`);
  await expect(a.locator('.row-title')).toHaveText('Veritasium');
  await expect(a.locator('.st')).toHaveText('Active');
  await expect(a.locator('.rest')).toContainText('youtube.com');
  await expect(a.locator('.rest')).toContainText('every hour');
  await expect(a.locator('.rest')).toContainText('checked 3 min ago');
  await expect(a.locator('.rest')).toContainText(/due in 5[5-7] min/);
  await expect(a.locator('.rest')).toContainText('317 seen');
  await expect(a.locator('.sw')).toHaveAttribute('aria-checked', 'true');
  await expect(a.locator('.sub-err')).toBeHidden();

  // Disabled: the switch is off, the word says so, and there is no "next due" to promise.
  const b = page.locator(`.row[data-sub="${SUBS.b}"]`);
  await expect(b.locator('.st')).toHaveText('Paused');
  await expect(b.locator('.rest')).toContainText('never checked');
  await expect(b.locator('.rest')).not.toContainText('due in');
  await expect(b.locator('.sw')).toHaveAttribute('aria-checked', 'false');

  // Failing: the badge carries the count and the server's error text gets its own line.
  const c = page.locator(`.row[data-sub="${SUBS.c}"]`);
  await expect(c.locator('.st')).toHaveText('Failed ×3');
  await expect(c.locator('.st')).toHaveClass(/err/);
  await expect(c.locator('.sub-err')).toHaveText('HTTP Error 404: Not Found');

  expect(await page.evaluate(() => window.__csp)).toEqual([]);
}));

test('subscription frames create, spin and remove a row in place', withMock({}, async ({ page, mock }) => {
  await open(page, mock);
  const row = page.locator(`.row[data-sub="${SUBS.scripted}"]`);
  await expect(row).toBeVisible({ timeout: 10_000 });
  await expect(row.locator('.row-title')).toHaveText('Scripted feed');
  await expect(row.locator('.rest')).toContainText('never checked');

  const before = await row.elementHandle();
  await expect(row.locator('.st')).toHaveText('Checking…', { timeout: 10_000 });
  await expect(row.locator('.disc')).toHaveClass(/spin/);
  await expect(row.locator('[data-sact="check"]')).toBeDisabled();

  await expect(row.locator('.st')).toHaveText('Active', { timeout: 10_000 });
  await expect(row.locator('.rest')).toContainText('checked just now');
  await expect(row.locator('.rest')).toContainText('2 seen');
  // The same node throughout: an upsert patches the row, it never rebuilds it.
  expect(await before.evaluate((el, other) => el === other, await row.elementHandle())).toBe(true);

  await expect(row).toHaveCount(0, { timeout: 10_000 });
}));

test('Check now and Check all post to the §4.7 routes', withMock({ freeze: true }, async ({ page, mock, request }) => {
  await open(page, mock);
  await page.click(`.row[data-sub="${SUBS.a}"] [data-sact="check"]`);

  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
  const [one] = await mock.log(request);
  expect(one.method).toBe('POST');
  expect(one.path).toBe(`api/v2/subscriptions/${SUBS.a}/check`);
  await expect(page.locator(`.row[data-sub="${SUBS.a}"] .st`)).toHaveText('Checking…');

  await page.click('#subs-check');
  await expect.poll(async () => (await mock.log(request)).length).toBe(2);
  const [, all] = await mock.log(request);
  expect(all.path).toBe('api/v2/subscriptions/check');
  expect(all.body).toEqual({});                       // §4.7: `{}` means "every one of them"
  await expect(page.locator('.toast')).toContainText('Checking 3 subscriptions');
}));

test('adding a subscription posts the §9 body, and 400/409 show the server sentence', withMock({ freeze: true }, async ({ page, mock, request }) => {
  await open(page, mock);
  await page.click('#subs-new');
  await expect(page.locator('#sub-form')).toBeVisible();

  await page.fill('#sub-url', 'https://www.youtube.com/@veritasium');
  await page.click('#sub-save');
  await expect(page.locator('#sub-err')).toHaveText('This URL is already subscribed');

  await page.fill('#sub-url', 'https://www.youtube.com/watch?v=dQw4w9WgXcQ');
  await page.click('#sub-save');
  await expect(page.locator('#sub-err')).toContainText('not a channel or playlist');
  await expect(page.locator('#rows-subs > .row')).toHaveCount(3);   // neither one landed

  await page.fill('#sub-url', 'https://www.youtube.com/@newfeed');
  await page.fill('#sub-name', 'New feed');
  await page.fill('#sub-every', '15');
  await page.click('#sub-save');
  await expect(page.locator('#sub-form')).toBeHidden();

  const log = await mock.log(request);
  expect(log).toHaveLength(3);
  expect(log[2].method).toBe('POST');
  expect(log[2].path).toBe('api/v2/subscriptions');
  expect(log[2].body).toEqual({
    url: 'https://www.youtube.com/@newfeed',
    name: 'New feed',
    check_interval_minutes: 15,
    download_type: 'video',
    format: 'mp4',
    quality: 'best',
    codec: 'auto',
    folder: null,
  });

  await expect(page.locator('#rows-subs > .row')).toHaveCount(4);
  await expect(page.locator('#rows-subs .row-title').filter({ hasText: 'New feed' })).toBeVisible();
  await expect(page.locator('#sub-url')).toHaveValue('');
}));

test('the toggle, the inline editor and delete use PATCH and DELETE', withMock({ freeze: true }, async ({ page, mock, request }) => {
  await open(page, mock);
  const a = page.locator(`.row[data-sub="${SUBS.a}"]`);

  await a.locator('.sw').click();
  await expect(a.locator('.sw')).toHaveAttribute('aria-checked', 'false');
  await expect(a.locator('.st')).toHaveText('Paused');

  await a.locator('[data-sact="edit"]').click();
  await expect(a.locator('.subedit')).toBeVisible();
  await expect(a.locator('.e-name')).toHaveValue('Veritasium');
  await expect(a.locator('.e-every')).toHaveValue('60');
  await a.locator('.e-name').fill('Veritasium (renamed)');
  await a.locator('.e-every').fill('90');
  await a.locator('.e-save').click();
  await expect(a.locator('.subedit')).toBeHidden();
  await expect(a.locator('.row-title')).toHaveText('Veritasium (renamed)');
  await expect(a.locator('.rest')).toContainText('every 90 min');

  page.once('dialog', (d) => d.accept());
  await a.locator('[data-sact="delete"]').click();
  await expect(a).toHaveCount(0);
  await expect(page.locator('#rows-subs > .row')).toHaveCount(2);

  const log = await mock.log(request);
  expect(log.map((e) => [e.method, e.path])).toEqual([
    ['PATCH', `api/v2/subscriptions/${SUBS.a}`],
    ['PATCH', `api/v2/subscriptions/${SUBS.a}`],
    ['DELETE', `api/v2/subscriptions/${SUBS.a}`],
  ]);
  expect(log[0].body).toEqual({ enabled: false });
  expect(log[1].body).toEqual({ name: 'Veritasium (renamed)', check_interval_minutes: 90 });
}));

test('a dismissed delete confirm leaves the subscription alone', withMock({ freeze: true }, async ({ page, mock, request }) => {
  await open(page, mock);
  page.once('dialog', (d) => d.dismiss());
  await page.click(`.row[data-sub="${SUBS.a}"] [data-sact="delete"]`);
  await page.waitForTimeout(300);
  expect(await mock.log(request)).toEqual([]);
  await expect(page.locator(`.row[data-sub="${SUBS.a}"]`)).toBeVisible();
}));

/* "3 min ago" and "due in 57 min" are the only two strings on the page that go wrong on their own,
   with no frame to correct them. The clock is installed before the navigation so the page's 30 s
   timer is the fake one; nothing here touches the socket. */
test('relative times re-render on their own timer, with no frame', withMock({ freeze: true }, async ({ page, mock }) => {
  await page.clock.install();
  await page.goto(mock.base);
  const rest = page.locator(`.row[data-sub="${SUBS.a}"] .rest`);

  // A frozen clock runs no rAF, so nudge it until the snapshot it already received is painted.
  await expect
    .poll(async () => { await page.clock.runFor(200); return rest.textContent().catch(() => ''); }, { timeout: 20_000 })
    .toContain('checked 3 min ago');

  await page.clock.runFor('04:10');
  const after = await rest.textContent();
  expect(after).toContain('checked 7 min ago');
  expect(after).toMatch(/due in 5[123] min/);         // 57 minus the four that just passed
}));

/* ------------------------------------------------------------ screenshots */

async function shoot(page, mock, name, { phone = false, sheet = false } = {}) {
  await page.setViewportSize(phone ? { width: 390, height: 844 } : { width: 1440, height: 1100 });
  await open(page, mock);
  await page.locator(`.row[data-id="${IDS.group}"] .chev`).click();
  if (sheet) {
    await page.click('#add-btn');                      // on a phone the plus opens the sheet
    await expect(page.locator('#sheet')).toBeVisible();
    await page.fill('#sheet-url', 'https://m.youtube.com/watch?v=8Xrcn5B04u4');
    await page.waitForTimeout(700);                    // let the catalog line settle
  }
  await page.waitForTimeout(250);
  await page.screenshot({ path: join(SHOTS, `${name}.png`), fullPage: !sheet, animations: 'disabled' });
}

test('screenshot — desktop light', withMock({ freeze: true, theme: 'light' }, async ({ page, mock }) => {
  await shoot(page, mock, 'desktop-light');
}));

test('screenshot — desktop dark', withMock({ freeze: true, theme: 'dark' }, async ({ page, mock }) => {
  await shoot(page, mock, 'desktop-dark');
}));

test('screenshot — phone light', withMock({ freeze: true, theme: 'light' }, async ({ page, mock }) => {
  await shoot(page, mock, 'phone-light', { phone: true });
}));

test('screenshot — phone dark', withMock({ freeze: true, theme: 'dark' }, async ({ page, mock }) => {
  await shoot(page, mock, 'phone-dark', { phone: true });
}));

test('screenshot — phone add sheet', withMock({ freeze: true, theme: 'light' }, async ({ page, mock }) => {
  await shoot(page, mock, 'phone-add', { phone: true, sheet: true });
}));

/* ------------------------------------------------- against a real server */

/**
 * The subset that needs nothing but a served page: run only when `AULOS_WEB_BASE` names a live
 * `aulos-server`. It asserts the same contract the mock is held to — so the two halves are
 * checked against one another — and then photographs the real page at both widths, empty queue
 * included (an empty queue must still look intentional).
 */
test.describe('real server', () => {
  test.skip(REAL_BASE === '', 'set AULOS_WEB_BASE to a running aulos-server');

  const CSP =
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
    "connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri 'none'; " +
    "form-action 'none'; frame-ancestors 'none'";

  test('index.html: the page, the CSP, the ETag and a 304', async ({ request }) => {
    const res = await request.get(REAL_BASE, { headers: { accept: 'text/html' } });
    expect(res.status()).toBe(200);
    const h = res.headers();
    expect(h['content-type']).toContain('text/html');
    expect(h['cache-control']).toBe('no-cache');
    expect(h['x-content-type-options']).toBe('nosniff');
    expect(h['referrer-policy']).toBe('no-referrer');
    expect(h['content-security-policy']).toBe(CSP);
    expect(h.etag).toMatch(/^"[0-9a-f]{64}"$/);

    const body = await res.text();
    expect(body).not.toContain('{{');
    const prefix = new URL(REAL_BASE).pathname;
    expect(body).toContain(`<meta name="aulos-prefix" content="${prefix}">`);
    expect(body).toMatch(/<meta name="aulos-theme" content="(auto|light|dark)">/);
    expect(body).toContain(`href="${prefix}assets/app.css"`);
    expect(body).toContain(`src="${prefix}assets/app.js"`);
    expect(body).toContain(`href="${prefix}manifest.webmanifest"`);

    const again = await request.get(REAL_BASE, {
      headers: { accept: 'text/html', 'if-none-match': h.etag },
    });
    expect(again.status()).toBe(304);
    expect(again.headers().etag).toBe(h.etag);
    expect(await again.body()).toHaveLength(0);
  });

  test('the same route without text/html is still the identity document', async ({ request }) => {
    const res = await request.get(REAL_BASE, { headers: { accept: 'application/json' } });
    expect(res.status()).toBe(200);
    expect(res.headers()['content-type']).toContain('application/json');
    expect(res.headers()['content-security-policy']).toBeUndefined();
    const body = await res.json();
    expect(body.name).toBe('aulos-server');
  });

  for (const [path, type] of [
    ['assets/app.css', 'text/css'],
    ['assets/app.js', 'javascript'],
    ['assets/icon.svg', 'image/svg+xml'],
    ['assets/icon-180.png', 'image/png'],
    ['manifest.webmanifest', 'application/manifest+json'],
  ]) {
    test(`${path}: 200 with its type, then 304`, async ({ request }) => {
      const res = await request.get(REAL_BASE + path);
      expect(res.status()).toBe(200);
      const h = res.headers();
      expect(h['content-type']).toContain(type);
      expect(h['cache-control']).toBe('no-cache');
      expect(h['x-content-type-options']).toBe('nosniff');
      expect(h['referrer-policy']).toBe('no-referrer');
      expect(h.etag).toMatch(/^"[0-9a-f]{64}"$/);
      expect(h['content-security-policy']).toBeUndefined();
      expect((await res.body()).length).toBeGreaterThan(0);

      const again = await request.get(REAL_BASE + path, { headers: { 'if-none-match': h.etag } });
      expect(again.status()).toBe(304);
      expect(await again.body()).toHaveLength(0);
    });
  }

  test('the manifest is scoped to the prefix', async ({ request }) => {
    const res = await request.get(REAL_BASE + 'manifest.webmanifest');
    const m = await res.json();
    expect(m.name).toBe('Aulos');
    expect(m.display).toBe('standalone');
    expect(m.theme_color).toBe('#E07850');
    expect(m.background_color).toBe('#F2F2F7');
    // Either spelling of the contract: the literal prefix, or a relative URL that resolves to it.
    const prefix = new URL(REAL_BASE).pathname;
    const resolve = (u) => new URL(u, REAL_BASE + 'manifest.webmanifest').pathname;
    expect(resolve(m.start_url)).toBe(prefix);
    expect(resolve(m.scope)).toBe(prefix);
    for (const icon of m.icons) expect(resolve(icon.src)).toBe(`${prefix}assets/${icon.src.split('/').pop()}`);
  });

  test('an unknown asset gets the standard 404 envelope', async ({ request }) => {
    const res = await request.get(REAL_BASE + 'assets/nope.js');
    expect(res.status()).toBe(404);
    expect(res.headers()['content-type']).toContain('application/json');
    const body = await res.json();
    expect(body.error.code).toBe('not_found');
    expect(typeof body.error.message).toBe('string');
  });

  test('the page boots, reaches the API and renders an empty queue', async ({ page }) => {
    const failures = [];
    page.on('console', (m) => { if (m.type() === 'error') failures.push(m.text()); });
    page.on('pageerror', (e) => failures.push(String(e)));
    await page.addInitScript(() => {
      window.__csp = [];
      document.addEventListener('securitypolicyviolation', (e) => window.__csp.push(`${e.violatedDirective} ${e.blockedURI}`));
    });

    await page.goto(REAL_BASE);
    await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 20_000 });
    await expect(page.locator('#addbar')).toBeVisible();
    expect(await page.evaluate(() => window.__csp)).toEqual([]);
    expect(failures).toEqual([]);

    // Empty or not, the page is never blank: the add bar is always there, and with no rows the
    // empty line is what fills the space.
    const rows = await page.locator('.row').count();
    if (rows === 0) await expect(page.locator('#empty')).toBeVisible();
    expect(await page.evaluate(() => document.scrollingElement.scrollWidth))
      .toBeLessThanOrEqual(await page.evaluate(() => window.innerWidth));
  });

  for (const [name, width, height] of [
    ['real-desktop', 1440, 1000],
    ['real-phone', 390, 844],
  ]) {
    test(`screenshot — ${name}`, async ({ page }) => {
      await page.setViewportSize({ width, height });
      await page.goto(REAL_BASE);
      await expect(page.locator('#conn-text')).toHaveText('Live', { timeout: 20_000 });
      await page.waitForTimeout(400);
      await page.screenshot({ path: join(SHOTS, `${name}.png`), fullPage: true, animations: 'disabled' });
    });
  }
});
