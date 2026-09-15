-- Plan 6.11: systemd transient scopes are usable on this node
-- (systemd-run --user answers + cgroups v2 mounted). The scope sandbox
-- backend is optional; false on legacy nodes / hosts without systemd.
ALTER TABLE nodes ADD COLUMN systemd_scope_supported INTEGER NOT NULL DEFAULT 0;
