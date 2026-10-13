# Privacy

What a Scrobblr server stores about the people who use it, what they can
take with them, what deleting an account removes and when, and where that
stops. A technical description of this code for operators, not a privacy
policy (see the end).

## What is stored

**The account** (`users`): username, email address, the password as an
Argon2 hash, display name, bio, avatar (a URL, or an image uploaded to the
server), website, country, whether the profile is private, when the account
was created, changed and last scrobbled, its scrobble count.

**Listening history** (`scrobbles`, kept until the account is deleted): for
each play, when, the track, artist and album, the track length the client
reported, how long it says it was listened to, the client's name for itself
(`source`), the client as the server identified it (protocol, client name,
whether its identity was checked; `scrobble_clients` is a shared list of
client names), and the import that brought it, if any. What is playing now
is kept until the track would have ended.

**Made from the history**, all keyed by user:

- Labels: which plays the classifier found to be suspect, duplicate or of
  unknown length, per user and day, and totals per user. Kept with the
  history.
- Daily totals per user, artist and track (the continuous aggregates
  behind profiles and catalog pages).
- Ranking weights per user, day and track, for the global rankings. Kept
  for a year and a week.
- Coordination factors per user and day, and the accounts flagged in each
  detection run, for anti-botting ([rankings.md](rankings.md)). Runs are
  kept 30 days.

**Where an account registered from** (`registration_origins`), only when the
operator sets `REGISTRATION_HASH_KEY`: two 64-bit keyed hashes (HMAC-SHA256)
of the client's address and of its network, never the address, deleted
after `REGISTRATION_ORIGIN_DAYS` (90 by default). With the key, which lives
outside the database and its backups, the IPv4 space is small enough to
recover the address ([rankings.md](rankings.md#privacy)).

**Credentials**: login sessions (an id that is the bearer token, when it
was created, expires and was last used; the IP and user-agent columns are
never filled); API tokens and scrobbler credentials as SHA-256 hashes, with
their names, scopes and dates (a scrobbler token's MD5, which Audioscrobbler
1.2 needs, encrypted with `TOKEN_ENCRYPTION_KEY`); browser authorizations of
scrobblers until they are traded or expire; linked Spotify and Last.fm
accounts: the account id there, OAuth tokens encrypted with
`TOKEN_ENCRYPTION_KEY`, when it was polled and the last error.

**Imports**: each Last.fm import's Last.fm username, window, progress and
errors.

**Social and community**: follows, blocks (who blocks whom, since when),
followers removed (until when they may follow again), comments, uploaded
artist and album images (as candidates, with their likes), likes given.

**Who sees it** ([social.md](social.md#who-sees-whom)): a public profile's
listening is shown to anyone, a private one's only to its owner, followers
included (following takes no approval). Followers' feeds and community
charts include only public accounts; follow lists, listener lists and
search name only public ones; counts and catalog numbers include private
ones, unnamed. An account that blocks a viewer doesn't exist for that
viewer signed in, and neither appears in the other's lists of people;
nobody is told who blocks them ([social.md](social.md#blocking-and-removing-followers)).

**Uploaded images**: files under `UPLOAD_DIR`, re-encoded as JPEG (no
metadata survives), served at `/uploads` with a one-year immutable cache
lifetime ([uploads.md](uploads.md)).

**Redis**, all expiring: a session's user id (until the session expires),
rate-limit counters keyed by user id, address network or username (15
minutes to 2 days), single-use states of account links (10 minutes),
Audioscrobbler 1.2 sessions (24 hours after last use).

**Logs**: at the default level (`RUST_LOG=info`, `tower_http=warn` in
Compose) requests aren't logged; lines name user ids for some events
(exports, deletions, erases, failures) and, on errors, what failed. With
request logging turned up, request paths are logged, and they hold
usernames (and an Audioscrobbler 1.2 handshake's token). Compose rotates
each service's log at 10 MB, 5 files.

**Backups** ([operations.md](operations.md#backups)): every day, the whole
database and every upload; the newest of each of the last 7 days and 4
weeks are kept (about 5 weeks), on the host and, if configured, on a remote
with the same retention. Readable by root only.

**Not personal, and shared**: artists, albums and tracks (names, lengths,
images, MusicBrainz ids from providers), created by whoever scrobbled them
first, and their play counters, which add up everyone's plays.

## Export

`GET /v1/user/me/export` (a session, not an API token; 3 a day per account,
10 per address) streams a ZIP archive: `account.json` (the account,
settings, follows, the accounts the user blocks and the followers it
removed, comments, uploads and likes, sessions without their ids,
API tokens and scrobbler credentials without their values, authorizations
without their tokens, linked accounts without their OAuth tokens, imports,
now playing, when the registration origin was recorded),
`scrobbles.csv` and `scrobbles.json` (every scrobble with its source,
client, import and label), and the uploaded images. Nothing is written to
the server's disk: there is no copy to expire.

Left out, and said so in `account.json`: the password hash; credentials'
values or hashes (the archive would sign in as the user wherever it ends
up); linked accounts' OAuth tokens; the registration origin's hashes
(meaningless without the key, which can't be handed out without exposing
everyone's); ranking weights, coordination factors and cohort membership
(anti-abuse scores computed from the scrobbles, which are included); who
blocks the user or removed them from their followers (their decisions); daily
totals and counters (computed from the scrobbles); logs, Redis and backups.

## Deletion

`POST /v1/user/me/delete` with the password and the username typed out (a
session, not an API token; 5 attempts an hour per account, 10 per address).

**At once**, in one transaction: every session (its Redis cache entry
too), API token, scrobbler credential and approved authorization and linked
account (with its OAuth tokens) is deleted, imports in progress are
cancelled, now playing is cleared. The account disappears from profiles,
the Last.fm-compatible API, search, listener lists, follower lists,
followers' feeds and community charts, comments and image candidates. Its history and the totals made from it
stay, and still count in catalog numbers and rankings.

**Grace period**: `ACCOUNT_DELETION_GRACE_DAYS` (7 by default, 0 to 30).
Logging in with the password (the native login, never a scrobbler's)
restores the account, history, follows and blocks included; credentials and
links have to be made again. The username and email stay taken.

**Then the worker erases it** (within a minute of the date): the scrobbles,
a chunk at a time, each taking its plays off the track, artist and album
counters in the same transaction; the user's rows in the daily totals
(deleted directly, not by recomputing everyone's); labels, weights,
coordination factors and flags; then the user row, which every other table
referencing it follows (`ON DELETE CASCADE`, checked by a test that looks
for any column holding a user id): sessions and tokens left, origins,
imports, follows, blocks and removals either way, comments, likes (taken
off the candidates' counts),
uploads, queued work. The global rankings are recomputed on the worker's
next pass (within minutes). The uploaded files are deleted; one that
became an artist's or album's image stays as the catalog's, no longer tied
to anyone. The username and email can be taken again from then on.

Measured on a synthetic account with 503,700 scrobbles over three years
(1.3 million rows in all, TimescaleDB 2.29 on Postgres 18): 124 seconds,
other users' scrobbling and reads going on meanwhile (p50 22 ms, p99
335 ms, against 31 and 111 ms without it). On TimescaleDB 2.30 and
Postgres 16, as Compose runs it, 201,480 scrobbles took 58 seconds (p99
430 ms, against 81). It leaves its chunks partly compressed, which the
compression policy tidies in its next run (17 to 23 seconds for 152
chunks).

`worker accounts pending` lists the accounts waiting; `worker accounts
erase NAME` erases one now, for a request made outside the app.

## Limits

- **Backups** keep an erased account until they rotate out (about 5 weeks
  with the defaults, remote copies too). Restoring one brings erased
  accounts back; nothing in the database remembers them, so an operator
  restoring must erase again what was requested since.
- **Caches outside the server**: images were served as immutable for a
  year, so browsers and CDNs may keep a deleted avatar; public ranking
  responses are cacheable for 5 minutes. A web app, reverse proxy or
  analytics in front keep their own logs.
- **Other services** keep their copies: revoke Scrobblr's access in
  Spotify's or Last.fm's settings; Last.fm keeps the history it had.
- **Shared catalog rows stay**: an artist or track first scrobbled by the
  user keeps the spelling they sent, and a MusicBrainz id their client
  suggested.
- **Catalog counts** (a track's or artist's plays, an artist's top tracks,
  search results) are served only for entities at least
  `RANKING_MIN_LISTENERS` (5) distinct accounts played, private ones
  included and never named, as for the rankings. Five listeners don't stop
  one of them from making up most of a count, and two looks at a count
  over time show that someone among them played it.
- **During the grace period** the account's plays still count in catalog
  numbers and rankings.
- **Community charts** are served only for entities `RANKING_MIN_LISTENERS`
  (5) of the viewer's followed accounts played, but the viewer knows who
  those accounts are and can see their profiles; a chart computed before an
  account turned private or left is no longer served once it has (its key
  covers the accounts), but one already fetched stays with the client.
- **An export is a snapshot read page by page**: a play arriving while it
  streams may or may not be in it.

## Before opening to strangers

A lawyer, or a template written for your jurisdiction, should cover:

- Terms of service: who may register (age), acceptable use, uploads and
  comments (copyright, takedown, moderation), ending accounts.
- A privacy policy: who the controller is, purposes and legal bases (the
  anti-abuse scoring included), the retention periods above, recipients
  (hosting, backup storage, the providers the worker asks about tracks),
  users' rights and how to exercise them.
- Data protection law (GDPR or local): whether the export answers access
  and portability requests in full, in particular without the anti-abuse
  scores; whether the grace period and backup retention fit erasure
  deadlines; a record of processing; processing agreements with the
  hosting and backup providers; international transfers.
- Breach handling: who is told, how fast.
- A record of deletion requests kept outside the database, to re-apply
  them after restoring a backup, and how long to keep it.
