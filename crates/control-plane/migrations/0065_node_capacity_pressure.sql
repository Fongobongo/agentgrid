-- Plan 2.14 (#27): capacity pressure — node reports its own headroom
-- (rss_mib / cpu_load_pct / active_rss_mib) so the scheduler can reject an
-- assignment when the node is at capacity, instead of letting the attempt
-- OOM on the device.
--
-- Why columns on nodes (not a separate metrics table): the scheduler reads
-- the latest sample in the same transaction that picks the next task —
-- keeping the headroom live on the nodes row keeps the read path O(1)
-- with a single JOIN. The heartbeat route is the writer.
ALTER TABLE nodes ADD COLUMN rss_mib INTEGER;
ALTER TABLE nodes ADD COLUMN cpu_load_pct INTEGER; -- 0..100, from load_avg normalized
ALTER TABLE nodes ADD COLUMN active_rss_mib INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN max_rss_mib INTEGER NOT NULL DEFAULT 1024;

-- Plan 2.14 (#27): metric how many assignment requests were refused due to
-- capacity pressure. Update via scheduler.
CREATE TABLE IF NOT EXISTS metrics_capacity_pressure (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    node_id TEXT NOT NULL,
    threshold_mib INTEGER NOT NULL,
    active_mib INTEGER NOT NULL,
    forecast_mib INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_metrics_capacity_pressure_at
    ON metrics_capacity_pressure(at DESC);
