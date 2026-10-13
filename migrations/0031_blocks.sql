-- =============================================================
--  Scrobblr — Blocks and removed followers
--
--  A block ends the follows between two accounts, and while it stands
--  neither follows the other; the blocker is hidden from the blocked
--  account, which finds no such account, and the blocked one from the
--  blocker's lists of people (docs/social.md). A removed follower can't
--  follow the account again until `until`.
-- =============================================================

CREATE TABLE user_blocks (
    blocker_id  BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    blocked_id  BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),

    PRIMARY KEY (blocker_id, blocked_id),
    CHECK (blocker_id <> blocked_id)
);

-- The primary key answers "does either block the other" in both
-- directions; this one the cascade when a blocked account is erased.
CREATE INDEX idx_user_blocks_blocked ON user_blocks (blocked_id);

CREATE TABLE follower_removals (
    user_id      BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    follower_id  BIGINT       NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    until        TIMESTAMPTZ  NOT NULL,

    PRIMARY KEY (user_id, follower_id)
);

CREATE INDEX idx_follower_removals_follower ON follower_removals (follower_id);
CREATE INDEX idx_follower_removals_until ON follower_removals (until);
