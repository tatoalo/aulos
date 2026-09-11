# Origin routing, live progress, delete safety, subscriptions — design

Date: 2026-09-11. Scope: the Aulos server (this repo) and the iOS app (`metube_ios`, branch
`v2-protocol`). Written by the orchestrating session after mapping both codebases and the VPS.

## Findings that drive the design

1. **Progress never reaches the Telegram board or the APNs Live Activity.** Provider progress
   travels on the aggregator's mpsc channel and is merged into the published snapshot only.
   `DomainEvent::StatusChanged` carries a view built with a `None` progress cell
   (`crates/aulos-queue/src/engine.rs:739,757`), so `ItemView::percent` is `0.0` for every
   non-group item until `Finished` (`crates/aulos-core/src/item.rs:357`). The Telegram board
   (`bot.rs:981-1011`) and the Live Activity `content-state` (`payload.rs:47-57`) both read that
   view. This is the operator's item 1 (bar stuck at 0 %) and most of item 5 (island frozen while
   the app is backgrounded, because background updates are push-only).
2. **Push fan-out is device-wide.** Alerts and Live Activity starts go to every registered device
   (`notifier.rs:752-770, 810`). Nothing on an item records which install added it: an iOS add is
   `SourceRef::bare(Ios)` with `ref = null` (`v2/downloads.rs:98`), and the app's
   `X-Aulos-Client` header is `ios/<version>` on every device. Item 3 (iPad silent for an iPhone
   add) is unimplementable without a per-install identity.
3. **Origin-only reporting already exists on the server** (commit 3401fd1: `APNS_PUSH_ALL` and
   `AULOS_TELEGRAM_WATCH_ALL`, both default off, and the VPS runs that build). Item 2 needs
   end-to-end verification, not a new mechanism — except the alert-if-tracked rule in
   `notifier.rs:775-788`, which stays.
4. **Delete can unlink files.** `delete_file` defaults to `DELETE_FILE_ON_TRASHCAN` (`false`),
   but the iOS dialog offers "Delete Item and File" and sends `delete_file: true`. Item 4.
5. **APNs pushes are lost on a transient egress failure.** The VPS runs inside a VPN namespace;
   during a VPN blip both Telegram polling and APNs failed, and the two APNs attempts since boot
   ended in `GaveUp` after `[1s, 4s, 16s]`. `healthz.apns.sent_total` is `0`.
6. **Subscriptions exist only on the server.** Full v2 REST + WS frames (PROTOCOL §4.x, §5.9,
   §9). The web page ignores the frames; the iOS core decodes them into
   `QueueStore.subscriptions` and no view reads it. Item 6.
7. **Live Activity has no stale date** on either side (`LiveActivityPresenter.swift:67,86,96`;
   `payload.rs:131-139`), so a stalled stream looks fresh forever.

## Decisions

### D1 — Subscribers pull live progress from the published snapshot (server)

No new event flood through the `EventRouter` (its per-subscriber queues are `DropNewest`, and a
progress flood could evict a `Completed`). Instead:

- `aulos-core` gains a small port, `ProgressReader` (name at implementer's discretion), with
  `fn view(&self, id: ItemId) -> Option<Arc<ItemView>>`. `aulos-queue::StateView` implements it
  (`publish.rs:224`, `Published::get`). Wiring injects it into the Telegram actor and the APNs
  notifier. Dependency rules in `aulos-workspace-tests` must stay green — hence the port.
- **Telegram**: on every 1 Hz tick, for each job on a live board that is `downloading` /
  `postprocessing`, refresh `percent / speed / eta / downloaded / total` from the reader; mark the
  board dirty when the rendered body changes. The existing 3 s per-chat edit limiter stays, so a
  progressing board redraws about every 3 s with a real bar. Status transitions keep their
  immediate path.
- **APNs Live Activity**: the update path (`step()` / `arm_timer()`) reads the freshest view from
  the reader when it builds the `content-state`, and while an item is `downloading` the trailing
  timer keeps re-arming on a **progress cadence of 5 s** per (item, device) until the item leaves
  the progressing statuses. Status changes stay immediate (2 s window as today). Two new
  `aps` keys on updates: `stale-date` = now + 45 s (the widget renders a stale hint if no push
  lands by then) and `relevance-score` = percent / 100.
- Groups already carry real numbers; the reader path must not regress them.

### D2 — Per-install identity (server + iOS)

- New request header `X-Aulos-Install: <id>`; `id` is 8–64 chars of `[A-Za-z0-9._-]`. Added to
  the CORS allow-list next to `X-Aulos-Client`. Documented in PROTOCOL §1.3.
- The iOS app mints a UUID once, stores it in the App Group defaults (shared with the share
  extension), and sends the header on every request from both targets.
- On `POST api/v2/downloads` (and the v2 batch form), when the resolved source kind is `ios` and
  the header is present and valid, the item is stored as `SourceRef::with_ref(Ios, install_id)`.
  Missing or malformed header → `SourceRef::bare(Ios)` as today (older app builds keep working).
- Device registration (`PUT api/v2/devices/{token}`) accepts optional `"install_id"`; the
  `devices` table gains a nullable `install_id` column (migration `0004`). Documented in
  PROTOCOL §4.8.
- Routing in `aulos-apns` with `APNS_PUSH_ALL=false`, for an item whose `source.kind == ios`:
  - completion/failure **alert** → devices with `alerts = 1` **and** `install_id == source.ref`;
    if `source.ref` is null → every alerting device (legacy behaviour);
  - Live Activity **start** → the same install match over `live_activity_start_token`;
  - **update** / **end** → unchanged: every registration held for that item;
  - the alert-if-tracked rule is unchanged.
  `APNS_PUSH_ALL=true` keeps today's fan-out. Non-iOS origins are unchanged (no alert, no start).
- The WebSocket is untouched: an open app on any device still sees everything live.

### D3 — Delete never touches files from the clients

- iOS: remove the "Delete Item and File" choice; every delete and "Clear completed" sends
  `"delete_file": false` explicitly. The detail view stays without a delete button.
- Web: `delete` and `clear` send `"delete_file": false` explicitly.
- Server: no wire change. `DELETE_FILE_ON_TRASHCAN` and the `delete_file` parameter remain for
  curl/API users; PROTOCOL gets a one-line note that the shipped clients never set it to true.

### D4 — Live Activity reliability (iOS)

- Every local `Activity.request` / `update` / `end` carries a `staleDate` (now + 45 s for
  updates; the end frame keeps the 15-minute dismissal).
- The widget reads `context.isStale` and renders a muted "waiting for the server…" state instead
  of a confident number.
- The 2 s foreground throttle stays. Flush-on-background stays.
- Detached per-update `Task`s in `LiveActivityPresenter.updateActivity` are serialised (one
  actor-ordered chain) so two close updates cannot land out of order.

### D5 — APNs retry for the pushes that matter (server)

Alerts, Live Activity starts and ends (priority 10, one-hour TTL) retry on `429`/`5xx`/transport
errors with backoff `[1s, 4s, 16s, 60s, 120s, 300s]` and stop early at the push's `expiration`.
Updates keep `[1s, 4s, 16s]` (they are superseded anyway). `healthz.apns` gains
`retried_total`. Delivery order per device is not guaranteed across retries; that is acceptable
for alerts with `apns-collapse-id` and for start/end.

### D6 — Subscriptions UI (web + iOS)

Both clients get a first-class Subscriptions surface driven by the existing contract:

- List: name, URL host, enabled toggle, interval, **last checked** (relative time, from
  `last_checked` ms), **next due**, `checking` spinner, `consecutive_failures` / `error` badge,
  `seen_count`.
- Actions: **Check now** (per row → `POST subscriptions/{id}/check`; header button → `POST
  subscriptions/check {}`), add (URL, name, interval, the same download selection the add form
  uses → `POST subscriptions`), edit name / interval / enabled (`PATCH`), delete (`DELETE`).
- Live: `subscription` and `subscription_removed` frames update the list in place; the snapshot
  seeds it.
- Web: a "Subscriptions" panel on the shipped page. The 70 KB page budget in `ci.yml` has ~500
  bytes of headroom; raise it to 96 KB in the same commit with a one-line justification (the
  page stays dependency-free and served from the binary).
- iOS: a new `AppSection.subscriptions` (tab on iPhone, sidebar row on iPad) backed by a
  `SubscriptionsViewModel` over `QueueStore.subscriptions` plus the REST calls. Follows the
  existing card style and theme tokens.

### D7 — Automation hook (iOS, DEBUG only)

A `DEBUG`-only launch environment override `AULOS_DEBUG_SERVER_URL` sets the server URL at
launch so the orchestrator can point a device build at a local server with
`xcrun devicectl device process launch --environment-variables`.

## Testing

- Server: TDD per crate; `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace`, `cargo test -p aulos-workspace-tests`. The APNs suite
  keeps using `wiremock` via `APNS_BASE_URL_OVERRIDE`; new tests assert real percent in the
  update `content-state`, the 5 s progress cadence, `stale-date`, install targeting, and the
  extended retry ladder.
- Web: the Playwright smoke in `tools/web` gains subscription cases (mock server emits the
  frames and serves the routes); the budget check passes.
- iOS: `swift test` in `Packages/AulosCore` and `xcodebuild test` on a simulator; new tests for
  the install id, the header, the delete body, `staleDate`, and the subscriptions view model.
- End to end (orchestrator): local server on the Mac with the real APNs key and sandbox tokens
  from a debug build on the iPhone; then the amd64 image loaded onto the VPS for Telegram and
  production-token flows. Assertions: Telegram add → chat board with a moving bar, no APNs push;
  iOS add → phone alert + island with moving percent, no Telegram message, iPad silent; delete
  from web/app leaves the file on disk; subscription check-now round-trips.

## Commit rules

No AI attribution trailers in commit messages (project rule, `docs/STATUS.md`). Never run the
production Telegram token from a dev machine.
