-- =============================================================
--  Scrobblr — Account deletion
--
--  A deletion revokes every credential and hides the account for a grace
--  period during which a login restores it; the worker then erases it
--  (db::queries::accounts).
-- =============================================================

-- When the worker erases the account; a login before then clears it.
ALTER TABLE users ADD COLUMN delete_after TIMESTAMPTZ;
-- From then on, a login can't restore it.
ALTER TABLE users ADD COLUMN erase_started_at TIMESTAMPTZ;

CREATE INDEX idx_users_delete_after ON users (delete_after) WHERE delete_after IS NOT NULL;

-- An erased account counted in it: refresh it now.
ALTER TABLE ranking_snapshots ADD COLUMN invalidated BOOLEAN NOT NULL DEFAULT FALSE;
