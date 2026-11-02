-- One open intent per host, enforced by the table.
--
-- `issued` rows are exempt from retention on purpose: they answer "was this
-- host ever configured?", which stays true however old the row gets. That made
-- a repeated mint for the same host the one way `node_installs` could grow
-- without bound, and the guard against it lived in the control plane's job
-- list — which is capped at twenty entries, so the guard stopped working
-- exactly when the list was busy with other installs. A guard that is correct
-- only while its window happens to hold the row is not a guard; the constraint
-- belongs in the table.
--
-- Partial index: only *open* intents are unique. A host may accumulate any
-- number of finished attempts (they age out) and may have at most one
-- unresolved one.
--
-- Existing databases can already hold duplicates — the old guard was advisory
-- — so collapse them first, keeping the newest row per (host, name): that is
-- the one an operator would expect to see, and `rowid` follows insertion order
-- because the id is a UUID and cannot be compared.
DELETE FROM node_installs
 WHERE state = 'issued'
   AND rowid NOT IN (
       SELECT MAX(rowid) FROM node_installs WHERE state = 'issued' GROUP BY host, name
   );

CREATE UNIQUE INDEX IF NOT EXISTS idx_node_installs_open_intent
    ON node_installs (host, name) WHERE state = 'issued';
