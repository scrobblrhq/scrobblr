# Scrobbling from other apps

Scrobblr speaks the protocols existing scrobblers already know: the Last.fm
API (as Libre.fm and other GNU FM servers do), the older Audioscrobbler 1.2
protocol, and ListenBrainz's. Most apps that let you change the server only
need that server URL and a token. Below, `https://scrobblr.example` stands for
your Scrobblr server, the address its API is reachable on.

## 1. Make a scrobbler token

In Scrobblr's settings, under Scrobblers, create a token and give it the
name of the app it's for. It's shown once. A token can scrobble and love
or unlove tracks, and do nothing else; you can revoke it at any time.

Use it wherever an app asks for a ListenBrainz token or an Audioscrobbler
password. In Last.fm-style apps, use it instead of your password too: your
password then never leaves Scrobblr (some servers only accept tokens there).

## 2. Point the app at Scrobblr

| App | Choose | Server URL | Sign in with |
| --- | --- | --- | --- |
| Web Scrobbler | ListenBrainz, custom API URL | `https://scrobblr.example/1/submit-listens` | the token |
| Pano Scrobbler | GNU FM | `https://scrobblr.example/2.0/` | your username and the token |
| Pano Scrobbler | ListenBrainz, custom | `https://scrobblr.example/` | the token |
| mpris-scrobbler | `libre.fm` with `--url` | `https://scrobblr.example` | approve in the browser |
| Audioscrobbler 1.2 players (mpdscribe, Quod Libet's plugin, DeaDBeeF…) | custom submission URL | `https://scrobblr.example/` | your username and the token |
| Other ListenBrainz clients | custom ListenBrainz server | `https://scrobblr.example/`, `…/1/` or `…/1/submit-listens`, whichever form the app asks for | the token |

Any other app with a setting for a Libre.fm or GNU FM server, a custom
Audioscrobbler 1.2 handshake URL, or a custom ListenBrainz server should work
the same way. Apps that only talk to last.fm itself (Spotify's Last.fm
connection, official streaming-service integrations, Web Scrobbler's Last.fm
and Libre.fm scrobblers) can't be pointed elsewhere; use Scrobblr's own apps
or its Spotify connection for those.

Some Last.fm-style desktop apps open a browser page to approve them. That
page is on the Scrobblr website: sign in there and approve the app.

## What to expect

- Scrobbles go through the same checks as Scrobblr's own apps. An app sending
  the same scrobble again (retries, an offline cache) records it once.
- Only plays from the last 14 days are accepted, as on Last.fm. Bring older
  history over with the Last.fm import in settings.
- A device clock running up to 5 minutes fast is fine; more and its scrobbles
  are ignored as being in the future.
- Limits: 3,000 scrobbles a day per account (the server can change this), 50
  per request (1,000 for a ListenBrainz import), 120 requests a minute.
- Supported: scrobbling, now playing, and in Last.fm apps loving and
  unloving tracks, tagging artists, albums and tracks and reading your
  tags back, your profile, recent scrobbles (with whether you love each
  track), loved tracks, your most used tags, track and artist info (with
  the community bio and tags). Audioscrobbler 1.2 players that rate a play
  as loved love its track too. Other Last.fm features (tag pages, album
  info, charts, friends) are not.
- Tags: Pano Scrobbler's GNU FM account adds and removes your tags from
  its info screen and reads them back; they are your Scrobblr tags, shown
  on your profile. Tagging something the server has never seen scrobbled
  is refused (error 6) rather than adding it to the catalog. Tags count
  against 120 tag changes an hour per account (300 per address), shared
  with Scrobblr's own apps. Pano shows artist and album info, bios and top
  tags from last.fm itself whatever server you set; the tags and bios
  Scrobblr shows are on its own pages, and in `artist.getInfo` and
  `getTopTags` for apps that ask the server (pylast-based tools).
- Loving a track the server hasn't seen yet adds it to the catalog, as
  scrobbling it would. Loves count against 300 loves and unloves an hour
  per account (1,000 per address), shared with Scrobblr's own apps, and an
  account can love 50,000 tracks; past either, the app gets Last.fm's
  error 29 or 6. Loves show on your profile, as private as it is, and
  nowhere else: they don't count in rankings or recommendations.
- `track.getInfo` answers `userloved` and `userplaycount` for a `username`
  whose profile you may see (pylast-based tools read them); Pano Scrobbler
  asks last.fm for track info whatever server you set, so its hearts come
  from the loved tracks it reads here.
- ListenBrainz apps can love: Pano Scrobbler's ListenBrainz account loves
  what is playing (the server hands it a `recording_msid`) and lists your
  loves; feedback by recording MBID works for tracks the catalog knows
  (matched by MusicBrainz, or named by a client). A hate (-1) only takes a
  love back; nothing else is kept.
- Web Scrobbler can't love tracks on Scrobblr. Its Last.fm scrobbler talks
  only to last.fm, and its ListenBrainz scrobbler sends loves to
  listenbrainz.org itself, whatever server you set, with your Scrobblr
  token: listenbrainz.org refuses it, but the token has been sent there.
  Revoke it if that worries you. The ListenBrainz feedback endpoints this
  server now has don't change that: Web Scrobbler never calls them. If you
  scrobble from Web Scrobbler through its ListenBrainz scrobbler with a
  Scrobblr token, turn off "Love/unlove tracks through players" in its
  options (on by default: liking a song on a website sends the love) and
  don't press its love button. Love from Scrobblr or another app instead.
- Your Last.fm loved tracks come over with the Last.fm import.

## For server operators

The endpoints live at the API's root: `/2.0/` (Last.fm), `/api/auth/` (its
browser approval, forwarded to the web app), `/` and `/1.2/` (Audioscrobbler
1.2 handshake) with `/1.2/nowplaying` and `/1.2/submissions`, and
`/1/submit-listens` and `/1/validate-token` (ListenBrainz). Settings:

- `PUBLIC_BASE_URL`: the Audioscrobbler handshake hands out URLs built on it.
- `WEB_APP_URL`: where `/api/auth/` sends browsers to approve an app.
- `TOKEN_ENCRYPTION_KEY`: needed for Audioscrobbler 1.2. Its handshake proves
  knowledge of the password's MD5, so the server keeps that, encrypted, for
  tokens made while the key is set.
- `SCROBBLER_PASSWORD_LOGIN`: whether Last.fm-style logins accept account
  passwords besides tokens. On by default, off when `AUTH_APP_KEYS` is set.
- `SCROBBLER_API_KEYS`, `SCROBBLER_STRICT_API_KEYS`: Last.fm API keys whose
  secrets you know (their request signatures are then checked), and whether
  to refuse all others.
- `SCROBBLER_DAILY_LIMIT`: scrobbles per account per UTC day (default 3000),
  counted together with the native `/v1/scrobble`.
