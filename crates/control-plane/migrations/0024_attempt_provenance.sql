-- Stage 13: provenance — an optional external-origin link on each attempt.
-- Stored as a JSON ProvenanceRecord ({originator, external_id, optional label});
-- only identifiers, never secrets, so safe to persist + surface in the UI.
ALTER TABLE attempts ADD COLUMN provenance TEXT;
