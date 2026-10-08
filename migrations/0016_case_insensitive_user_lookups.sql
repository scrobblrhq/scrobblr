-- =============================================================
--  Scrobblr — Indexes for case-insensitive user lookups
--
--  Usernames and emails are looked up as lower(col) = lower($1) (every
--  profile, chart and login request), which no index served: each was a
--  sequential scan of users. The plain indexes dropped here duplicate the
--  unique constraints' own.
-- =============================================================

CREATE INDEX idx_users_username_lower ON users (lower(username));
CREATE INDEX idx_users_email_lower    ON users (lower(email));

DROP INDEX idx_users_username;
DROP INDEX idx_users_email;
DROP INDEX idx_tokens_hash;
