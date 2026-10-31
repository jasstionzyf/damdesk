-- 0003 seed_settings — 默认设置种子（INSERT OR IGNORE，不覆盖已有值）

INSERT OR IGNORE INTO settings (key, value, updatedAt) VALUES
  ('schema_version',  '3',            unixepoch()),
  ('store_mode',      '"reference"',  unixepoch()),
  ('active_embedder', 'null',         unixepoch()),
  ('active_tagger',   'null',         unixepoch());
