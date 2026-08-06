-- Hardening P2 item 809: add network_mode column to nodes table
-- Values: "none" | "restricted" | "unrestricted"

ALTER TABLE nodes ADD COLUMN network_mode TEXT NOT NULL DEFAULT 'none';