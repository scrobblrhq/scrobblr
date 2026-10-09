# Global rankings and anti-botting (shadow mode)

Scrobblr labels every scrobble (the classifier: `counted`, `suspect`,
`duplicate`, `no_data`) and weighs it for global rankings. **Shadow mode:**
no route serves a global ranking yet. The worker computes the weights and
`worker rank report` compares raw and filtered rankings; nothing else reads
them. Profiles do read the labels: they leave duplicates out and say how
much of a user's listening is verified (below).

## Profiles

A profile shows its user's listening, suspect plays included, but never the
classifier's duplicates (the same listen reported again, such as 2,000
scrobbles of one track 1 to 2 seconds apart).

- `scrobble_count` (every `UserProfile`, and Last.fm's `user.getInfo`)
  leaves classified duplicates out. Top artists and tracks, the heatmap and
  recent scrobbles (and `user.getRecentTracks`) leave them out too.
- `GET /v1/user/{username}` and `GET /v1/user/me` add `scrobble_breakdown`:
  `verified` (labelled `counted`) and `unverified` (`suspect + no_data +
  pending`) add up to `scrobble_count`; `duplicates` is in neither.
- Top artists and tracks add `unverified_count` (plays labelled `suspect` or
  `no_data`), and recent scrobbles a `status` (`counted`, `suspect`,
  `no_data`, or `null` until classified).

A trigger on `scrobble_classification_days` keeps `users.scrobble_count`
and `user_label_totals` in step with every classification (migration
`0019`), so no query has to subtract anything at read time.

## Weights

Each scrobble weighs 0 to 1 (`shared::ranking`), the product of:

| factor | weight | setting |
| --- | --- | --- |
| label | `counted` 1, `suspect` and `duplicate` 0, `no_data` 0.5, not classified yet 0 | `RANKING_NO_DATA_WEIGHT` |
| import | imported history 0, and none of the factors below | `RANKING_IMPORT_WEIGHT` |
| account | 0 for plays before the account is 7 days old | `RANKING_MIN_ACCOUNT_DAYS` |
| listened time | 0 under the scrobble point; 0.25 when the client sends none | `RANKING_NO_LISTENED_WEIGHT` |
| client | verified and known 1, unknown 0.5 | `RANKING_UNKNOWN_CLIENT_WEIGHT`, `RANKING_KNOWN_CLIENTS` |

Then, per user and UTC day, only the first 4 weighted plays of a track
(`RANKING_TRACK_DAILY_CAP`) and 30 of an artist (`RANKING_ARTIST_DAILY_CAP`)
keep their weight. The caps count plays, not weight, so trust can't be
traded for volume. Changing a setting reweighs history in the background,
as a classifier threshold does.

- **Listened time.** A play declaring `listened_ms` under its scrobble
  point (half the track, at most 4 minutes) weighs nothing. The point is
  measured on the shortest plausible length: the one the client reported,
  since that is what it timed the listen against, unless it is under half
  of MusicBrainz's or the catalog's (the signature of a client claiming
  short tracks); with 1 s of tolerance. The extension and the mobile app
  scrobble the moment they reach `max(30 s, min(their length / 2, 4 min))`,
  4 minutes when the mobile app has no length, so they pass. The extension
  falls back to 30 s when YouTube Music hasn't given it a length yet; those
  plays don't weigh.
- **Clients.** `scrobble_clients.verified` (the Spotify poller, a Last.fm
  request signed with a secret in `SCROBBLER_API_KEYS`) weighs in full, as
  do the known clients: this project's own (`scrobblr:ytmusic`, and the
  mobile app's sources `android`, `spotify`, `youtube-music`, `youtube`,
  `tidal`, `deezer`, `apple-music`, `amazon-music`, `vlc`, `poweramp`) and
  `listenbrainz:Web Scrobbler` and `listenbrainz:Pano Scrobbler` (with any
  version). `RANKING_KNOWN_CLIENTS` adds comma-separated `protocol:name`
  entries. Native names are claims, so "known" says which software a
  client says it is, not who runs it.
- **New accounts.** `users.created_at` is set by the server, so plays can't
  be backdated past it (the native API takes plays 30 days old).

### Listeners and plays

A ranking orders artists or tracks over a period by **listeners**, then by
**weighted plays**. A user's credit as a listener of an entity is their
weight for it over the period against one play from a known client without
listened time (0.25), at most 1: one ordinary play makes a full listener,
whatever the client, while an unknown client without listened time needs
two. The raw ranking counts every user who scrobbled the entity and every
scrobble.

### Why these numbers

From the one real history available (31,921 scrobbles imported from
Last.fm over three years, one user):

- **Track cap 4.** 75 % of plays are a track's only play that day, 96 %
  within 3 and 97.8 % within 4. A cap of 4 takes 1.0 % of the counted
  plays, all of it on-repeat days (up to 22 plays of one track).
- **Artist cap 30.** An artist's plays on a day: p50 1, p90 3, p99 15,
  p99.9 58. A cap of 30 takes 4.3 % of the counted plays, on 53 of 13,278
  artist-days (album binges). A bot inside the listening budget gets ~800
  counted plays a day spread over an artist's tracks; the cap holds it to a
  heavy fan's day.
- **No listened time 0.25.** Without `listened_ms` the classifier lets a
  client count up to ~67 plays an hour of 4-minute tracks (each cut to its
  2-minute scrobble point). The real history plays 8 tracks in a median
  active hour, 16 at p90 and 19 at p99, so 0.25 holds a client at that
  ceiling to ~17 full plays an hour, an honest heavy listener's hour. The
  caps bound it per day whatever the rate. Listener credit is scaled so a
  single such play still makes a full listener, which keeps the primary
  metric fair to Last.fm-protocol users.
- **Unknown clients 0.5, no_data 0.5.** Neither can be checked; halving
  them keeps them in the rankings without letting them lead.
- **Imports 0.** Imported history comes from another service, can't be
  checked play by play, and is the cheapest thing to dump: it still counts
  on the user's profile, never in global rankings.

## Storage and cost

Weighing is per user and UTC day, after the day is classified
(`db::queries::rankings::weigh_user_day`): it reads the day's scrobbles with
their labels and writes `ranking_days` (totals and plays per reason) and
`ranking_daily` (per track: all plays, and their weights). Classification
queues the day in `ranking_queue` in the transaction that writes its labels;
a 5-minute sweep queues days weighed with other settings, from an older
classification, or never. It reads `scrobbles` only by user, the
compression's segment key, and never writes it.

A ranking over a period sums `ranking_daily`'s rows for its days
(`db::queries::rankings::rankings`), never scrobbles: compressed chunks are
segmented by user, so anything by artist or track over raw scrobbles
decompresses the whole period.

Measured on a synthetic population (`scripts/synthetic/`: 4,000 users, 60
days, 3.95M scrobbles, 126,684 user-days, chunks older than 30 days
compressed) in TimescaleDB 2.29.2 / Postgres 18 with `shared_buffers=1GB`,
on an 8-core laptop:

| | 7 days | 30 days | 60 days |
| --- | --- | --- | --- |
| weights computed inside the ranking query, artists | 2.75–3.2 s | 11.1–11.4 s | 22 s |
| `rankings()` over `ranking_daily`, artists | 0.42–0.45 s | 1.05–1.1 s | 1.4–1.46 s |
| `rankings()` over `ranking_daily`, tracks | 0.75–0.78 s | 1.9–2.0 s | 2.6–2.8 s |

Both give the same rankings. Computing the weights in the query reads every
scrobble of the period, decompressing the compressed ones, on every
request; the materialized rows stay cheap and are written once per
classified day:

- Weighing costs about 8 ms per user-day in one process (a day of 2,100
  users and 66,673 plays took 17 s), the same order as classifying it; all
  126,684 user-days took 343 s with 6 processes.
- `ranking_daily` holds 3.38M rows (85 % of the scrobbles), 224 MB of heap
  and 445 MB with its indexes. A covering index on `day` saves 10–20 % of a
  ranking's time for ~150 MB more; the plain one (22 MB) is kept.

A ranking still takes 0.4 to 2.7 s, too slow to compute per request: a
route that serves rankings should read them from a table the worker
refreshes on a schedule (open).

## Commands

```bash
worker rank report [--from DATE | --all] [--to DATE] [--limit N]
worker rank recompute [--from DATE] [--to DATE] [--user NAME] [--dry-run]
worker rank backfill [--from DATE] [--to DATE] [--dry-run]
worker tracks mb-review [--limit N] [--apply]
```

`report` prints, for the range (the last 7 days by default): plays and how
many each reason down-weighted, then the top N artists and tracks by
listeners and by plays, filtered beside raw (what moved, entered and left),
then the users ranked by plays and by weight. `recompute` weighs days
synchronously with the current `RANKING_*` settings; run it with other
settings on a scratch copy of the database to preview them.

## Track lengths

Tracks with no length anywhere leave their scrobbles `no_data`. Besides the
Last.fm backfill (`LASTFM_API_KEY`), the worker asks Deezer's public search,
one track a second, most scrobbled first: tracks with no length, then tracks
whose catalog and MusicBrainz lengths differ by 1.5x or more. A hit needs
the artist and the title to match. Its length is kept as
`tracks.deezer_duration_ms`, never written into the catalog's, and the
classifier and the weights use it when the catalog has none. Misses are
asked again after 30 days.

MusicBrainz matches snippets, live takes and medleys, and a track's mbid is
never overwritten. `worker tracks mb-review` lists the tracks whose
MusicBrainz length both the catalog and Deezer contradict: the two agree
within 10 % and MusicBrainz differs from both by 1.5x or more
(`shared::track_lengths`). Two disagreeing sources decide nothing. With
`--apply` it takes those matches back: the recording goes into
`track_mbid_rejections` (enrichment never adopts it for that track again),
the mbid, its length and a hint naming it are cleared, enrichment searches
again, and every day with plays of the track is reclassified. Other
disagreements (the catalog or Deezer as the odd one out) are listed, not
changed.

On a real 3-year Last.fm history (3,904 tracks), Deezer was asked about
682 (613 with no length, 69 disputed) and answered 449; 385 of the 613 got
a length, and the history's `no_data` scrobbles went from 1,884 to 516.
`mb-review` flagged 37 MusicBrainz matches, among them "Feel Good Inc." at
2:08 (3:42), "Runaway Baby" at 8:30 (2:27), "Stressed Out" at 8:44 (3:22)
and "Wannabe" at 0:33 (2:53); taking them back changed one label (suspect
to counted), since the classifier already charges the longest length.
Deezer lists a few tracks only as 30-second previews ("Firework", which
MusicBrainz has as a 30 s snippet too): hits that short are ignored.

## Synthetic scenario

`scripts/synthetic/` builds a population and four attacks in a scratch
database, never a real one (the scripts refuse a database with users, or
without the population):

```bash
createdb synth && DATABASE_URL=postgres://…/synth worker migrate
psql postgres://…/synth -v users=4000 -v days=60 -f scripts/synthetic/population.sql
psql postgres://…/synth -v farm=1500 -v bots=30 -f scripts/synthetic/botting.sql
DATABASE_URL=postgres://…/synth worker classify reclassify
DATABASE_URL=postgres://…/synth worker rank recompute
DATABASE_URL=postgres://…/synth worker rank report
```

With 4,000 users over 60 days and `-v farm=4000 -v bots=30`, the week of
the attacks (2026-10-02 to 2026-10-08):

| | raw | filtered |
| --- | --- | --- |
| Farm Target (4,000 accounts 3 days old, 839,944 plays) | #1 by listeners and plays | 0 listeners, 0 weight |
| Bot Target (30 accounts, 259,140 plays of 30 s, all labelled counted) | #2 by plays | 0 (short listens) |
| Dump Target (one account, 50,000 imported plays) | #3 by plays | 0 (imported) |
| Loop Target's track (one account, 1,120 plays) | #60 of 61,993 tracks by plays | 28 plays (4 a night), #1,092 |

The filtered top 10 artists are the same, in the same order, as a raw
ranking of the honest users alone; the top 50 share 50 entries (48 in the
same position), the top 100 99. Honest users kept their weight through
the extension, the mobile app and Spotify (95 %, the rest duplicates and
`no_data`), an unknown script (47 %) and ListenBrainz scrobblers (24 %, no
listened time): the same for every artist, so the order holds. The
classifier labelled none of the honest plays suspect.

## Limits

- **Aged farms.** Accounts older than `RANKING_MIN_ACCOUNT_DAYS` that play
  like fans, through a known client name and declaring plausible listened
  time, weigh like fans: per account, nothing tells them apart. Telling
  them apart takes cross-account signals (many accounts, one artist, the
  same times); not done.
- **Native client names are claims.** Known clients weigh in full because
  of the software they name, which anyone can name.
- **The extension without a length.** It scrobbles after 30 s when YouTube
  Music hasn't reported a length; those plays weigh nothing. Waiting for
  the length, or 4 minutes without one as the mobile app does, fixes it.
- **One account with the real history.** The real data has a single user,
  so its rankings show the weights' effect on plays (duplicates, caps), not
  on listeners.
- **Scale.** Weighing, like classification, runs in one loop (~120
  user-days a second); the queue takes several workers (`SKIP LOCKED`, the
  per-day lock), but the worker starts one.
- **Profile freshness.** A play counts on the profile as soon as it is
  stored and loses its place only when its day is classified (after a 30 s
  settle); top lists subtract duplicates from `scrobble_flags`, one row per
  flagged scrobble.
