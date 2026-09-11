CREATE TABLE devices (
  token                     TEXT    PRIMARY KEY,
  platform                  TEXT    NOT NULL,
  bundle_id                 TEXT    NOT NULL,
  environment               TEXT    NOT NULL CHECK (environment IN ('sandbox','production')),
  alerts                    INTEGER NOT NULL DEFAULT 1,
  live_activity_start_token TEXT,
  app_version               TEXT,
  registered_at             INTEGER NOT NULL,
  last_seen_at              INTEGER NOT NULL
, install_id TEXT) STRICT;
CREATE TABLE items (
  id                  TEXT    PRIMARY KEY,
  kind                TEXT    NOT NULL CHECK (kind IN ('item','group')),
  group_id            TEXT    REFERENCES items(id) ON DELETE CASCADE,
  group_index         INTEGER,
  ord                 INTEGER NOT NULL UNIQUE,
  url                 TEXT    NOT NULL,
  canonical_key       TEXT    NOT NULL,
  provider            TEXT,
  media_id            TEXT,
  title               TEXT    NOT NULL,
  status              TEXT    NOT NULL CHECK (status IN
                        ('queued','resolving','preparing','downloading',
                         'postprocessing','finished','error','canceled')),
  auto_start          INTEGER NOT NULL DEFAULT 1,
  msg                 TEXT,
  error_json          TEXT,
  request_json        TEXT    NOT NULL,
  entry_json          TEXT,
  filename            TEXT,
  size                INTEGER,
  chapter_files_json  TEXT    NOT NULL DEFAULT '[]',
  subtitle_files_json TEXT    NOT NULL DEFAULT '[]',
  source_json         TEXT    NOT NULL,
  attempt             INTEGER NOT NULL DEFAULT 0,
  children_total      INTEGER,
  created_at          INTEGER NOT NULL,
  started_at          INTEGER,
  finished_at         INTEGER,
  updated_at          INTEGER NOT NULL,
  clear_after         INTEGER
) STRICT;
CREATE TABLE kv (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
CREATE TABLE live_activities (
  device_token  TEXT    NOT NULL REFERENCES devices(token) ON DELETE CASCADE,
  item_id       TEXT    NOT NULL,
  update_token  TEXT    NOT NULL,
  environment   TEXT    NOT NULL CHECK (environment IN ('sandbox','production')),
  registered_at INTEGER NOT NULL,
  PRIMARY KEY (device_token, item_id)
) WITHOUT ROWID, STRICT;
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
CREATE TABLE subscription_seen (
  subscription_id TEXT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  media_id        TEXT NOT NULL,
  seen_at         INTEGER NOT NULL,
  PRIMARY KEY (subscription_id, media_id)
) WITHOUT ROWID, STRICT;
CREATE TABLE subscriptions (
  id                     TEXT PRIMARY KEY,
  name                   TEXT NOT NULL,
  url                    TEXT NOT NULL UNIQUE,
  enabled                INTEGER NOT NULL DEFAULT 1,
  check_interval_minutes INTEGER NOT NULL DEFAULT 60,
  request_json           TEXT NOT NULL,
  last_checked           INTEGER,
  last_success           INTEGER,
  next_due               INTEGER,
  consecutive_failures   INTEGER NOT NULL DEFAULT 0,
  error                  TEXT,
  created_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL
) STRICT;
CREATE TABLE telegram_chats (
  chat_id     INTEGER PRIMARY KEY,
  config_json TEXT NOT NULL,
  updated_at  INTEGER NOT NULL
) STRICT;
CREATE INDEX devices_start_token ON devices(live_activity_start_token)
  WHERE live_activity_start_token IS NOT NULL;
CREATE INDEX items_canonical     ON items(canonical_key);
CREATE INDEX items_clear_after   ON items(clear_after) WHERE clear_after IS NOT NULL;
CREATE INDEX items_finished_at   ON items(finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX items_group         ON items(group_id, group_index);
CREATE INDEX items_media_id      ON items(media_id) WHERE media_id IS NOT NULL;
CREATE INDEX items_ord           ON items(ord);
CREATE INDEX items_status_ord    ON items(status, ord);
CREATE INDEX items_url           ON items(url);
CREATE INDEX live_activities_item ON live_activities(item_id);
CREATE INDEX subscription_seen_age ON subscription_seen(subscription_id, seen_at DESC);
CREATE INDEX subscriptions_due ON subscriptions(enabled, next_due);
