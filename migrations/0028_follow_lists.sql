-- =============================================================
--  Scrobblr — Follow lists, newest first, a page at a time
--
--  Followers and following were returned whole, unordered by any index;
--  they are now paged newest follow first, by (created_at, the other
--  account's id). These indexes serve both directions, and counting
--  followers (index-only); the primary key still serves following
--  counts and membership.
-- =============================================================

CREATE INDEX idx_follows_followee_recent ON user_follows (followee_id, created_at, follower_id);
CREATE INDEX idx_follows_follower_recent ON user_follows (follower_id, created_at, followee_id);

DROP INDEX idx_follows_followee;
