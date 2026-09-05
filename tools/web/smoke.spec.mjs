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

const ID = (s) => (s + '00000000000000000000000000').slice(0, 26);
const IDS = {
  dl: ID('DL'), group: ID('GRP'), c1: ID('C1'), c2: ID('C2'), c3: ID('C3'),
  pp: ID('PP'), resolving: ID('RES'), waiting: ID('WAIT'), fin: ID('FIN'), err: ID('ERR'),
};

async function startMock(opts = {}) {
  const args = ['mock-server.mjs', '--port', '0'];
  if (opts.prefix) args.push('--prefix', opts.prefix);
  if (opts.theme) args.push('--theme', opts.theme);
  if (opts.token) args.push('--token', opts.token);
  if (opts.freeze) args.push('--freeze');
  if (opts['big-group']) args.push('--big-group');
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
    stop() { child.kill('SIGKILL'); },
  };
}

/** Spawn a mock, run `body`, always kill the child. */
function withMock(opts, body) {
  return async ({ page, request }) => {
    const mock = await startMock(opts);
    try { await body({ page, request, mock }); } finally { mock.stop(); }
  };
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

  // The removed frame took the finished row out; the paused one moved to "Waiting for you".
  await expect(page.locator(`.row[data-id="${IDS.fin}"]`)).toHaveCount(0);
  await expect(page.locator(`#rows-waiting .row[data-id="${IDS.dl}"]`)).toBeVisible();
}));

test('Clear deletes every terminal id, after a confirm', withMock({}, async ({ page, mock, request }) => {
  await open(page, mock);
  page.once('dialog', (d) => d.accept());
  await page.click('#clear');

  await expect.poll(async () => (await mock.log(request)).length).toBe(1);
  const [entry] = await mock.log(request);
  expect(entry.body.action).toBe('delete');
  expect(entry.body.ids.sort()).toEqual([IDS.err, IDS.fin].sort());
  await expect(page.locator('#sec-done')).toBeHidden();
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
  await page.goto(mock.base);
  await expect(page.locator('#token-sheet')).toBeVisible();
  await expect(page.locator('#conn-text')).toHaveText('Sign in');
  await expect(page.locator(`.row[data-id="${IDS.dl}"]`)).toHaveCount(0);

  await page.fill('#token-input', 's3cret');
  await page.click('#token-save');

  // The WS upgrade carried ?token= (the snapshot arrived) …
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

  const targets = await page.evaluate(() => {
    const out = [];
    for (const el of document.querySelectorAll('.iconbtn, .btn-primary, .chip, .seg button')) {
      const r = el.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) continue;
      out.push({ cls: el.className, h: Math.round(r.height), w: Math.round(r.width) });
    }
    return out;
  });
  expect(targets.length).toBeGreaterThan(2);
  for (const t of targets) expect(t.h, `${t.cls} height`).toBeGreaterThanOrEqual(44);

  // Only the primary action and the ⋯ button survive the collapse.
  const acts = page.locator(`.row[data-id="${IDS.dl}"] .row-acts .iconbtn:visible`);
  await expect(acts).toHaveCount(2);

  await page.click('#url');
  await expect(page.locator('#sheet')).toBeVisible();
  await expect(page.locator('#sheet-type button')).toHaveCount(4);
  await expect(page.locator('#sheet-quality .chip').first()).toBeVisible();
  expect(await page.evaluate(() => document.scrollingElement.scrollWidth)).toBeLessThanOrEqual(390);
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
