-- Successful downloads outlive queue records and files removed outside Aulos.
CREATE TABLE download_history (
  canonical_key TEXT NOT NULL,
  download_type TEXT NOT NULL,
  downloaded_at INTEGER NOT NULL,
  PRIMARY KEY (canonical_key, download_type)
) WITHOUT ROWID, STRICT;

INSERT INTO download_history (canonical_key, download_type, downloaded_at)
SELECT canonical_key, json_extract(request_json, '$.selection.download_type'),
       MAX(COALESCE(finished_at, updated_at))
FROM items
WHERE kind = 'item' AND status = 'finished'
GROUP BY canonical_key, json_extract(request_json, '$.selection.download_type');

CREATE TRIGGER remember_inserted_download AFTER INSERT ON items
WHEN NEW.kind = 'item' AND NEW.status = 'finished'
BEGIN
  INSERT INTO download_history (canonical_key, download_type, downloaded_at)
  VALUES (NEW.canonical_key, json_extract(NEW.request_json, '$.selection.download_type'),
          COALESCE(NEW.finished_at, NEW.updated_at))
  ON CONFLICT (canonical_key, download_type) DO UPDATE SET
    downloaded_at = MAX(download_history.downloaded_at, excluded.downloaded_at);
END;

CREATE TRIGGER remember_finished_download AFTER UPDATE OF status ON items
WHEN NEW.kind = 'item' AND NEW.status = 'finished'
BEGIN
  INSERT INTO download_history (canonical_key, download_type, downloaded_at)
  VALUES (NEW.canonical_key, json_extract(NEW.request_json, '$.selection.download_type'),
          COALESCE(NEW.finished_at, NEW.updated_at))
  ON CONFLICT (canonical_key, download_type) DO UPDATE SET
    downloaded_at = MAX(download_history.downloaded_at, excluded.downloaded_at);
END;

-- Recover labels for existing downloads whose subscription still exists.
UPDATE items SET source_json = json_set(source_json, '$.name', (
  SELECT name FROM subscriptions WHERE id = json_extract(items.source_json, '$.ref')
))
WHERE json_extract(source_json, '$.kind') = 'subscription'
  AND json_extract(source_json, '$.name') IS NULL
  AND EXISTS (SELECT 1 FROM subscriptions WHERE id = json_extract(items.source_json, '$.ref'));
