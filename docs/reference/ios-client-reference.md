
# Aulos (metube_ios) — iOS Client Reference

Repo: `/Users/apogliaghi/Development/metube_ios` · HEAD `8622a2f` (clean tree) · ~6,700 lines Swift total.

## 1. Project layout & build

### Files
| Path | Purpose |
|---|---|
| `/Users/apogliaghi/Development/metube_ios/project.yml` | XcodeGen spec (source of truth) |
| `/Users/apogliaghi/Development/metube_ios/Aulos.xcodeproj` | Generated; `xcshareddata/xcschemes/Aulos.xcscheme` is the only scheme |
| `/Users/apogliaghi/Development/metube_ios/CLAUDE.md` | 2 lines: backend path + build command |
| `/Users/apogliaghi/Development/metube_ios/README.md` | **Stale** — still describes "MeTube iOS App", `group.com.metube.app`, `metube://` scheme, `MeTubeAPIService`, pre-rebrand file tree |
| `docs/superpowers/specs/2026-06-28-fast-share-add-design.md` | Design for fire-and-forget share add |
| `docs/superpowers/plans/2026-06-28-fast-share-add.md` | Task-by-task implementation plan (full code listings) |

### Targets (`project.yml`)
| Target | Type | Bundle id | Notes |
|---|---|---|---|
| `Aulos` | `application` | `com.tatoalo.aulos` | sources `Aulos/` (excludes `**/*.entitlements`); deps: `AulosShareExtension` (embed), `AulosCore`, `SocketIO` |
| `AulosShareExtension` | `app-extension` | `com.tatoalo.aulos.share-extension` | sources `AulosShareExtension/` **plus** `Aulos/Theme`, `Aulos/Views/StatusBadge.swift`, `Aulos/Views/Components`; dep: `AulosCore` only (no SocketIO) |
| `AulosCore` | SwiftPM library | — | `Packages/AulosCore`, platforms `.iOS(.v17)`, `.macOS(.v13)`; test target `AulosCoreTests` |

Global settings: `deploymentTarget.iOS = "17.0"`, `xcodeVersion "15.0"`, `SWIFT_VERSION = "5.9"`, `TARGETED_DEVICE_FAMILY = "1,2"` (iPhone + iPad), `MARKETING_VERSION 1.0.0` / `CURRENT_PROJECT_VERSION 1`, `bundleIdPrefix: com.tatoalo.aulos`. iPad supports all 4 orientations, iPhone portrait + both landscapes. `GENERATE_INFOPLIST_FILE: YES` with a hand-written `INFOPLIST_FILE` also set.

### Dependencies
| Package | Spec | Resolved |
|---|---|---|
| `socket.io-client-swift` | `from: "16.1.0"`, product `SocketIO` | **16.1.1** (`42da871…`) |
| `Starscream` (transitive) | — | **4.0.8** |
| `AulosCore` | local path | — |

`Aulos.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved` holds the pins. No CocoaPods, no Carthage, no fastlane (only `.gitignore` entries).

### Build / test from CLI
```bash
# regenerate project (README)
brew install xcodegen && xcodegen generate

# build app + extension (CLAUDE.md, canonical)
xcodebuild -project Aulos.xcodeproj -scheme Aulos \
  -destination 'platform=iOS Simulator,name=iPhone 17' -quiet build 2>&1 | tail -10

# core unit tests on host (macOS), 14 tests
cd Packages/AulosCore && swift test
```
The `Aulos` scheme builds both `Aulos.app` and `AulosShareExtension.appex`; `<Testables>` is **empty** (AulosCore tests are not wired into the Xcode scheme — only `swift test`). Configs: Run/Test/Analyze = Debug, Profile/Archive = Release.

### Entitlements / app group / keychain
| File | Contents |
|---|---|
| `Aulos/Aulos.entitlements` | `com.apple.security.application-groups = [group.com.tatoalo.aulos]` |
| `AulosShareExtension/AulosShareExtension.entitlements` | identical |

- **No keychain sharing group.** Auth cookies are stored in **app-group `UserDefaults`**, not the keychain (see §3).
- No push entitlement (local notifications only), no background-modes key, no App Transport Security exception (so plain `http://` to a LAN IP relies on the default ATS behavior — the UI offers an `http://` toggle; note this may fail on device without an ATS exception).
- `Constants.appGroupIdentifier = "group.com.tatoalo.aulos"`, `Constants.callbackURLScheme = "aulos"` (`Packages/AulosCore/Sources/AulosCore/Services/Constants.swift`).

### Info.plists
`Aulos/Info.plist`: only `CFBundleURLTypes` → scheme `aulos` (name `com.tatoalo.aulos`, role Editor). Everything else is generated.

`AulosShareExtension/Info.plist`:
```
CFBundleDisplayName = Aulos
NSExtensionPointIdentifier = com.apple.share-services
NSExtensionPrincipalClass = $(PRODUCT_MODULE_NAME).ShareViewController
NSExtensionAttributes.NSExtensionJavaScriptPreprocessingFile = ShareExtensionPreprocessing
NSExtensionActivationRule = { NSExtensionActivationSupportsWebPageWithMaxCount: 1,
                              NSExtensionActivationSupportsWebURLWithMaxCount: 1 }
```
`AulosShareExtension/ShareExtensionPreprocessing.js` returns `{"URL": window.location.href}` — the address-bar URL, used to defeat canonical/truncated URLs from `UTType.url`.

---

## 2. Architecture

### Module map
```
Packages/AulosCore/Sources/AulosCore/        (shared, UIKit-free except UserNotifications)
  Models/  AddRequest, APIResponse, ConnectionStatus, DownloadSettings, QueueItem(+HistoryResponse),
           ServerConfiguration, ServerVersion
  Services/ Constants, ConfigurationManager, CookieManager,
            AulosAPIService (actor), QueueService (actor),
            BackgroundAddService, BackgroundAddUploader, BackgroundAddCompletionHandler,
            AddResultClassifier, AddNotificationPresenter
Aulos/                                        (app target only)
  App/      MeTubeApp (@main), AppDelegate, ContentView
  Services/ SocketService (@MainActor ObservableObject), AuthenticationService (@MainActor),
            StressTestService (@MainActor)
  ViewModels/ QueueViewModel, SettingsViewModel  (both @MainActor ObservableObject)
  Views/    QueueView, SettingsView, StatusBadge, WebLoginView, iPadSidebarView,
            Components/HeaderView, Components/SettingsCard
  Theme/    Theme.swift   Extensions/ AppTheme+SwiftUI.swift
AulosShareExtension/  ShareViewController (UIKit host), ShareView (SwiftUI), ShareViewModel
```

### Concurrency model
- **MVVM + Combine**, *not* Observation. `@Published` + `@StateObject`/`@ObservedObject`; view models bind to services via `Combine.sink(...).receive(on: DispatchQueue.main)`.
- `@MainActor` on `SocketService`, `QueueViewModel`, `SettingsViewModel`, `ShareViewModel`, `AuthenticationService`, `StressTestService`.
- Two **actors** for networking: `AulosAPIService` and `QueueService` (each owns its own `URLSession`).
- `ConfigurationManager` and `CookieManager` are `final class … Sendable` singletons that recompute `UserDefaults(suiteName:)` on every access (no caching).
- Socket.IO callbacks hop to main via `Task { @MainActor in … }`.
- Timing is done with `Task.sleep(nanoseconds:)`, cancellation via stored `Task<Void, Never>?`.

### Navigation
`ContentView` branches on `horizontalSizeClass`:
- **regular (iPad)** → `iPadSidebarView` = `NavigationSplitView` with a `List` sidebar (`SidebarSection.settings` / `.queue`) and `NavigationStack` details.
- **compact (iPhone)** → `iPhoneTabView` = `TabView` with tags `DefaultTab.settings` / `.queue`; initial tab from `ConfigurationManager.shared.downloadSettings.defaultTab`.

Both wire `scenePhase`: `.active` → `socketService.connect()` if authenticated, not connected and `!autoRetryExhausted`; `.background` → `socketService.disconnect()`. Both call `.preferredColorScheme(settingsViewModel.appTheme.colorScheme)`. Tab/sheet changes send `resignFirstResponder` to dismiss the keyboard.

---

## 3. Backend client / wire protocol as consumed

### Base URL & URL_PREFIX handling
Configured as a single free-text string `ServerConfiguration.serverURL` (Settings → Server, split into a protocol chip `https://`/`http://` plus a "hostname or IP:port" text field). `isConfigured` requires a parseable `URL` with non-nil `scheme` **and** `host`.

Endpoints are built by **appending to the configured path** (so a `URL_PREFIX` like `https://host/metube/` works), in `Packages/AulosCore/Sources/AulosCore/Models/ServerConfiguration.swift`:

| Property | Construction | Example with prefix `https://h/metube` |
|---|---|---|
| `addEndpointURL` | `path + ("add" if path ends "/" else "/add")` | `https://h/metube/add` |
| `versionEndpointURL` | `… + "/version"` | `https://h/metube/version` |
| `historyEndpointURL` | `… + "/history"` | `https://h/metube/history` |
| `deleteEndpointURL` | `… + "/delete"` | `https://h/metube/delete` |
| `socketIOPath` | strips a trailing `/` then `+ "/socket.io"`; `"/socket.io"` fallback | `/metube/socket.io` |

Note the app calls `/add`, `/history`, `/version`, `/delete` at the **prefix root**, i.e. it does **not** use a `/api/` segment — except one stale code path: `AuthenticationService.verifyAuthenticationWithServer` probes `serverURL.appendingPathComponent("api/history")` (`Aulos/Services/AuthenticationService.swift:101`). That path is only reachable from the `ASWebAuthenticationSession` cancel branch, which the current UI never uses (login goes through `WebLoginView`), so it is effectively dead code but inconsistent.

### REST endpoints
| Method | Path | Caller | Request body | Success handling | Failure handling |
|---|---|---|---|---|---|
| `POST` | `/add` | `AulosAPIService.addURL` (foreground, in-app; currently no in-app add UI wired) | `AddRequest` JSON: `{"url":String,"quality":String,"format":String,"auto_start":Bool}` | 2xx → decode `APIResponse`; if `status != "ok"` and `msg != nil` → throw `.serverError(msg)` | 401/403 → `.authenticationRequired`; other → decode `APIResponse.msg` else `"HTTP \(code)"` |
| `POST` | `/add` | Share extension via **background upload** (`ShareViewModel.enqueueAdd`) | Same 4 keys, built with `JSONSerialization` from a `[String: Any]`, staged to a file | Silent | Classified by `AddResultClassifier`, local notification |
| `GET` | `/version` | `AulosAPIService.testConnection` (ignores body) and `.fetchServerVersion` | — | decode `ServerVersion` | 401/403 → auth; else `"HTTP \(code)"` |
| `GET` | `/history` | `QueueService.fetchHistory` **and** `SocketService.fetchInitialState` (duplicated inline, uses `URLSession.shared`) | — | decode `HistoryResponse` | 401/403 → auth |
| `POST` | `/delete` | `AulosAPIService.deleteItems(ids:where:)`, and `clearCompleted(ids:)` = `deleteItems(ids:where:"done")` | `JSONSerialization` of `{"ids": [String], "where": "done" | "queue"}` | 2xx → return, body ignored | 401/403 → auth; else `"HTTP \(code)"` |

**Not used at all:** `/start`, `/add_batch`, any subscription/playlist/config endpoint, any file/download/stream URL. There is **no retry/start/pause action** in the UI — only delete.

Header set on every request: `Content-Type: application/json` (POSTs only) + manual `Cookie` headers. No `Authorization` header, no HTTP Basic auth, no bearer token, no `ETag`/`If-None-Match`, no `Accept`, no custom User-Agent.

### URLSession configuration
`AulosAPIService` and `QueueService` each build `URLSessionConfiguration.ephemeral` with `timeoutIntervalForRequest = 30`, `timeoutIntervalForResource = 60`, and cookie handling fully disabled:
```swift
configuration.httpCookieAcceptPolicy = .never
configuration.httpCookieStorage = nil
configuration.httpShouldSetCookies = false
```
Comment: *"This is critical for share extensions where `HTTPCookieStorage.shared` is empty."* Cookies are injected/harvested manually by `addCookies(to:for:)` / `storeCookies(from:for:)` (identical private helpers duplicated in both actors).

`SocketService.fetchInitialState` instead uses `URLSession.shared` with manual cookie headers.

### Auth model
Cookie/session based (Authelia-style SSO in front of MeTube). **No basic auth anywhere.**

1. `SettingsViewModel.startLogin()` saves the URL and shows `WebLoginView` (a `WKWebView` on `WKWebsiteDataStore.default()` loading `serverURL`).
2. `WebViewRepresentable.Coordinator.checkForAuthCookies` fires on each `didFinish`; it *skips hosts containing `"auth"`*, then requires at least one cookie whose **name contains `"session"` or `"auth"`**, filters cookies by domain suffix match, saves them via `CookieManager`, and calls `onAuthSuccess` once (`hasCheckedCookies` latch).
3. `CookieManager` serializes `HTTPCookie.properties` into app-group `UserDefaults` key `authCookies` as JSON, converting `Date` values to `timeIntervalSince1970` and adding sibling marker keys `"<key>_isDate": true`.
4. `isAuthenticated` == `CookieManager.shared.hasCookies()` — **presence only**, not validity. (`areCookiesValid()` exists and checks `expiresDate` but is never called.)
5. `logout()` clears app-group cookies and wipes `WKWebsiteDataStore` data records.
6. `AuthenticationService` also contains a full `ASWebAuthenticationSession` flow (`callbackURLScheme: "aulos"`, `prefersEphemeralWebBrowserSession = false`) with `HTTPCookieStorage.shared` and `WKWebsiteDataStore` cookie harvesting — present but not invoked by the current UI.

`CookieManager.getCookies(for:)` matches by suffix: strips a leading `.` from `cookie.domain`, then `host.hasSuffix(cookieDomain) || host == cookie.domain`. Path and `secure` flags are not considered.

### Socket.IO
Client: `socket.io-client-swift` 16.1.1 over Starscream. `Aulos/Services/SocketService.swift`.

```swift
var socketConfig: SocketIOClientConfiguration = [
    .log(false),
    .compress,
    .forceWebsockets(true),      // no HTTP long-poll upgrade dance
    .reconnects(false),          // library reconnection DISABLED; hand-rolled retry
    .connectParams(["EIO": "4"]) // Socket.IO v4 protocol
]
socketConfig.insert(.cookies(cookies))       // only if CookieManager has any for baseURL
socketConfig.insert(.path(config.socketIOPath))
manager = SocketManager(socketURL: baseURL, config: socketConfig)
socket  = manager?.defaultSocket             // default namespace "/"
```

#### Client events observed
| Event | Behavior |
|---|---|
| `.connect` | cancel "testing" delay task, `isConnected = true`, reset `failedConnectionAttempts`, `autoRetryExhausted = false`, persist `lastConnectionWasConnected = true` (`UserDefaults.standard` key `aulos.lastConnectionWasConnected`), `connectionStatus = .connected`, cancel timeout task, then **`await fetchInitialState()`** (HTTP `GET /history`) |
| `.disconnect` | honors a one-shot `ignoreNextDisconnectEvent` flag (set when `connect()` tears down an existing socket), `isConnected = false`, returns early if `isAttemptingConnection`, else clears persisted flag, schedules auto-retry, `connectionStatus = .disconnected` |
| `.error` | extracts `String` or `Error` from `data.first`, else `"Connection error"`; sets `connectionError`; `connectionStatus = .error(msg)` |
| `.reconnectAttempt` | sets `.testing` unless `autoRetryExhausted` (dead in practice — `reconnects(false)`) |
| `.statusChange` | if payload is `SocketIOStatus == .disconnected`, same handling as `.disconnect` |
| `onAny` | logs `event.event` + payload **count** for every event except `"updated"`; explicit comment that logging MeTube payloads (full yt-dlp metadata tree) "is large enough to stall UI updates while debugging on device" |

#### Server events consumed
| Event | Expected payload | Decoding | Applied |
|---|---|---|---|
| `all` | `[[active…],[done…]]` where each item is `[key, info_dict]` | 3-way fallback: (a) `data.first as? String` → `JSONDecoder` → `SocketAllResponse`; (b) `data.first as? [Any]` with `count >= 2` → `handleAllEventFromArray`; (c) `data` itself as the tuple. `handleAllEventFromArray` additionally falls back to `NSArray` iteration, taking `itemTuple[1] as? [String: Any]` → re-serialize → `QueueItem` | replaces `items` (preserving `stress-test-*` ids); **active then done order** |
| `updated` | JSON string **or** dict of one item | `QueueItem` | `queueThrottledUpdate` (250 ms throttle) |
| `added` | JSON string **or** dict | `QueueItem` | `items.insert(at: 0)` if id not present |
| `completed` | JSON string **or** dict | `QueueItem` | `updateItemImmediately` (no throttle) |
| `canceled` | a plain id `String` (tries `JSONDecoder().decode(String.self, …)` first, then the raw string) | — | `removeItems(withIds:)`, matching **either** `item.id` **or** `item.url` |
| `cleared` | same as `canceled` | — | same |
| `formats` | JSON string **or** `[[String: Any]]` | `[ServerFormat]` | writes `ConfigurationManager.availableFormats`, bumps `formatsUpdated: Date` (drives the Settings/Share pickers) |

No client→server emits at all (all mutations go over REST). No WebSocket-native or SSE code paths.

`SocketAllResponse` (bottom of `SocketService.swift`) is a hand-written `Decodable` that walks two nested unkeyed containers, discards the first element of each pair (the key `String`), and decodes the second into `[String: AnyCodable]`. `AnyCodable` is a local `Decodable`-only shim trying `nil/Bool/Int/Double/String/[AnyCodable]/[String:AnyCodable]`.

**Important asymmetry for the backend:** the socket `all` payload is parsed as **pairs** `[key, info]`, while `GET /history` is decoded as `HistoryResponse` = three **flat arrays of item objects** (`queue`, `done`, `pending`). Both shapes must hold simultaneously.

#### Reconnection logic (hand-rolled)
| Knob | Value |
|---|---|
| `maxAutoRetryAttempts` | `3` |
| retry delay | `5_000_000_000` ns (5 s) fixed, no backoff/jitter |
| `connectionAttemptTimeout` | `10_000_000_000` ns (10 s) → `connectionError = "Connection timed out"`, `.disconnected`, `recordFailedConnectionAttempt()` |
| `testingStateDelay` | `300_000_000` ns — delay before showing `.testing`, to avoid flicker on fast connects |
| `connectedHoldDelay` | `900_000_000` ns — when the last session was connected, hold the optimistic `.connected` label this long before showing `.testing` |
| `updateDebounceInterval` | `250_000_000` ns — progress throttle |
| persisted optimism | `UserDefaults.standard["aulos.lastConnectionWasConnected"]`; on `init()` the service **starts in `.connected`** if the last session was connected |

`connect(resetFailures:showTestingImmediately:)` always calls `disconnect()` first (setting `ignoreNextDisconnectEvent` if a socket existed). Retry is gated on `shouldAutoReconnect && authService.isAuthenticated && failedConnectionAttempts < 3`. After 3 failures: `autoRetryExhausted = true`, `shouldAutoReconnect = false`, persisted-connected flag cleared, and Settings surfaces a manual **"Retry Connection"** row (`SettingsViewModel.shouldShowRetryConnection`). Pull-to-refresh in the queue = `disconnect()` → 100 ms sleep → `connect(resetFailures: true, showTestingImmediately: true)`.

---

## 4. Data models (exact keys)

### `QueueItem` — `Packages/AulosCore/…/Models/QueueItem.swift`
| Swift field | JSON key | Type | Notes |
|---|---|---|---|
| `id` | `id` | `String` (non-opt) | if absent falls back to `url`, then `UUID().uuidString` |
| `url` | `url` | `String?` | also used as the **delete key** |
| `title` | `title` | `String` | defaults `"Unknown"` |
| `status` | `status` | `DownloadStatus` | defaults `.pending` |
| `percent` | `percent` | `Double?` | 0–100; `progress` clamps to `[0,100]` |
| `eta` | `eta` | `String?` | accepts `String`, `Int`, or `Double` seconds → formatted `"Xm Ys"` / `"Ys"` |
| `msg` | `msg` | `String?` | error text shown in the Failed row + alert |
| `speed` | `speed` | `Double?` | bytes/s; rendered via `ByteCountFormatter` + `"/s"` |
| `downloadedBytes` | `downloaded_bytes` | `Double?` | decoded but **never rendered** |
| `totalBytes` | `total_bytes` | `Double?` | never rendered |
| `totalBytesEstimate` | `total_bytes_estimate` | `Double?` | never rendered |
| `fragmentIndex` | `fragment_index` | `Double?` | never rendered |
| `fragmentCount` | `fragment_count` | `Double?` | never rendered |

All five numeric extras go through `decodeFlexibleDoubleIfPresent` (accepts `Double`, `Int`, or numeric `String`). `Equatable` compares every field **except `url` and `title`**. `withStatus(_:)` produces a status-overridden copy (unused in current code paths).

### `DownloadStatus: String, Codable`
| Case | Accepted raw values | `displayName` | `isActive` |
|---|---|---|---|
| `pending` | `"pending"` (+ **any unknown value**, with a `print` of `"DEBUG: Unknown status value: '…'"`) | `Pending` | true |
| `preparing` | `"preparing"` | `Preparing` | true |
| `downloading` | `"downloading"` | `Downloading` | true |
| `finished` | `"finished"` **or** `"done"` | `Done` | false |
| `error` | `"error"` | `Error` | false |

No `canceled`/`paused`/`skipped` case.

### `HistoryResponse`
```swift
public struct HistoryResponse: Codable, Sendable {
    public let queue: [QueueItem]     // required
    public let done: [QueueItem]      // required
    public let pending: [QueueItem]   // required
}
activeItems    = (queue + pending).sorted { $0.title < $1.title }
completedItems = done.sorted { $0.title < $1.title }
allItems       = activeItems + completedItems
```
All three keys are **non-optional** — a `/history` response missing `pending` fails the whole decode (`APIError.invalidResponse` / silent drop in `fetchInitialState`).

### `AddRequest`
`{"url": String, "quality": String, "format": String, "auto_start": Bool}` (`autoStart` default `true`).

### `APIResponse`
`{"status": String, "msg": String?}`; `isSuccess == (status == "ok")`.

### `ServerVersion`
`{"version": String, "yt-dlp": String}`. `ytDlpDisplay` turns `2026.02.12.233641` (>3 dot components) into `2026.02.12 (nightly)`.

### `ServerFormat` / `ServerQuality` (server-driven, from the `formats` socket event)
```
ServerFormat  { "id": String, "text": String, "qualities": [ServerQuality] }
ServerQuality { "id": String, "text": String }
```
`ServerFormat.defaultFormats` (fallback when nothing cached) — exact ids:

| format `id` | `text` | quality ids |
|---|---|---|
| `any` | Any | `best,2160,1440,1080,720,480,360,240,worst,audio` |
| `mp4` | MP4 | `best,best_remux,best_ios,2160,1440,1080,720,480,360,240,worst` |
| `m4a` | M4A | `best,192,128` |
| `mp3` | MP3 | `best,320,192,128` |
| `opus` | OPUS | `best` |
| `wav` | WAV | `best` |
| `flac` | FLAC | `best` |
| `thumbnail` | Thumbnail | `best` |

Legacy enums `VideoFormat` (`mp4,any,m4a,mp3,opus,wav,flac,thumbnail`) and `VideoQuality` (`best,best_ios,2160,1440,1080,720,480,360,240,worst,audio,320,192,128`) are retained but marked *"kept for reference, no longer used by app"*. Note `best_remux` exists only in the server-driven list.

### `DownloadSettings` (persisted, app-group)
| Field | JSON key | Default | Notes |
|---|---|---|---|
| `defaultQualityId` | `defaultQuality` | `"best"` | |
| `defaultFormatId` | `defaultFormat` | `"mp4"` | |
| `autoStart` | `autoStart` | `true` | sent as `auto_start` |
| `defaultTab` | `defaultTab` | `.settings` | `DefaultTab: settings|queue` |
| `appTheme` | `appTheme` | `.system` | `AppTheme: system|light|dark` |
| `developerModeEnabled` | `developerModeEnabled` | `false` | unlocks the stress-test section |

Custom `init(from:)` gives every key a default, so partial/old blobs migrate cleanly.

### `ServerConfiguration`
`{"serverURL": String}` only.

### `ConnectionStatus`
`unknown | testing | connected | disconnected | error(String)` with hand-written `==`.

### `AddOutcome` / `AddTaskContext`
- `AddOutcome: success | failure(reason: String)`
- `AddTaskContext: Codable {url: String, host: String, payloadPath: String}` — JSON-encoded into `URLSessionTask.taskDescription` to survive the extension→app process hop.

### Persisted app-group keys (`UserDefaults(suiteName: "group.com.tatoalo.aulos")`)
| Key | Written by | Value |
|---|---|---|
| `serverConfiguration` | `ConfigurationManager` | JSON `ServerConfiguration` |
| `downloadSettings` | `ConfigurationManager` | JSON `DownloadSettings` |
| `serverVersion` | `ConfigurationManager` | JSON `ServerVersion` (cache) |
| `availableFormats` | `ConfigurationManager` ← `formats` event | JSON `[ServerFormat]` |
| `authCookies` | `CookieManager` | JSON array of cookie property dicts |
| `handledAddTasks` | `AddNotificationPresenter` | `[String]` dedup keys, capped at last 50 |

Plus `UserDefaults.standard["aulos.lastConnectionWasConnected"]: Bool` (standard domain, not the group). `ConfigurationManager.reset()` removes the first four + clears cookies.

---

## 5. Features

### Queue view (`Aulos/Views/QueueView.swift`)
`ScrollView` → `LazyVStack` with three hard-coded sections, filtered client-side in `QueueViewModel`:
| Section | Filter |
|---|---|
| **"In Progress"** | `status.isActive` (pending + preparing + downloading) |
| **"Completed"** + a "Clear" button | `status == .finished` |
| **"Failed"** | `status == .error` |

States: error banner (with a refresh button) → `notAuthenticatedView` ("Login Required / Go to Settings and login first 👋🏻") → `loadingView` (only when `isLoading && isEmpty`) → `emptyStateView` ("Queue Empty") → list. `.refreshable { await viewModel.refresh() }`. Per-row actions: **open source URL** (`openURL(item.url)` → external browser), **delete**; failed rows add an **info** button opening an alert with `item.msg`. Rows are `.id("\(item.id)-\(item.status)")` with `.transition(.scale.combined(with: .opacity))`.

Progress rendering: linear `ProgressView(value: item.progress, total: 100)` only for `.downloading`/`.preparing`; the status line shows `progressText` (`"Starting..."` if 0, `"%.1f%%"` under 10%, else integer %), then `• speed`, then `• eta`. The `.downloading` status icon is an indeterminate circular `ProgressView`.

**UI update cadence:** `updated` events coalesce into `pendingUpdates[id]` and flush at most every **250 ms** (`SocketService.queueThrottledUpdate` / `applyPendingUpdates`). The comment is explicit that this is a *throttle*, not a debounce, because *"Progress events can arrive faster than this interval; canceling/restarting here prevents the UI from updating until the download pauses or completes."* `completed` bypasses the throttle (`updateItemImmediately`).

### Add download form
There is **no add form in the main app** — only the share extension. `AulosAPIService.addURL` exists but has no caller in the app target. Options exposed in the share sheet: **Format** and **Quality** menu pickers (server-driven lists) — that's it. No folder, no custom name prefix, no playlist / playlist-item options, no cookies/subtitles/embed toggles, no batch add. `auto_start` comes from the persisted setting, not from the sheet.

### Subscriptions / history / streaming
**Absent.** No subscriptions UI or endpoint, no separate history browser (the "Completed"/"Failed" sections are the only history surface, sourced from `/history` + socket), no in-app player (`AVPlayer` absent), no `ShareLink`/`UIActivityViewController`/`QuickLook`, no file download or `/download` link. The only "open" is `openURL(item.url)` on the **source** page, not the produced file.

### Delete / retry / start
- Delete one item: `POST /delete` with `ids: [item.url ?? item.id]`, `where: item.status.isActive ? "queue" : "done"`, then an **optimistic local removal** `socketService.removeItems(withIds: [item.id, deleteKey])`.
- Clear completed: `ids = completedItems.compactMap { $0.url }` (items without a `url` are silently skipped), `where: "done"`, then local removal by `id`.
- Comment at `QueueViewModel.swift:105`: *"Server uses URL as the key for delete operations, not video ID"*; at `:112`: *"Remove items locally since Socket.IO events are unreliable."*
- **No retry, no start, no pause, no reorder.**

### Settings (`Aulos/Views/SettingsView.swift`, 976 lines)
Sections: **Server** (protocol chip + host field, both locked once authenticated; info sheet; Login / Retry Connection / Logout rows), **Media Settings** (only when `connectionStatus == .connected`: Format, Quality, Auto-start), **About** (Appearance, Open on Launch, App Version — tap 5× to reveal the Developer Mode toggle, Server Version, yt-dlp Version, GitHub link to `https://github.com/tatoalo/metube_pot`), **Developer** (stress test), **Danger Zone** (Reset All Settings). Every field autosaves via `.onChange` → `saveDownloadSettings()`/`saveConfiguration()`; changing the server URL calls `socketService.disconnect()` **on every keystroke**. Sheets: `URLInfoSheet`, `ProtocolInfoSheet`, `ConnectionAttemptSheet` (attempt N of 3), `AulosInfoSheet`, `WebLoginView`. `HeaderView` embeds `StatusBadge`/`AnimatedDotsText` with a `TimelineView(.animation)` dot animation. Server/yt-dlp versions are fetched on every transition to `.connected` and cached in the app group.

### Share extension flow (the interesting part)
1. `ShareViewController.viewDidLoad` → `extractSharedURL`: loads **all** representations of the first attachment in parallel via `DispatchGroup` — `UTType.url`, `UTType.plainText`, `UTType.propertyList` (`NSExtensionJavaScriptPreprocessingResultsKey["URL"]`) — then picks `propertyList ?? plainText ?? url`. Rationale in comments: `UTType.url` can be canonical/truncated on SPA sites. All candidates are stored in `ShareDebugInfo.shared` and **rendered in the sheet as monospaced debug text** (`ShareView.swift:163`, marked "temporary").
2. `ShareView` shows: "Aulos Not Configured" / "Login Required" pre-checks, else a URL preview card (host-specific SF Symbol for youtube/vimeo/twitter-x/instagram/tiktok) and the Format/Quality pickers. `canAdd = sharedURL != nil && !isLoading && isConfigured && isAuthenticated`.
3. **Add tap** → `ShareViewModel.enqueueAdd()` (synchronous, returns `Bool`):
   - `sanitizeURL` — **only for hosts containing `youtube` or `youtu.be`**, strips query params `si, feature, pp, utm_source, utm_medium, utm_campaign`. All other hosts keep every param verbatim (explicit comment about `streamingcommunity`).
   - builds `URLRequest(url: addEndpointURL)`, `POST`, `httpShouldHandleCookies = false`, `Content-Type: application/json`, manual `Cookie` headers from `CookieManager`.
   - writes `{"url","quality","format","auto_start"}` to `<appGroupContainer>/AddPayloads/<UUID>.json`.
   - `BackgroundAddUploader().enqueue(request:bodyFile:taskDescription:)` → `uploadTask(with:fromFile:)` on a **process-wide singleton** background session, `taskDescription` = encoded `AddTaskContext`.
   - returns `true` → `viewModel.dismiss()` → `extensionContext.completeRequest(returningItems: nil)` **immediately**. Cancel → `cancelRequest(withError:)` with domain `com.metube.share`.
4. Background session (`BackgroundAddService.makeConfiguration()`): identifier **`com.tatoalo.aulos.add.background`**, `sharedContainerIdentifier = group.com.tatoalo.aulos`, `isDiscretionary = false`, `sessionSendsLaunchEvents = true`, `httpCookieStorage = nil`, `httpShouldSetCookies = false`. `BackgroundAddUploader` keeps a `static let holder` (one `URLSession` + one delegate per process) precisely because *"creating multiple URLSessions with the same background identifier in one process is unsupported"* — this was a bug fixed in `9d35038`.
5. Completion is handled by `BackgroundAddCompletionHandler` (shared by **both** processes): accumulates the body from **both** `URLSessionDataDelegate.didReceive` **and** `URLSessionDownloadDelegate.didFinishDownloadingTo` (commit `5fc472a` — background uploads deliver the body on either path), refuses redirects (`willPerformHTTPRedirection` → `completionHandler(nil)`), then on `didCompleteWithError`: classify → delete the staged payload file → `AddNotificationPresenter.postFailureIfNeeded(dedupKey: payloadPath)`.
6. `AddResultClassifier.classify(statusCode:body:transportError:)`:

| Condition | Outcome |
|---|---|
| transport error | `failure("Couldn't reach the server. Tap to retry in Aulos.")` |
| `statusCode == nil` | `failure("Add failed.")` |
| `401`, `403`, or any `300..<400` | `failure("Session expired — open Aulos to log in.")` |
| body JSON `status == "ok"` | `success` |
| body JSON `status != "ok"` | `failure(msg)` with a leading `"ERROR: "` stripped |
| 2xx with non-JSON body (HTML login page) | `failure(authMessage)` |
| other non-2xx | `failure("Add failed (HTTP \(code)).")` |

7. `AppDelegate` (main app) sets the `UNUserNotificationCenter` delegate, requests `[.alert, .sound]` authorization on launch, calls `BackgroundAddService.sweepStalePayloads()` (removes `AddPayloads` files older than 24 h), and eagerly recreates the background session (`dispatchPrecondition(condition: .onQueue(.main))` — main-thread contract from commit `8d3e02f`). `handleEventsForBackgroundURLSession` stores the system handler and fires it from `urlSessionDidFinishEvents`.

### Notifications
Local only. Title **"Couldn't add to Aulos"**, body `"<host>: <reason>"`, `sound = .default`, no trigger (immediate), random UUID identifier. **Failures only — success is silent.** Foreground presentation forced via `willPresent` → `[.banner, .sound, .list]`. Cross-process dedup via app-group `handledAddTasks` keyed on the payload path (fallback `"task-<taskIdentifier>"`). No APNs, no notification categories/actions (so "Tap to retry in Aulos" only opens the app).

### Widgets / background tasks
**None.** No WidgetKit, no `BGTaskScheduler`/`BGAppRefreshTask`, no Live Activities/ActivityKit. The only background work is the `URLSession` background upload.

### Developer stress test (`Aulos/Services/StressTestService.swift`)
Hidden behind 5 taps on App Version + a Developer Mode toggle. Waits 10 s, spawns 50 synthetic `stress-test-<UUID>` items over ~30 s, each driven by a `Timer` at a random **50–200 ms** interval (comment: *"Real yt-dlp sends updates every 50-200ms"*), ~15% fail at a random 20–80%. UI pushes are throttled to 100 ms with an immediate flush on status transitions. `SocketService` deliberately preserves `stress-test-` prefixed items across every server refresh (`fetchInitialState`, `handleAllEvent`, `handleAllEventFromArray`). This service is a strong signal that **progress-update volume is the app's known performance risk**.

---

## 6. Pain points (evidence from code/docs/commits)

**Backend-limitation workarounds**
1. **`GET /history` is used instead of the socket `all` event.** `SocketService.swift:217`: *"Fetch initial queue state via HTTP since Socket.IO 'all' event is unreliable."* So every connect costs a full HTTP round trip on top of the socket handshake, and the socket `all` handler survives only as dead-ish fallback code with **four** parsing strategies (`String` → `SocketAllResponse`, `[Any]` tuple, `data`-as-tuple, `NSArray` iteration) plus a hand-rolled `AnyCodable`.
2. **Socket events are distrusted for deletion.** `QueueViewModel.swift:112`: *"Remove items locally since Socket.IO events are unreliable."* Deletes are applied optimistically client-side; if the server also emits `canceled`/`cleared` the removal is idempotent, but a failed server delete leaves the UI lying until the next refresh.
3. **Unstable identity.** `removeItems(withIds:)` matches `id` **or** `url`; delete uses `url ?? id`; `QueueItem.init(from:)` falls back `id → url → UUID()`. The client clearly cannot rely on one stable key. `clearCompleted` drops any completed item whose `url` is nil.
4. **Synchronous `/add`.** The whole fire-and-forget design exists because *"the backend's `/add` … runs yt-dlp metadata extraction **synchronously** before responding — a multi-second wait (worse with POT tokens). The user stares at a spinner the whole time."* (design doc). Cost: a background-session dance, app-group payload staging, cross-process dedup, an orphan-file sweeper, and 3 follow-up bug-fix commits (`674efe4`, `5fc472a`, `9d35038`).
5. **HTTP 200 for errors + 3xx for auth.** The classifier cannot trust status codes: *"success/failure MUST be determined by parsing the JSON body, not by the status code alone"*; redirects are refused and a 2xx-with-HTML body is treated as an expired session.
6. **`eta` type instability** — decoded as `String`, `Int`, or `Double`; and five numeric fields need `decodeFlexibleDoubleIfPresent` (Double/Int/String).
7. **`status` value drift** — `"finished"` and `"done"` both map to `.finished`; unknown values silently become `.pending` with a debug print.

**Flicker / jitter / responsiveness**
8. 250 ms progress throttle exists explicitly *"to prevent UI flickering during rapid progress updates"* — and the code comment records a prior **debounce** bug where the UI froze until a download paused.
9. `applyPendingUpdates()` inserts updates for **unknown ids at index 0**: `nextItems.insert(contentsOf: updates.values.filter { !updatedIds.contains($0.id) }, at: 0)`. `updates.values` iteration order is a dictionary order, so a burst can reorder rows nondeterministically.
10. **Ordering is inconsistent between sources.** `HistoryResponse.allItems` sorts alphabetically by title; socket-driven `added`/`applyPendingUpdates` prepend. So the list reshuffles between a refresh and live updates.
11. **Row identity churns on status change:** `.id("\(item.id)-\(item.status)")` combined with `.transition(.scale.combined(with: .opacity))` destroys and recreates each row on every `pending→preparing→downloading→finished` step — guaranteed visual pop, and the row also jumps between sections.
12. **Connection-status theater:** three timers exist purely to hide latency — `testingStateDelay` 300 ms, `connectedHoldDelay` 900 ms, and a persisted `aulos.lastConnectionWasConnected` flag that makes the service **boot into `.connected`** before any socket exists. Cheap to get wrong; the app can display "Connected" while offline.
13. **Debug logging on hot paths (shipping code, not `#if DEBUG`):** `onAny` logs every non-`updated` event; `handleAllEvent` prints `data.first` — potentially the *entire* payload; `fetchInitialState` prints the first 1000 chars of `/history`; `QueueViewModel.setupBindings` builds a formatted per-item summary **string on every 250 ms flush**; `failedItems` (a computed property recomputed on every render) prints one line per failed item. `SocketService.swift:296` admits payload logging "is large enough to stall UI updates while debugging on device".
14. **Refresh = full teardown.** Pull-to-refresh disconnects the socket, sleeps 100 ms, reconnects, and re-fetches all of `/history`. There is no cheap incremental refresh.
15. **`saveConfiguration()` on every URL keystroke calls `socketService.disconnect()`** — typing a server URL repeatedly tears the socket down.
16. No exponential backoff (fixed 5 s × 3), then the user must tap "Retry Connection" manually; `.reconnectAttempt` handling is dead because `reconnects(false)`.
17. `ConfigurationManager`/`CookieManager` construct `UserDefaults(suiteName:)` and JSON-decode on **every property read**; `CookieManager.getCookies(for:)` deserializes the whole cookie array per request, and `AulosAPIService.storeCookies` re-reads + rewrites it on every response.
18. Duplicated networking: `addCookies`/`storeCookies` are copy-pasted into `AulosAPIService` and `QueueService`, and `/history` fetching is implemented twice (`QueueService.fetchHistory` — which nothing calls — and `SocketService.fetchInitialState`).

**Missing features / rough edges**
19. Share-sheet **debug text is still rendered in production UI** (`ShareView.swift:163`, comment "temporary"), and `ShareDebugInfo.shared` is a mutable global `static var`.
20. `isLoading`/the `ProgressView` branch in the share sheet's Add button is now permanently false — dead UI (the plan called for removing it).
21. `README.md` is entirely pre-rebrand and wrong about the app group, URL scheme, service names, and file layout.
22. `isAuthenticated` = "some cookie exists", so an expired session presents as logged-in until a request 401s; `areCookiesValid()` is written but unused.
23. `WebLoginView` heuristics are brittle: skips any host containing `"auth"`, requires a cookie named `*session*`/`*auth*`.
24. No `/start` support → **a failed download cannot be retried**, only deleted and re-added; no pause/resume/reorder; no folder/prefix/playlist controls; no subscriptions; no way to reach the downloaded file.
25. `HistoryResponse` requires all three of `queue`/`done`/`pending`; a missing key silently empties the queue (`fetchInitialState` swallows `DecodingError` with a print).
26. AulosCore's 14 unit tests aren't in the Xcode scheme's `<Testables>`, so `xcodebuild test` runs nothing.

---

## 7. What the backend should provide to make this app feel snappy

Ranked by expected payoff, each tied to the client code it would delete.

1. **Async `/add` that returns immediately** (`{"status":"ok","id":"<stable-id>"}` before yt-dlp extraction, then emit `added` and `updated`). This is the single biggest win: it removes the entire background-session/app-group/staging/dedup/sweeper machinery (`BackgroundAddService`, `BackgroundAddUploader`, `BackgroundAddCompletionHandler`, `AddResultClassifier`, `AddNotificationPresenter` and their tests), lets the share sheet show real success/failure inline, and enables an in-app add form.
2. **Honest HTTP semantics.** `4xx` for client errors, `401` (never `303`) for auth, a JSON error envelope on non-2xx. Today the client must parse the body and treat "2xx with non-JSON" as an expired session.
3. **One stable, server-assigned `id` per item, immutable for its lifetime, and used as *the* key for `/delete`, `canceled`, `cleared` and every event.** Kill the `url`-as-key duality. This removes the `id → url → UUID` fallback chain, the dual-key `removeItems`, `url ?? id` delete keys, and the "skip items with nil url" bug in `clearCompleted`.
4. **A reliable, complete `all` (or a `GET /history`-equivalent) snapshot on connect** so `fetchInitialState()`'s HTTP round trip can go away. Emit it in **the same shape** as `/history` (flat objects), not `[key, info]` pairs — that alone deletes `SocketAllResponse`, `AnyCodable`, `handleAllEventFromArray`, `parseAllEventManually` (~180 lines and 4 fallback paths).
5. **Server-side batched progress deltas instead of per-item full objects.** One `updated` frame every ~250–500 ms carrying an array of *changed fields only* — e.g. `[{"id":…,"percent":…,"speed":…,"eta":…,"status":…}, …]` — with the full metadata sent only on `added`/`completed`. The client already coalesces at 250 ms and explicitly refuses to log payloads because they carry "the full yt-dlp metadata tree". Server-side batching lets the client drop its throttle, its `pendingUpdates` map, and the index-0 insertion that reorders rows.
6. **A server-defined, stable sort key** (e.g. monotonically increasing `seq` or `created_at`) on every item, and a documented ordering for `queue`/`pending`/`done`. That fixes the title-sort-vs-prepend inconsistency and eliminates row reshuffling — the main source of perceived flicker.
7. **Consistent field types.** `eta` always integer seconds (or always a string), `percent` always a `Double` 0–100, byte counts always numbers. Deletes `decodeFlexibleDoubleIfPresent` and the 3-branch `eta` decoder.
8. **A closed, documented `status` vocabulary** (pick `finished` **or** `done`, and declare whether `canceled`/`skipped` can appear). Today unknown values silently become `pending`.
9. **`ETag` / `Last-Modified` + `If-None-Match` on `GET /history`** (and ideally a `?since=<seq>` delta query) so pull-to-refresh is a `304` instead of a full payload — and so refresh doesn't need to tear down the socket.
10. **A cheap, unauthenticated-ish liveness/config endpoint** (or make `/version` extremely cheap and include the format catalog + capability flags). Then the client can drop the 300/900 ms "hide the latency" timers and the persisted optimistic-connected flag, and stop fetching `/version` on every reconnect.
11. **A `/start` (retry) endpoint plus `pause`/`resume`**, so a failed item is one tap from retrying instead of delete-and-reshare.
12. **A server-emitted terminal event with the final error string already cleaned** (no leading `"ERROR: "`), so `AddResultClassifier.cleanMessage` and the client-side prefix stripping can go.
13. **Push (APNs) or at least a completion webhook**, so "download finished/failed" reaches the user without the app being foregrounded. Right now the app disconnects the socket on `.background` and has no notification path except the background-upload failure notification.
14. **File access for finished items** — a stable `download_url`/`filename` field and a range-capable, cookie-authenticated file endpoint — to unlock open/stream/share, which the client currently cannot do at all.
15. **`URL_PREFIX` echoed in `/version`** (e.g. `{"url_prefix": "/metube/"}`) so the client can validate its constructed `/add`, `/history`, `/delete` and `socket.io` paths instead of guessing by string concatenation.
16. **Richer `/add` options** (`folder`, `custom_name_prefix`, `playlist_strict_mode`, `playlist_item_limit`, `auto_start`) advertised via the same server-driven mechanism as `formats`, so the share sheet can grow beyond Format/Quality without a client release.

---

## ~25-line summary

1. **App**: "Aulos" (`com.tatoalo.aulos`), SwiftUI, iOS 17+, Swift 5.9, XcodeGen (`project.yml`), one scheme `Aulos` building app + share extension. iPhone `TabView` / iPad `NavigationSplitView`.
2. **Build**: `xcodegen generate` then `xcodebuild -project Aulos.xcodeproj -scheme Aulos -destination 'platform=iOS Simulator,name=iPhone 17' -quiet build`; core tests via `cd Packages/AulosCore && swift test` (not in the Xcode scheme).
3. **Modules**: local SwiftPM `AulosCore` (models + config + cookies + REST actors + background-add stack) shared by app and extension; app-only `SocketService`, view models, views.
4. **Deps**: `socket.io-client-swift` 16.1.1 (+ Starscream 4.0.8) — app target only.
5. **Sharing**: app group `group.com.tatoalo.aulos` on both targets; **no keychain group** — session cookies live in app-group `UserDefaults` key `authCookies`.
6. **Protocol**: Socket.IO v4 over forced WebSockets (`.forceWebsockets(true)`, `.reconnects(false)`, `.connectParams(["EIO":"4"])`, path `<prefix>/socket.io`) for live state + plain JSON REST for all mutations. No SSE, no raw WebSocket, no client→server emits.
7. **REST**: `POST /add` `{url,quality,format,auto_start}`; `GET /version` → `{version,"yt-dlp"}`; `GET /history` → `{queue,done,pending}` (three **flat** arrays of items, all required); `POST /delete` `{ids:[…],where:"done"|"queue"}`. Paths are appended to the configured URL, so `URL_PREFIX` works. `/start`, batch add, subscriptions: unused/absent.
8. **Socket events consumed**: `all` (parsed as `[[key,info],…]` pairs, 4 fallback parsers), `updated`, `added`, `completed`, `canceled` (bare id string), `cleared` (bare id string), `formats` (→ `[{id,text,qualities:[{id,text}]}]`, cached in the app group).
9. **`QueueItem` keys**: `id,url,title,status,percent,eta,msg,speed,downloaded_bytes,total_bytes,total_bytes_estimate,fragment_index,fragment_count`; `eta` accepts String/Int/Double, the numerics accept Double/Int/String.
10. **`status` raw values**: `pending|preparing|downloading|finished|done|error`; `done`→`finished`, unknown→`pending`.
11. **Auth**: cookie/SSO only (no basic auth, no headers). `WKWebView` login harvests cookies named `*session*`/`*auth*`; `isAuthenticated` == "any cookie exists". All sessions are `.ephemeral` with cookie handling disabled and `Cookie` headers set manually.
12. **Share extension mechanism**: JS preprocessing (`window.location.href`) + parallel `UTType.url`/`plainText`/`propertyList` extraction, prefer the plist URL; YouTube-only tracking-param stripping; then body JSON is **staged to a file in the app-group container** and enqueued as a **background `URLSession` upload task** (id `com.tatoalo.aulos.add.background`, `sharedContainerIdentifier` = the app group) — the sheet dismisses instantly. Completion is delivered to whichever process survives (extension or relaunched app via `handleEventsForBackgroundURLSession`), classified by `AddResultClassifier`, and **failures only** post a local notification "Couldn't add to Aulos"; success is silent. Dedup via app-group `handledAddTasks`; orphan payloads swept after 24 h.
13. **Why**: `/add` is synchronous yt-dlp extraction (multi-second, worse with POT) — documented in `docs/superpowers/specs/2026-06-28-fast-share-add-design.md`.
14. **Progress rendering**: `updated` events coalesce into a dict and flush at most every **250 ms** (throttle, not debounce — a previous debounce froze the UI); `completed` bypasses it.
15. **Known flicker sources**: rows keyed `"\(id)-\(status)"` with scale+opacity transitions; `/history` sorts by title while socket updates prepend; unknown-id updates inserted at index 0 in dictionary order.
16. **Connection UX**: hand-rolled retry (3 attempts, fixed 5 s, 10 s timeout), a persisted `aulos.lastConnectionWasConnected` flag that boots the UI into "Connected", and 300 ms/900 ms delays to hide "testing".
17. **Explicit backend distrust in code**: *"Socket.IO 'all' event is unreliable"* → HTTP `/history` on every connect; *"Socket.IO events are unreliable"* → optimistic local deletes.
18. **Missing features**: no add form in the app, no folder/prefix/playlist options, no subscriptions, no retry/start/pause, no file open/stream/share, no widgets, no `BGTaskScheduler`, no push.
19. **Shipping debug cruft**: `onAny` event logging, raw `/history` and `all` payload prints, a per-flush formatted summary string in `QueueViewModel`, prints inside the computed `failedItems`, and monospaced share-extension debug text rendered in the production sheet.
20. **Dev tooling**: `StressTestService` fakes 50 items updating every 50–200 ms with a 15% failure rate — the team's own model of the load the queue must absorb.
21. **README.md is stale** (pre-rebrand: `group.com.metube.app`, `metube://`, `MeTubeAPIService`).
22. **Top backend asks**: async `/add` returning a stable id; honest status codes; one immutable server id used everywhere; a reliable `all` snapshot in the same shape as `/history`; **server-batched progress deltas (changed fields only)**; a stable server-side sort key; consistent field types and a closed status vocabulary; ETag/`?since=` on `/history`; `/start` for retry; APNs for completion; `download_url` for file access.
