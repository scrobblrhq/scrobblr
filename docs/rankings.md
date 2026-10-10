# Global rankings and anti-botting (shadow mode)

Scrobblr labels every scrobble (the classifier: `counted`, `suspect`,
`duplicate`, `no_data`) and weighs it for global rankings. **Shadow mode:**
no route serves a global ranking yet. The worker computes the weights,
looks for accounts pushing the same entities together ([Coordination](#coordination)),
and keeps each period's ranking precomputed; `worker rank report` compares
raw and filtered rankings and `worker rank top` prints a stored one;
nothing else reads them. Profiles do read the labels: they leave
duplicates out and say how much of a user's listening is verified (below).

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

## Coordination

The weights judge one account at a time. A farm of accounts older than
`RANKING_MIN_ACCOUNT_DAYS`, each playing like a plausible fan through a
known client and declaring plausible listened time, weighs like fans: per
account, nothing tells them apart. What gives a farm away is what its
accounts share. Every 6 hours the worker looks for accounts pushing the
same artists and tracks together (`shared::coordination`) and gives each a
factor from 0 to 1 that global rankings multiply its listener credits and
weight by. It blocks, deletes and relabels nothing, and profiles never
read it. Like the rest of this page, it is shadow mode: only `worker rank
report`, `worker rank cohorts` and the stored snapshots read it.

### Groups, not pairs

Comparing every account with every other grows with the square of the
accounts. Instead, every artist and every track is a candidate group: the
accounts that weighed something for it over the last 28 days
(`ranking_daily`). A farm exists to push something, so it sits inside its
targets' groups, and building the groups costs one pass over the
(account, entity) pairs. Within a group, each signal asks whether the
members share a trait far more often than the active population explains.
A popular artist's listeners look like the population on every trait, so
popularity flags no one; "accounts whose listening coincides beyond what
popularity explains" is what a cell inside a group measures.

### Signals

Four signals compare a group with the population. For a value of a trait
(a registration week, an address hash, a client) held by `observed` of a
group's `n` members, where `c` of the population's `N` active accounts hold
it, chance predicts `expected = n × c / N`. A **cell** counts only when its
excess, `observed − expected`, is at least 10 accounts
(`RANKING_COORDINATION_MIN_COHORT`) and 4 standard deviations
(`RANKING_COORDINATION_MIN_Z`, Poisson). Its members get its **purity**,
`(observed − expected) / observed`: the share of the cell the excess
explains. A farm's cell is nearly pure; the few honest accounts that share
its trait by chance get the same purity but, lacking the farm's other
traits, only what one signal costs.

| signal | a member's cell | weight |
| --- | --- | --- |
| `age` | registered in a 7-day window (`_AGE_DAYS`) the group is over-represented in; every non-overlapping window that passes | 0.5 |
| `origin` | registered from an address, or a network, that at least 3 members share beyond chance ([Privacy](#privacy)); address or network, whichever explains more | 0.5 |
| `client` | scrobbled the entity mostly through a client family (`protocol:name` without its version) used by under 2 % of the accounts outside the group (`_RARE_CLIENT`) | 0.35 |
| `sync` | played its tracks in crowds on at least 4 of the last 14 days (`_SYNC_DAYS`, `_SYNC_WINDOW_DAYS`); 0.5 at 4 days, 1 from 8 | 0.5 |
| `shape` | its own listening: the share of its weight on its top 3 artists, 0 at 45 % and 1 from 85 % (`_SHAPE_FROM`, `_SHAPE_TO`) | 0.125 |
| `burst` | new to the entity (no weight for it in the 28 days before the window); worth the group's share of new members | 0.125 |

**A crowd** is a play with at least 3 other accounts playing the same track
within 2 minutes of it (`_SYNC_SECS`), 10 times (`_SYNC_RATIO`) what the
accounts playing it within the hour around (`_SYNC_CONTEXT_SECS`) would put
in those 4 minutes if spread evenly: a third of that hour's other listeners
within 4 minutes. The hour, not the day, is the null because habits vary
over the day (everyone listens in the evening) and hardly within an hour.
Calqued schedules crowd every day; a premiere, a release night or a
listening party crowds one.

The clients the weights trust are the ones anyone can claim, so `client`
only counts a client nearly nobody outside the group uses: a farm's own
Last.fm API key or script name. A platform-exclusive catalogue skewing a
fandom to one popular client never fires it.

### Suspicion and factor

A member's suspicion for a group is `min(1, Σ weight × evidence)`; an
account's is its highest over the groups it is in. One primary signal at
full strength reaches 0.5, the two behavioural ones together 0.25, so
**nothing short of two independent group signals reaches certainty**, and
behaviour alone never reaches the curve. The factor is 1 up to 0.4, 0 from
0.9 (`RANKING_COORDINATION_FROM`, `_TO`), a smoothstep between:

| evidence | suspicion | factor |
| --- | --- | --- |
| shape and burst only, at full strength | 0.25 | 1 |
| one primary signal at full strength | 0.50 | 0.90 |
| one primary at full strength, plus shape and burst | 0.75 | 0.22 |
| two primary signals at purity 0.8 | 0.80 | 0.10 |
| two primary signals at full strength | 1 | 0 |

The bounds come from two requirements: one signal alone must keep at least
~90 % (it has innocent explanations: a campus, an artist bringing its fans
over in one week), and two strong ones must keep at most ~10 %.

### In the rankings

A run writes the factor of each flagged account's active days in
`coordination_days` (rows below 1 only). Rankings and snapshots multiply a
user's listener credits and weight by the user's factor for the period:
the factors of their days, weighed by their weight on each day. They
multiply the credit **after** it is capped at one listener: a farm account
with lots of weight would otherwise stay a full listener at any factor.
`rankings()` keeps the numbers without the factors beside them, for
`worker rank report`.

A run rewrites its window's days and leaves earlier days as the last run
covering them left them, so a farm that stopped keeps its days dented in
the year's ranking; the rows expire with the weights. A run is due every 6
hours, when the UTC day changes and when its settings do; a snapshot is due
again when a newer run lands. A run that fails keeps the previous factors
and is retried 10 minutes later; the loop's heartbeat reports it. Runs,
cohorts and flagged accounts are kept 30 days for review.

`RANKING_COORDINATION=report` lists cohorts without applying any factor
(every stored factor is removed); `off` doesn't run (and removes them);
`apply`, the default, applies them to the shadow rankings. Below 200 active
accounts (`_MIN_ACCOUNTS`) nothing is judged: every signal compares with
the population, which is then too small to be a baseline.

### Cohorts

A cohort is the flagged accounts whose strongest group signal is in the
same group (0.05 apart counts as equal and the larger group wins, so a
farm meets on its main target rather than scattering over groups that
differ by rounding). `worker rank cohorts` lists each with its size, mean
score and factor, the members' mean evidence per signal, the cells of its
group (registration windows, how many origin and client values, lockstep
days, new listeners), and its top 5 targets with the cohort's share of
their listeners and weight; `--members` adds every account's evidence.

### Why these numbers

- **28-day window, 14 days of plays.** A push that moves the month's
  ranking lasts weeks; 28 days covers the week and the month, and the
  baseline for `burst` is another 28 (inside the 372 days of weights
  kept). Plays are read raw for `sync` and `client`, so their window is
  shorter: 14 days is two weekends, enough for 4 days of crowds to mean
  something.
- **Excess of 10 accounts, z of 4.** Ten accounts add at most ten
  listeners, a few positions where the week's #100 artist has 178 of them
  on the synthetic population (#50: 414, #200: 92). Under
  Poisson, z ≥ 4 is a 3 × 10⁻⁵ chance per cell; small expectations are
  held by the 10-account floor instead (11 or more where 1 is expected:
  under 10⁻⁸). Friend groups (4) and households (3) never reach it.
- **3 members per value.** A couple's two accounts never make a cell.
- **Crowds: 3 others, 2 minutes, a third of the hour, 4 days.** Two
  minutes is under half a typical track (3.7 min), so a crowd is the same
  track at the same time; 3 others leaves out a household listening
  together on one speaker; one day is a premiere, two a weekly party.
- **Rare client: 2 %** of the accounts outside the group: a client the
  group nearly owns.
- **Shape: 45 % to 85 %** of a month on 3 artists. Honest synthetic
  accounts put a median 21 % of their month on their top 3 artists (p90
  31 %, p99 51 %); the aged farm 70 to 74 %, the decoy farm 26 to 38 %.
- **Weights.** Primary signals 0.5 so that two are needed. `client` 0.35:
  a name is free to change, so it corroborates rather than convicts.
  Behavioural signals 0.125: together half a primary, and never enough
  alone.

### Cost

Detection is linear in the (account, entity) pairs of the window plus
sorting the sync window's plays; it never compares two accounts. Measured
on the synthetic population below (5,154,957 scrobbles, 5,550 accounts,
chunks older than 30 days compressed) in TimescaleDB 2.29.2 / Postgres 18
with `shared_buffers=512MB`, on an 8-core laptop with 7 GB of RAM:

| | |
| --- | --- |
| input | 5,522 active accounts, 976,481 (account, track) pairs, 1,439,097 plays, 14,388 artists and tracks examined |
| loading | 9.8 s: the window's pairs 3.4 s, the baseline 2.4 s, the plays 3.2 s |
| detection | 2.3 s on a blocking thread |
| storing | ~1 s, one transaction (36k factor rows) |
| a run | 13.5 s, 257 MB peak memory; ~25 s under the load below |

It only reads until its final transaction. With 4 clients inserting
scrobbles and reading profiles and the stored ranking (pgbench), latency
was the same with and without detection running back to back: 9.47 ms
(422 transactions/s) idle, 9.50 ms (421/s) during, within this machine's
run-to-run noise.

Applying the factors costs the rankings little, with 1,473 accounts
flagged (27 % of the active ones, an extreme where the farms are a quarter
of the population; with no flags the cost is near nothing):

| | without factors | with factors |
| --- | --- | --- |
| all six snapshot refreshes | 9.8 s | 10.6 s |
| `rankings()`, artists, week / month / 60 days | 0.51 / 1.45 / 2.34 s | 0.59 / 1.70 / 2.58 s |
| `rankings()`, tracks, week / month / 60 days | 0.84 / 2.36 / 3.58 s | 0.91 / 2.70 / 4.07 s |
| reading 50 positions of a snapshot | unchanged | unchanged |

Two first attempts cost more and were dropped: joining every
`ranking_daily` row to the factors (the planner merge-joined in day order,
+25 to 65 % with no factor stored) and subtracting per (user, entity, day)
what the factors take (+45 to 80 % with them). One factor per user and
period, joined by hash, is what the table measures.

### Synthetic results

`scripts/synthetic/coordination.sql` adds four farms and three honest
crowds to the 4,000-user population (commands under
[Synthetic scenario](#synthetic-scenario)). Every farm account is older
than 7 days, claims a client the weights trust, declares listened time at
the scrobble point and plays 15 to 40 tracks a day over 28 days, three
target artists and filler; the classifier labels none of it suspect.

- **a, aged farm**: 400 accounts registered within 2 days, 40 days ago,
  before origins were recorded; own hours; 70 % targets.
- **b, decoy farm**: 400 registered within 2 days, no origins; 25 %
  targets, the rest from 25 favourites drawn like the population's.
- **c, proxied farm**: 300 registered within 3 days from 3 addresses in
  one /24, one schedule for all (20:00, same tracks), its own Last.fm API
  key.
- **d, spread farm**: 300 registered over 4 months (60 to 180 days ago,
  so most past origin retention), each from its own residential address,
  own hours, 50 % targets.
- **e, release**: 2,650 honest users play a new album the night it comes
  out (40 % in its first 10 minutes), half replay it later; 150 accounts
  sign up the week before for it.
- **f, friends and shared networks**: 40 groups of 4 friends registered
  the same days sharing a playlist and a Saturday listening party in
  sync; 30 households of 3 behind one address listening together most
  evenings; 150 students behind one campus address registered within 2
  weeks.
- **g, fandom**: 60 established fans of a small artist, 6 to 12 plays
  most nights, and a listening party.

With the default settings (run #6, the week ending 2026-10-10):

| scenario | accounts | flagged | at 0 | mean factor | weight kept | its artists in the week by listeners |
| --- | --- | --- | --- | --- | --- | --- |
| a, aged farm | 400 | 400 | 0 | 0.46 | 45.6 % | #52–54 → #94–96 (400 → 182 listeners) |
| b, decoy farm | 400 | 400 | 0 | 0.80 | 79.9 % | #55–57 → #52–54 (400 → 320) |
| c, proxied farm | 300 | 300 | 300 | 0 | 0 % | #65–67 → out of the ranking |
| d, spread farm | 300 | 211 | 0 | 0.91 | 90.7 % | #62–64 → #63–65 (300 → 272) |
| e, release listeners | 2,650 | 104 | 0 | 0.999 | 99.89 % | Release Star #6 → #4 |
| e, hype signups | 150 | 5 | 0 | 0.999 | 99.99 % | |
| f, campus | 150 | 31 | 0 | 0.993 | 99.24 % | |
| f, friends | 160 | 63 | 0 | 0.991 | 99.01 % | |
| f, households | 90 | 26 | 0 | 0.991 | 98.88 % | |
| g, fandom | 60 | 2 | 0 | 0.999 | 99.85 % | Indie Darling #316 → #308 |
| every honest account | 4,150 | 162 | 0 | 0.999 | 99.89 % | |

The proxied farm fires all four group signals and is zeroed; each other
farm fires one (registration) plus behaviour. No honest account was
dented below 0.92: 58 kept 0.99 or more, 104 between 0.90 and 0.99. They
are mostly the friends, households and students, whose registrations the
scenario packs into the same four weeks: shared listening (a playlist, a
household's evenings) puts several of them in the same artists' groups,
where that shared registration week is one signal. The decoy farm moves
*up* three places because the aged farm above it falls. `worker rank
cohorts` shows the first three farms as one cohort each, named after
their first target, and 194 of the spread farm's 211 flagged accounts as
a fourth; the honest dents and the rest of the spread farm make 38
smaller cohorts of 1 to 14 accounts.

**More aggressive settings** (`RANKING_COORDINATION_FROM=0.3`, `_TO=0.7`,
shape and burst 0.2) zero the aged farm, keep 15 % of the decoy farm and
47 % of the spread one, but dent 542 honest accounts, 67 of them below
half: friends keep 76 %, the campus 77 %, households 79 %, hype signups
80 %. The defaults choose the other side.

### What it doesn't catch, and what evading costs

- **The spread farm (d) is not caught**, and the aged (a) and decoy (b)
  farms only dented: each shares one group trait (when its accounts
  registered) and nothing else. The aged farm's three targets stay in the
  week's top 100 at 182 listeners each. Telling them from fans an artist
  brought over in one week, or a campus, takes a second independent
  trait, which they don't have; zeroing on one trait is what the
  aggressive settings above do, at a cost to honest accounts.
- **Registration (`age`) costs time.** A week's cell needs 10 accounts
  above what the population's signups predict. A farm registering fewer
  than ~10 extra accounts a week passes unseen: 400 accounts take some 40
  weeks, all held until used. The spread farm, registered over 4 months
  against a population registered over 2 years, still stands out 6 times
  in every week it registered in.
- **Origin costs money or time:** one registration per residential
  address (proxy services sell them; a registration is a few requests),
  or accounts aged past the 90 days origins are kept.
- **Client, sync, shape and burst cost nothing to a script:** claim a
  popular client, randomize each account's schedule, add filler
  listening, ramp up slowly or keep pushing past 28 days. That is why
  they only corroborate (client weighs 0.35, behaviour 0.125).
- **Small farms.** A cell needs 10 accounts above chance, so up to ~10
  coordinated accounts per target are never judged; on a small instance
  that is a larger share of a ranking (the week's #200 here has 92
  listeners).
- **What the weights already handle isn't repeated here:** new accounts,
  imports, short listens, unknown clients and loops weigh what they did.

### Privacy

The origin signal needs to know which accounts registered from the same
place, never where:

- **What is stored.** With `REGISTRATION_HASH_KEY` set, registering stores
  two 64-bit values (`registration_origins`): HMAC-SHA256 under that key of
  the client's address (an IPv6 address by its /64, which one host rotates
  its addresses within) and of its network (IPv4 /24, IPv6 /48), the key's
  id, and when. Never the address. Unset, nothing is stored and the signal
  is silent.
- **Which address.** The one the API's limits use: the peer, or the entry
  the trusted proxies appended to `X-Forwarded-For` (`TRUSTED_PROXY_HOPS`,
  `TRUSTED_PROXIES`), so a client reaching the API around its proxies
  can't make another account look like it registered elsewhere, or like
  a farm's. Behind a proxy the API isn't told about, every registration
  shares the proxy's address; the signal then finds nothing over chance
  and flags no one, but sees nothing either.
- **The key is the secret.** The IPv4 space is small enough to hash
  whole, so whoever holds the key and the table (or a backup) can recover
  the addresses. It lives in the environment, like `TOKEN_ENCRYPTION_KEY`,
  never in the database or its backups ([operations.md](operations.md)).
- **Retention.** The worker deletes origins older than
  `REGISTRATION_ORIGIN_DAYS` (90) once a day; backups keep them as long as
  they keep everything else. Deleting an account deletes its row.
- **Key changes.** Each value carries its key's id, so values made under
  different keys never match: after a new key, earlier origins compare
  with nothing and the signal weakens until they expire. Change it when
  it may have leaked, not on a schedule.
- **Use.** Values are only compared for equality inside the detection;
  cohort listings say how many accounts share how many values, never
  which. Being pseudonymous data while the key exists, they belong in the
  privacy policy, as fraud prevention.

### Recalibrating with real users

Every threshold is a setting because none could be fitted to real data:
the real database has a handful of accounts, under the 200 below which
nothing is judged. The defaults lean to false negatives. Once there are
real users:

1. Run `RANKING_COORDINATION=report` for a few weeks: cohorts are
   computed and listed, no factor is applied.
2. After each run, review `worker rank cohorts --members`. A farm shows
   as a large cohort whose targets few others listen to (`… of … listeners`
   near 100 %) with two or more signals; honest dents show as small
   cohorts with one signal and popular targets. Note which signals fire
   for each kind.
3. Preview other settings with `worker rank detect --dry-run` and the
   variables set; it stores nothing. Compare flagged counts, and run
   `scripts/synthetic/` on a scratch copy to see what a change costs the
   honest crowds there.
4. Scale `RANKING_COORDINATION_MIN_COHORT` with the rankings: about the
   listeners a few positions are worth around the last position anyone
   looks at (here 10 against 178 at #100).
5. Watch the share of active accounts flagged and the honest
   cohorts' factors; on the synthetic population 3.9 % of honest accounts
   are dented, none below 0.92. If real honest cohorts are dented more,
   raise `_FROM`, or `_MIN_Z` for the signal that fires them.
6. Switch to `apply` once the listings hold no honest cohort you would
   not accept being dented as much as it is.

## Storage and cost

Weighing is per user and UTC day, after the day is classified
(`db::queries::rankings::weigh_user_day`): it reads the day's scrobbles with
their labels and writes `ranking_days` (totals and plays per reason) and
`ranking_daily` (per track: all plays, and their weights). Classification
queues the day in `ranking_queue` in the transaction that writes its labels;
a 5-minute sweep queues days weighed with other settings, from an older
classification, or never. It reads `scrobbles` only by user, the
compression's segment key, and never writes it.

Weights are kept for the days the longest period reaches, plus a week
(`RETAINED_DAYS`, 372): once a UTC day the sweep deletes older days'
`ranking_days` and `ranking_daily` rows, weighing a day before the window
removes its rows instead of writing them (a dry run still computes it), and
the sweep never queues one. Labels, which profiles read, are kept for all
history. A ranking over a longer span would need the window widened first.

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

### Precomputed rankings

A ranking still takes 0.4 to 3 s, too slow to compute per request, so the
worker keeps them precomputed (migration `0022`, the `ranking snapshots`
loop). For each period, a rolling window of UTC days ending today, today
included:

| period | days | recomputed |
| --- | --- | --- |
| `week` | 7 | every hour |
| `month` | 30 | every hour |
| `year` | 365 | every day |

and each also when the UTC day changes (the window moves), the weights'
settings do, or a newer coordination run brings other factors. A refresh sums `ranking_daily` over the period and replaces
the period's two rows in `ranking_snapshots` (artists, tracks) and their
first 1,000 positions in `ranking_entries`, in one transaction: listener
credits and weight with the coordination factors applied, the raw counts
beside them, entities without weight left out. Readers see the previous snapshot until the new one commits.

A refresh that fails (the database away, the 10-minute statement timeout)
rolls back: the previous snapshot stays as it was and the period is tried
again 5 minutes later. Nothing marks a snapshot invalid; a reader judges
how fresh it is from `computed_at` and `to_day`, and `pending_days` says how
many user-days of the period were still waiting to be weighed when it ran
(after new settings, history is reweighed over several sweeps). A route
would read `db::queries::rankings::snapshot_entries`: a primary-key range
and a lookup per entry for its name.

Measured on the same kind of population (4,000 users, 60 days, the four
attacks of [Synthetic scenario](#synthetic-scenario) with `farm=4000`:
5.10M scrobbles, 154,908 user-days, 3.60M `ranking_daily` rows), same
setup:

| | week | month | year (all 60 days) |
| --- | --- | --- | --- |
| before: `rankings()` per request, artists | 0.47–0.57 s | 1.24–1.41 s | 1.75–1.89 s |
| before: `rankings()` per request, tracks | 0.83–1.07 s | 1.75–2.09 s | 3.2–3.3 s |
| refresh, artists | 0.40–0.56 s | 1.07–1.14 s | 1.36–1.55 s |
| refresh, tracks | 0.55–0.64 s | 1.66–2.20 s | 2.3–2.5 s |
| after: reading 50 positions | 1.5 ms | 1.5 ms | 1.7 ms |

All six refreshes take about 7 s, about 4 s an hour plus 4 s a day on
this population, and the snapshots hold 6,000 rows (1.4 MB). Reading all
1,000 positions takes 16 ms. None of the four attack targets is in the
stored week ranking. The year's cost grows with the history: it reads
every `ranking_daily` row of the year.

## Commands

```bash
worker rank report [--from DATE | --all] [--to DATE] [--limit N]
worker rank recompute [--from DATE] [--to DATE] [--user NAME] [--dry-run]
worker rank backfill [--from DATE] [--to DATE] [--dry-run]
worker rank refresh [--period week|month|year]
worker rank top [--period week|month|year] [--kind artist|track] [--limit N]
worker rank detect [--dry-run] [--limit N] [--members]
worker rank cohorts [--dry-run] [--limit N] [--members]
worker tracks mb-review [--limit N] [--apply]
```

`report` prints, for the range (the last 7 days by default): plays and how
many each reason down-weighted, then the top N artists and tracks by
listeners and by plays, filtered beside raw (what moved, entered and left),
then the coordination section (the latest run, the weight its factors
took in the range, and the top N with the factors beside the same
without them, what moved and left), then the users ranked by plays and
by weight. `recompute` weighs days
synchronously with the current `RANKING_*` settings; run it with other
settings on a scratch copy of the database to preview them. Days before
the retention window have no weights: `report` shows none for them, and
`recompute` previews them only with `--dry-run`. `refresh`
recomputes the stored rankings now and `top` prints one as a route would
read it. `detect` runs a coordination detection now with the current
`RANKING_COORDINATION*` settings and stores it (the worker does every 6
hours); `cohorts` lists the latest stored run's cohorts. With `--dry-run`
both detect now and store nothing, which previews other settings;
`--members` lists each flagged account's evidence.

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

The coordination scenarios ([Synthetic results](#synthetic-results))
take the population without the four attacks:

```bash
psql postgres://…/synth -v users=4000 -v days=60 -f scripts/synthetic/population.sql
psql postgres://…/synth -f scripts/synthetic/coordination.sql
DATABASE_URL=postgres://…/synth worker classify reclassify
DATABASE_URL=postgres://…/synth worker rank recompute
DATABASE_URL=postgres://…/synth worker rank detect
DATABASE_URL=postgres://…/synth worker rank refresh
psql postgres://…/synth -f scripts/synthetic/coordination-results.sql
```

Each farm's size and the crowds' are variables (`-v aged=400 -v decoy=400
-v proxied=300 -v spread=300 -v release=2500 -v hype=150 -v fans=60`).
The population's registration days are spread over 60 to 900 days ago by
user number, without drawing from the random sequence, so its plays (and
the numbers above) are the same as before they were spread. Loading took
13 min for the population and 10 for the scenarios; classifying and
weighing 10 and 12 min in 6 processes split by date (days are
independent).

## Limits

- **Aged farms sharing one trait.** Coordination detection zeroes a farm
  that shares two group traits, but only dents one that shares a single
  one (registration week) and evades the rest; one registering slowly
  enough isn't seen at all ([what evading costs](#what-it-doesnt-catch-and-what-evading-costs)).
  Thresholds are unfitted to real users ([Recalibrating](#recalibrating-with-real-users)).
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
- **Snapshots are recomputed whole.** Nothing is incremental, and a
  snapshot keeps no earlier positions, so a chart's movement since last
  week would need snapshots kept by date.
- **Profile freshness.** A play counts on the profile as soon as it is
  stored and loses its place only when its day is classified (after a 30 s
  settle); top lists subtract duplicates from `scrobble_flags`, one row per
  flagged scrobble.
