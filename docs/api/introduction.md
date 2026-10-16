# Scrobblr API

Scrobblr is a self-hosted music scrobbling service: it records what people
listen to and shows their listening, charts and what the community makes
of the catalog. This is its HTTP API: JSON over HTTPS, at the root of the
server's API address (written `https://scrobblr.example` below).

Besides this native API, the server speaks the protocols existing
scrobblers know: the Last.fm API, Audioscrobbler 1.2 and ListenBrainz
(the **Scrobbler protocols** section). An app that already scrobbles to
Last.fm or ListenBrainz usually needs only a different server URL.

## Getting started

Most reads are public: a profile, its charts, an artist's page, the
rankings.

```http
GET /v1/user/alice HTTP/1.1
Host: scrobblr.example
```

To act as a user, send a credential as a bearer token:

```http
GET /v1/user/me HTTP/1.1
Host: scrobblr.example
Authorization: Bearer 3d0b1f7c-1c2e-4f61-9a43-5d6e2b8f9a10
```

Times are RFC 3339 in UTC (`2026-10-13T10:41:07Z`), days are UTC dates
(`2026-10-13`), lengths are milliseconds, and ids are 64-bit integers.
Image fields hold absolute URLs, or `null`.

## Authentication

There are two kinds of credentials for this API, both sent as
`Authorization: Bearer <token>`.

**Sessions** come from logging in (`POST /v1/auth/login` with `username`
and `password`) or registering (`POST /v1/auth/register`). The `token` in
the answer is the session; it lasts 30 days from login (`expires_at`),
however much it is used, and may do everything the account can.
`POST /v1/auth/logout` ends it, or with `?all=true` every session of the
account. Some servers accept logins and registrations only from their own
apps; elsewhere, ask the user for an API token.

**API tokens** are for scripts and apps that should do less than the
account can. The user creates one with a session (`POST /v1/auth/tokens`,
with a `name`, `scopes` and optionally `expires_days`); its value is shown
in that answer only. A token may do what its scopes allow, and none
implies another:

| scope | allows |
| --- | --- |
| `scrobble` | `POST /v1/scrobble` and `POST /v1/now-playing`: all a player needs. The default when no scopes are given. |
| `read` | the account's own data: `GET /v1/user/me`, its feed, community charts, recommendations, loved tracks, tags, imports, linked accounts and settings |
| `write` | changing the account and acting as it: the profile, follows, loves, tags, bios, comments, image votes and uploads, imports, catalog refreshes, recommendation skips, reports |

**Routes that take a session only**, never an API token, so that a leaked
token can't outlive its revocation or take the account over: creating and
revoking API tokens, logging out, scrobbler credentials and authorizations
(`/v1/scrobbler/*`), linking accounts (`GET /v1/connect/{provider}` and its
callback), blocks and removed followers, the similarity setting
(`PUT /v1/user/me/similarity`), the account export and deletion.

`GET /v1/auth/tokens` takes any credential; with an API token it lists only
that token, which is how an app checks the token it was given.

Routes that say more to a signed-in viewer (whether the viewer follows an
account, the viewer's own private profile, accounts blocking the viewer
left out) take a session only, since how other accounts stand to the
account is a session's to see; the track page (`GET /v1/track/{id}`) also
takes a token with `read`, for its `loved` flag. Any other credential
counts as signed out there.

Third-party scrobblers don't use any of these: they take **scrobbler
tokens**, which only scrobble, love and tag (the **Scrobblers** and
**Scrobbler protocols** sections).

A missing, invalid or expired credential is a `401`; one that doesn't
allow the route is a `403` saying what it lacks.

## Errors

Every error of the native API has the same body:

```json
{ "error": "this API token lacks the `write` scope" }
```

The message is for people and may change; branch on the status code.

| status | meaning |
| --- | --- |
| `400` | invalid input: a parameter out of range, a malformed cursor or tag, a body that doesn't parse |
| `401` | no credential, or an invalid or expired one; a wrong password at login |
| `403` | not allowed: someone else's private profile, a token without the scope, a route that takes a session, a wrong password where one is asked again, an account that can't edit yet |
| `404` | no such thing, or one the viewer may not know exists (below) |
| `409` | a conflict: a cap reached, a stale bio edit, a state that forbids it (following an account you block) |
| `413` | a body too large |
| `422` | a scrobble refused: too old, in the future, too short, a duplicate; or a JSON body missing a field |
| `429` | a rate limit (below) |
| `500` | something failed on the server; the body says nothing more |
| `503` | a feature this server isn't set up for (Spotify or Last.fm linking, browser approval) |

## Rate limits

Every request counts against a limit per client address, 60 a minute by
default (each server sets its own), an IPv6 client counting by its /64.
Some actions have limits of their own on top, per account and per
address, counted in fixed windows (the hour, the day) whether or not the
action succeeds:

| action | per account | per address |
| --- | --- | --- |
| scrobbles (every API together) | a daily allowance, 3,000 by default, until midnight UTC | |
| password logins (native and scrobbler APIs) | | 20 in 15 minutes, 10 of them for one username |
| image uploads | 20 an hour | 30 an hour |
| follows | 100 an hour; 3 new follows of the same account a day | 300 an hour |
| blocks, unblocks and removed followers | 200 an hour; 3 blocks of the same account a day | 600 an hour |
| loves and unloves (every API together) | 300 an hour | 1,000 an hour |
| bio edits and reverts | 30 an hour; 50 revisions, 3 of one bio, in 24 hours | 60 an hour |
| tag changes (every API together) | 120 an hour | 300 an hour |
| reports | 20 an hour | 60 an hour |
| comments | 30 an hour | 60 an hour |
| catalog refreshes | 30 an hour | 60 an hour |
| account exports | 3 a day | 10 a day |
| account deletion attempts | 5 an hour | 10 an hour |
| live streams open at once | 20 per signed-in viewer | 50 |
| scrobbler API requests | 120 a minute | |

Past a limit the answer is `429` with the usual error body, without a
`Retry-After` header: wait for the window to end (the next minute, hour or
UTC day) rather than retrying at once. Conditional requests answered `304`
count too.

## Pagination

Lists come in pages, in one of two ways:

- **Cursors**, for lists that grow at the top (followers, blocks, loved
  tracks, the feed, a bio's history, your tags): pass `limit`, then the
  previous page's `next` as `before` for the page after it. `next` is
  `null` on the last page. A cursor is opaque; an invalid one is a `400`.
  Entries the viewer may not see are skipped, so a page can hold fewer
  than `limit` entries and still have a next one.
- **Offsets**, for ranked lists (charts, rankings, similar artists,
  recommendations, a tag's entities): pass `limit` and `offset`; the page
  says how many entries there are in all (`total`). A negative `offset` is
  a `400`; past the end, the page is empty.

`limit` is brought within each route's range (1 to its maximum) rather
than refused. Recent scrobbles page by time instead: `before` takes the
`played_at` of the last scrobble seen.

## Caching

Routes whose answers can be reused say so:

| route | `Cache-Control` | validator |
| --- | --- | --- |
| global rankings | `public, max-age=300, stale-while-revalidate=300` | `ETag` |
| similar artists | `public, max-age=3600, stale-while-revalidate=3600` | `ETag` |
| an entity's tags, a tag's entities | `public, max-age=300` | `ETag` |
| a user's charts | `private, max-age=60`, `Vary: Authorization` | `ETag` |
| community charts | `private, max-age=60` | `ETag` |
| recommendations | `private, no-cache` | `ETag` |
| the feed, the account export | `no-store` | |
| uploaded images | `public, max-age=31536000, immutable` | |

Send an `ETag` back in `If-None-Match`; while nothing changed the answer
is `304 Not Modified` with no body. Tags are weak (`W/"…"`): compression
changes the bytes, not the content.

## Who sees what

A profile is either public or private, and the rules below hold on every
route, the scrobbler APIs included.

- **Private profiles** are their owner's alone. The profile and
  everything under it (recent scrobbles, top lists, charts, the heatmap,
  follow lists, loved tracks, tags, taste neighbours, the live stream)
  answer `403` to everyone else, followers included: following takes no
  approval.
- **Blocks.** An account that blocks the signed-in viewer doesn't exist
  for it: its profile and routes answer `404`, exactly as for a missing
  account, and it is left out of every list of people. Nobody is told who
  blocks them. The blocker still sees the blocked account's profile, with
  `blocking: true`.
- **Accounts waiting to be deleted** answer `404` and appear nowhere,
  until they are restored or erased.
- **Lists of people** (followers, following, listeners, search, taste
  neighbours) name public accounts only, but for the viewer's own entry;
  they, comments and image candidates leave out accounts blocking the
  viewer or blocked by it and accounts waiting to be deleted.
- **Authors.** Comments, uploaded images and bio revisions keep their
  author's name whatever the profile's privacy: they were published for
  everyone. A bio revision's `author` is `null` when its author is waiting
  to be deleted, blocks the viewer or is blocked by it.
- **Counts that add up everyone** include private accounts, unnamed:
  catalog play counts, follower counts, rankings, tag counts. Those that
  could stand for a handful of people are held back below a minimum of
  distinct accounts set by the server: a track's, artist's or album's
  `scrobble_count` is then `null`, and rankings, tag lists and community
  charts leave the entry out (community charts state the minimum as
  `min_listeners`).
