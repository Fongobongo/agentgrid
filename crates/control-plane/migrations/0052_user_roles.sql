-- Plan 5.2: RBAC roles. admin = full access; operator = view + approvals only.
-- Existing users keep admin (the only role before this migration).
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'admin';
