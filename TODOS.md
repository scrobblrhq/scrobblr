# TODOS

## Anti-botting

### Prefer MusicBrainz duration over client-reported for Rule 1

**What:** Use the MusicBrainz duration for time-budget math when available, and flag large client-vs-catalog duration disagreements.

**Why:** Durations are client-controlled: the first client to report a track sets `tracks.duration_ms` for good (`find_or_create_track` COALESCE), and enrichment only fills NULLs. A bot can under-report durations so thousands of plays fit inside the budget.

**Context:** Part 1 (shadow classifier) uses `COALESCE(tracks.duration_ms, scrobbles.duration_ms)` in `db::queries::classification::load_user_window`. Enrichment's fill-only-NULL policy is deliberate, so this likely needs a separate `mb_duration_ms` column written by `apply_track_metadata`. Check the shadow-mode data first for whether bots actually under-report.

**Effort:** M
**Priority:** P2
**Depends on:** Part 1 shadow data

### Persist listened_ms (real playback time)

**What:** Store the client-sent `listened_ms` on scrobbles (nullable, never required).

**Why:** It is validated once in `shared::scrobble::validate` and then dropped. Real playback time would let a later rule charge actual listening instead of full track length (fairer to skippers, harder to fake).

**Context:** Plumb `ScrobbleInput.listened_ms` → `InsertScrobble` → a nullable column (nullable ADD COLUMN is fine on the compressed hypertable). The Spotify poller fakes it as the track duration. First confirm what the extension and mobile repos actually send.

**Effort:** S (backend) + unknown (clients)
**Priority:** P3
**Depends on:** confirming extension/mobile send `listened_ms`

## Stats

### Continuous aggregates miss late/historical inserts

**What:** Make `scrobbles_daily_by_artist`, `scrobbles_daily_by_track` and `user_activity_daily` pick up inserts with old `played_at`.

**Why:** Their refresh policies only cover `[now - 3 days, now - 1 hour]` (migrations/0001), so imported history never reaches top artists/tracks or the heatmap until someone runs `refresh_continuous_aggregate` by hand.

**Context:** Options: refresh the affected range after an import, or a nightly wider refresh in the worker. Pre-existing; unrelated to the classifier.

**Effort:** S
**Priority:** P2
**Depends on:** None
