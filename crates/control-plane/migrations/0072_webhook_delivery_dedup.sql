-- GitHub webhook delivery dedup (audit CP-4): GitHub delivery is
-- at-least-once (response lost after commit, timeout, manual redelivery),
-- and every replay used to mint a fresh task — duplicate full agent runs
-- for the same issue / CI failure / PR. The delivery GUID is recorded with
-- INSERT OR IGNORE before any task creation; a row that already exists
-- means the delivery was processed and the replay is dropped.
--
-- `seen_at` enables opportunistic pruning of ancient GUIDs (GitHub GUIDs
-- are unique per delivery and never reused, so any retention window is
-- safe; the default maintenance sweep may trim rows older than 30 days).
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    guid TEXT PRIMARY KEY,
    seen_at TEXT NOT NULL
);
