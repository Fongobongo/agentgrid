-- opencode profile TTL: nullable absolute expiry (RFC3339 UTC). When the
-- janitor ticks past it the profile is deleted exactly like a manual
-- DELETE (nodes are re-pointed off via ON DELETE SET NULL and woken by a
-- ConfigUpdate clear push), so a temporary experiment profile cannot
-- outlive its purpose. NULL = no expiry (default).
ALTER TABLE opencode_profiles ADD COLUMN expires_at TEXT;
