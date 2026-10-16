# Scrobbler protocols

Scrobblr speaks the protocols existing scrobblers already know: the
Last.fm API (as Libre.fm and other GNU FM servers do), the older
Audioscrobbler 1.2 protocol, and ListenBrainz's. Most apps that let you
change the server need only the server URL and a token. These endpoints
live at the root of the API's address, and answer any web page (CORS).

## Setting up an app

**Make a scrobbler token**: in Scrobblr's settings, under Scrobblers,
create a token named after the app (`POST /v1/scrobbler/tokens`). It is
shown once. A scrobbler token scrobbles, loves and tags, and nothing else;
it never works on the native API, and revoking it signs the app out. Use
it wherever an app asks for a ListenBrainz token or an Audioscrobbler
password, and in Last.fm-style apps instead of your password: your
password then never leaves Scrobblr (some servers accept only tokens
there).

**Point the app at Scrobblr** (`https://scrobblr.example` stands for the
server's API address):

| app | choose | server URL | sign in with |
| --- | --- | --- | --- |
| Web Scrobbler | ListenBrainz, custom API URL | `https://scrobblr.example/1/submit-listens` | the token |
| Pano Scrobbler | GNU FM | `https://scrobblr.example/2.0/` | your username and the token |
| Pano Scrobbler | ListenBrainz, custom | `https://scrobblr.example/` | the token |
| mpris-scrobbler | `libre.fm` with `--url` | `https://scrobblr.example` | approve in the browser |
| Audioscrobbler 1.2 players (mpdscribe, Quod Libet's plugin, DeaDBeeF…) | custom submission URL | `https://scrobblr.example/` | your username and the token |
| other ListenBrainz clients | custom ListenBrainz server | `https://scrobblr.example/`, `…/1/` or `…/1/submit-listens`, whichever form the app asks for | the token |

Any other app with a setting for a Libre.fm or GNU FM server, a custom
Audioscrobbler 1.2 handshake URL or a custom ListenBrainz server should
work the same way. Apps that only talk to last.fm itself (Spotify's
Last.fm connection, streaming services' integrations, Web Scrobbler's
Last.fm and Libre.fm scrobblers) can't be pointed elsewhere: use
Scrobblr's own apps or its Spotify connection. Some Last.fm-style desktop
apps open a browser page to approve them; it is on the Scrobblr website:
sign in there and approve the app.

## What to expect

- Scrobbles go through the same checks as Scrobblr's own apps (the
  **Scrobbling** section). Sending a scrobble again (retries, an offline
  cache) records it once and counts as accepted.
- Only plays from the last 14 days are accepted, as on Last.fm; older ones
  are ignored. Bring older history over with the Last.fm import. A device
  clock up to 5 minutes fast is fine; more and its scrobbles are ignored.
- Limits: the account's daily allowance (3,000 by default), shared with
  every other API; 50 scrobbles per request (1,000 for a ListenBrainz
  import); 120 requests a minute per account; and the native limits on
  loves and tag changes.
- **Loves**: Last.fm apps love and unlove (`track.love`, `track.unlove`);
  a track the server hasn't seen is added to the catalog, as scrobbling it
  would. Audioscrobbler 1.2 players that rate a play as loved (`r=L`) love
  its track. ListenBrainz apps love through recording feedback (below).
  Loves show on your profile, as private as it is, and nowhere else.
- **Tags**: Last.fm apps add and remove your tags (`artist.addTags`,
  `.removeTag` and the same for albums and tracks) and read them back;
  tagging something the server has never seen is refused (error 6) rather
  than adding it to the catalog.
- **Web Scrobbler can't love tracks on Scrobblr.** Its Last.fm scrobbler
  talks only to last.fm, and its ListenBrainz scrobbler sends loves to
  listenbrainz.org itself, whatever server you set, with your Scrobblr
  token: listenbrainz.org refuses it, but the token has been sent there.
  If you scrobble from Web Scrobbler with a Scrobblr token, turn off
  "Love/unlove tracks through players" in its options (on by default) and
  don't press its love button; love from Scrobblr or another app instead.
- Pano Scrobbler shows track and artist info, bios and top tags from
  last.fm itself whatever server you set; Scrobblr's are on its own pages,
  and in `artist.getInfo` and the `getTopTags` methods for apps that ask
  the server.

## Last.fm API (`/2.0/`)

Requests as Last.fm's: a `method`, parameters in the query string or a
form body, answers in Last.fm's XML, or JSON with `format=json`, errors
with Last.fm's codes.

- **Signing in**: `auth.getMobileSession` (POST) with the username and a
  scrobbler token as the password (or the account password, where the
  server allows it) answers a session key; with a token, the session key
  is the token itself. The browser flow: `auth.getToken`, then send the
  user to `/api/auth/?api_key=…&token=…` (or `?api_key=…&cb=…` for the web
  flow) to approve, then `auth.getSession`. A session key (`sk`) is bound
  to the `api_key` it was issued to.
- **Signatures** (`api_sig`) are checked for API keys whose secret the
  server knows; others are accepted unless the server requires known keys.
  The credential is what authenticates, never the signature.
- **Methods**: `track.scrobble` (POST, up to 50 as `artist[i]`, `track[i]`,
  `timestamp[i]`…), `track.updateNowPlaying` (POST), `track.love`,
  `track.unlove` (POST), `track.getInfo` (by `artist` and `track` or
  `mbid`; `userloved` and `userplaycount` for a `username`, or the
  session's user, whose profile may be seen), `artist.getInfo` (the
  community bio, top tags, `userplaycount`), `artist.addTags`,
  `album.addTags`, `track.addTags` (POST, `tags` comma-separated, 10 at
  most), `.removeTag` (POST), `.getTags` (a user's tags on it), `.getTopTags`
  (the tags shown on it), `user.getInfo`, `user.getRecentTracks`
  (`extended=1` for the artist as an object and `loved`; 200 a page at
  most), `user.getLovedTracks` (1,000 a page at most), `user.getTopTags`.
  There are no `listeners` counts, and `playcount` is left out where the
  native API's count would be `null`.
- **Not served**: tag pages (`tag.*`), `album.getInfo`, charts, friends,
  library and similar-artist methods (error 3).
- **Visibility**: as on the native API. A private profile answers error
  17 but to its own session; an account blocking the session's account,
  waiting to be deleted or missing answers error 6.
- **Errors**: 6 invalid parameters (also a cap reached, an invalid tag, an
  unknown artist or track), 3 an unknown method, 4, 9, 10 and 13
  authentication (a suspended account's tagging too), 29 a rate limit, 16
  temporarily unavailable.

## Audioscrobbler 1.2

The handshake (`GET /?hs=true&p=1.2&u=USER&t=TIMESTAMP&a=AUTH`, also at
`/1.2/`) takes the username and `md5(md5(token) + timestamp)`, within 5
minutes of the server's clock: it needs a scrobbler token made while the
server could keep its MD5 (`legacy_auth` true when created). It answers a
session id and the now-playing (`/1.2/nowplaying`) and submission
(`/1.2/submissions`) URLs. Submissions take up to 50 plays (`a[i]`,
`t[i]`, `i[i]`, `o[i]`, `r[i]`, `l[i]`, `b[i]`, `n[i]`, `m[i]`); `r[i]=S`
or `B` drops a play, `L` loves its track. Sessions last a day after their
last use and end when the token is revoked.

## ListenBrainz (`/1/`)

Authenticated with the scrobbler token as `Authorization: Token <token>`.

- `POST /1/submit-listens`: `listen_type` `single`, `playing_now` or
  `import` (1,000 listens at most), `track_metadata` with `artist_name`,
  `track_name`, `release_name` and `additional_info` (`duration_ms`,
  `recording_mbid`, `submission_client`…). With `return_msid`, a
  `playing_now` answers the track's `recording_msid`.
- `GET /1/validate-token`: whether the token is valid, and its user.
- `POST /1/feedback/recording-feedback`: `recording_mbid` or
  `recording_msid`, and `score` 1 (love), 0 or -1 (take a love back; hates
  aren't kept). A love of a recording the catalog doesn't have is a `404`
  (scrobble it first).
- `GET /1/feedback/user/{user_name}/get-feedback` (`count` 25 by default,
  1,000 at most, `offset`, `score`, `metadata=true`): the user's loves,
  newest first. `get-feedback-for-recordings` (GET or POST,
  `recording_msids`, `recording_mbids`, 1,000 at most): 1 or 0 for each.
  Both are as visible as the profile and take no token.
- A recording MBID names the track MusicBrainz matched to it, or one a
  client named it for. An MSID is one this server handed out
  (`recording_msid`), never listenbrainz.org's.
- Not served: who loved a recording, and how many (loves have no public
  count), and the rest of ListenBrainz's API.
