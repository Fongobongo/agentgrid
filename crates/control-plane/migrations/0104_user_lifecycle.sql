-- Finding 7: user lifecycle.
--
-- `sessions_gen` is the "revoke every session issued so far" counter. A
-- password change, a role change, or an explicit revoke increments it; the
-- value is stamped into the JWT at login and `verify_token` refuses a token
-- whose generation is behind the row. A timestamp would have needed sub-second
-- precision to separate "minted a moment before the change" from "minted a
-- moment after it" — a counter cannot get that wrong.
ALTER TABLE users ADD COLUMN sessions_gen INTEGER NOT NULL DEFAULT 0;
