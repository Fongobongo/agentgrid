-- Reachability (ADR 0026 + ADR 0012): how a node reaches the control plane.
--
-- Two independent facts, deliberately not one column:
--
--   relay_hops   OBSERVED by the control plane from the `x-agentgrid-relay-hops`
--                header a relay stamps on every request it forwards. 0 = the
--                heartbeat arrived without a hop counter. NULL = no heartbeat
--                since this column existed (a row that predates the migration
--                and has not beaten since) — not the same claim as 0.
--
--   egress_proxy CLAIMED by the node in its heartbeat: its egress proxy pool
--                (ADR 0012) has an entry in rotation, so the request went
--                through a forward proxy. NULL = the node never said (a binary
--                older than the field). The control plane cannot see a forward
--                proxy, so this one has no observer.
--
-- Both are nullable on purpose: "nobody has told us" and "we were told: no"
-- must stay distinguishable, the same rule `adapter_env_delivery` follows.
ALTER TABLE nodes ADD COLUMN relay_hops INTEGER;
ALTER TABLE nodes ADD COLUMN egress_proxy INTEGER;
