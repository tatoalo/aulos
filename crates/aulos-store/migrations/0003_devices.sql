-- 0003 — the APNs push registrations: `devices` and `live_activities` (DESIGN §25, PROTOCOL §4.8).
--
-- Two tables, both written only by `PUT`/`DELETE api/v2/devices/…` and by the notifier's own
-- pruning when APNs reports a token dead (`400 BadDeviceToken`, `410 Unregistered`).
--
-- `devices.token` is the natural key: the APNs device token itself, lowercase hex. There is no
-- surrogate id because there is nothing to join on — a device *is* its token, and a token that
-- changes is a different device the app re-registers.
--
-- `live_activities.item_id` is deliberately **not** a foreign key to `items(id)`. A Live Activity
-- is registered by the app the moment it starts one, which can race ahead of the item row the
-- server has not written yet, and PROTOCOL §4.8 promises the route accepts any well-formed ULID.
-- The cleanup that a foreign key would have given for free is done explicitly instead: deleting an
-- item (`WriteOp::DeleteItems`) also deletes the Live Activity rows for it *and for its children*,
-- and the notifier calls `remove_live_activities_for` after the final `end` push.
--
-- `device_token` **is** a foreign key, with `ON DELETE CASCADE`: forgetting a device must forget
-- every activity registered under it, and `PRAGMA foreign_keys = ON` is in the store's pragma set.
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
) STRICT;

CREATE TABLE live_activities (
  device_token  TEXT    NOT NULL REFERENCES devices(token) ON DELETE CASCADE,
  item_id       TEXT    NOT NULL,
  update_token  TEXT    NOT NULL,
  environment   TEXT    NOT NULL CHECK (environment IN ('sandbox','production')),
  registered_at INTEGER NOT NULL,
  PRIMARY KEY (device_token, item_id)
) WITHOUT ROWID, STRICT;

-- The notifier's hot read is "every activity tracking this item", once per status change.
CREATE INDEX live_activities_item ON live_activities(item_id);

-- The push-to-start fan-out reads "every device that offered a start token".
CREATE INDEX devices_start_token ON devices(live_activity_start_token)
  WHERE live_activity_start_token IS NOT NULL;
