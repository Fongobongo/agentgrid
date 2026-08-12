-- opencode profile bundle-pinned skills (item 10): a profile may declare a
-- set of agentgrid skill names that the operator expects to be trusted for
-- this profile to run with its intended behaviour. Stored as a JSON array of
-- strings (canonical JSON, sorted) so two identical pin sets hash the same.
-- NULL = no pin set (default). The node-side apply reconciles this list
-- against the trust ledger and surfaces untrusted pins in the apply audit
-- (fail-loud warning, never blocks the config write — pins are a hint about
-- expected operator trust state, not a hard gate on opencode config).
ALTER TABLE opencode_profiles ADD COLUMN pinned_skills_json TEXT;

-- Apply audit: the node-side reconcile reports any pinned skills it found
-- in the worktree/home but NOT in the trust ledger, so an operator sees from
-- the dashboard which pinned skills are still acting as hints (untrusted).
-- JSON array of strings; NULL when the profile had no pins (or the node is a
-- legacy build that did not reconcile).
ALTER TABLE opencode_config_audit ADD COLUMN pinned_untrusted_json TEXT;
