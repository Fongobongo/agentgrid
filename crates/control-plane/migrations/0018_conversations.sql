-- Conversations: stateful multi-turn chat sessions routed through the control
-- plane to a coding agent on some node. One row per conversation; messages keep
-- the shared context the control plane composes into each task's prompt so any
-- node that picks the task up sees the full history.
CREATE TABLE conversations (
    id TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    repository TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE conversation_messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,           -- 'user' | 'assistant'
    content TEXT NOT NULL DEFAULT '',
    task_id TEXT,                 -- the task that produced (assistant) / carried (user) this message
    created_at TEXT NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations (id)
);

CREATE INDEX idx_conv_msgs ON conversation_messages (conversation_id, seq);
