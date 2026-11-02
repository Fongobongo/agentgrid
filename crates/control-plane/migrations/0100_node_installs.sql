-- Install attempts, durable.
--
-- A host that was provisioned and never came up is the one failure mode the
-- node table cannot show: `nodes` is keyed by enrollment, so a host whose
-- daemon never enrolled has no row anywhere. These jobs were in memory, which
-- meant a control-plane restart erased the only record that anyone had tried.
--
-- `steps` is the JSON array the panel renders; it is a log, not a state
-- machine, so it is stored whole rather than in its own table.
CREATE TABLE IF NOT EXISTS node_installs (
    id TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    name TEXT NOT NULL,
    -- running | succeeded | failed. A row left at `running` means the process
    -- that was doing the work never reported an outcome.
    state TEXT NOT NULL,
    steps TEXT NOT NULL DEFAULT '[]',
    error TEXT,
    server_url TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_node_installs_started
    ON node_installs (started_at DESC);
