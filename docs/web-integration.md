# Web app integration

The contract between this API and the Scrobblr web app (SvelteKit on
adapter-node). The browser talks only to the web app; the web app's server
calls the API with the user's session.

```
browser ─▶ Cloudflare ─▶ Caddy ─▶ web app (scrobblr.app) ─▶ API (http://api:8080)
mobile app, extension, scrobblers ─▶ Cloudflare ─▶ Caddy ─▶ API (api.scrobblr.app)
images: cdn.scrobblr.app, absolute URLs in the API's answers
```

`docker compose --profile caddy --profile web up` runs all of it
(`deploy/Caddyfile`, the `web` service in `docker-compose.yml`, the hosts in
`.env.docker.example`).

## Running the web app

| Setting | Value | Why |
| --- | --- | --- |
| `SCROBBLR_API_URL` | `http://api:8080` | The API on the Compose network. Never `https://api.scrobblr.app` from the server: that goes out through Cloudflare, and the API would see the server as every user. |
| `ADDRESS_HEADER` | `X-Forwarded-For` | Caddy replaces this header with the user's address, so `event.getClientAddress()` is the user. |
| `XFF_DEPTH` | `1` | Caddy is the one proxy that header comes from. |
| `BODY_SIZE_LIMIT` | `9M` | Uploads pass through the web app; the API takes up to 8 MiB (adapter-node's default is 512K). |
| `ORIGIN` | `https://scrobblr.app` | adapter-node 5 (SvelteKit 2) only. adapter-node 6 takes the origin from the `Host` header Caddy passes on, with `https`, or from `paths.origin`. |

The `web` service in `docker-compose.yml` sets all of them. With
`ADDRESS_HEADER` set, adapter-node fails a request without the header once
it asks for the address, so nothing but Caddy should reach the web app.

## Sessions

- **Register**: `POST /v1/auth/register` `{ username, email, password, display_name? }`
  → `201 { token, user_id, username, expires_at }`.
- **Log in**: `POST /v1/auth/login` `{ username, password }` → `200`, same
  body. `401` for a wrong password, `429` after too many attempts (below).
- Keep `token` in a cookie of the web app's own:
  `HttpOnly; Secure; SameSite=Lax; Path=/`, expiring at `expires_at`. A
  session lasts 30 days from login; using it doesn't extend it.
- A `401` from any other call means the session is over: clear the cookie
  and send the user to log in.
- **Log out**: `POST /v1/auth/logout` ends this session only (the user's
  phone stays signed in); `POST /v1/auth/logout?all=true` ends all of them.
  Clear the cookie either way.
- A session may do everything. The web app never needs an API token; it
  only lists, creates (`POST /v1/auth/tokens`, the token is shown once) and
  revokes them for the user's scripts and players.
- If the server sets `AUTH_APP_KEYS`, register and login must be signed:
  `X-App-Id`, `X-App-Timestamp` (Unix seconds), `X-App-Nonce` (16 to 128
  letters and digits, never reused) and `X-App-Signature`, the hex
  HMAC-SHA256 with the app's key of
  `METHOD\nPATH\nTIMESTAMP\nNONCE\nHEX(SHA256(BODY))`, the path with its
  query string if any. The web app's server can keep its key secret, unlike
  the extension or the mobile app: give it an app id of its own.

## Every call to the API

| Header | Value |
| --- | --- |
| `X-Forwarded-For` | `event.getClientAddress()`: exactly one address, replacing whatever the request came with. Every limit counts it; without it, all users share the web server's. |
| `Authorization` | `Bearer {token}`, when the user is signed in. |
| `Content-Type` | `application/json`, for JSON bodies. |

Send nothing else from the browser's request (cookies least of all).

```ts
// A server-only module, e.g. src/lib/server/api.ts
import { env } from '$env/dynamic/private';
import type { RequestEvent } from '@sveltejs/kit';

export function api(event: RequestEvent, path: string, init: RequestInit = {}) {
  const headers = new Headers(init.headers);
  headers.set('x-forwarded-for', event.getClientAddress());
  const session = event.cookies.get('session');
  if (session) headers.set('authorization', `Bearer ${session}`);
  return fetch(`${env.SCROBBLR_API_URL}${path}`, { ...init, headers });
}
```

Use the global `fetch`, not `event.fetch`, which passes the browser's
cookies on to a subdomain such as `api.scrobblr.app`.

## Errors

Errors are JSON, `{ "error": "message" }`; the message is for people, not
a stable code. The statuses:

| Status | Meaning |
| --- | --- |
| 400 | Invalid input |
| 401 | No session, or an expired or ended one; a wrong password at login |
| 403 | A private profile, or a credential not allowed this call |
| 404 | Not found |
| 409 | Taken: a username, an email, an account linked to another user |
| 413 | An upload over 8 MiB |
| 422 | A scrobble refused |
| 429 | A limit (below) |
| 503 | A feature this server isn't configured for |
| 500 | The server's fault (logged, never detailed) |

The OpenAPI spec lists each endpoint's. The framework answers a few in
plain text instead: a JSON body of the wrong shape (`422`), malformed JSON
or a path or query parameter that doesn't parse (`400`), a body without
`Content-Type: application/json` (`415`); an unknown path gets an empty
`404`.

## Limits

Counted per user address, the `X-Forwarded-For` the web app sends (an IPv6
address by its /64), in fixed windows; over them, `429` with no
`Retry-After`:

- 60 calls a minute to the API. Every call the web app makes for that user
  counts, so a page that makes five calls uses five. Keep calls per page
  few, and preload on tap rather than hover
  (`data-sveltekit-preload-data="tap"`).
- Logins: 20 per address and 10 per address and username, in 15 minutes. A
  success resets the second.
- Uploads: 20 an hour per account, 30 per address.
- Live streams: 50 open per address, 20 per signed-in viewer.

Show a message on a `429`; don't retry in a loop.

## Live now playing

`GET /v1/user/{username}/live` is a Server-Sent Events stream: first the
current state (`null` when nothing plays), then an event per change, each a
JSON `NowPlayingRich`; an idle stream sends a comment every 15 seconds. A
private profile answers `403` unless the session is its owner's or a
follower's.

The web app passes it through a route of its own:

```ts
// e.g. src/routes/live/[username]/+server.ts
import type { RequestHandler } from './$types';

export const GET: RequestHandler = async (event) => {
  const path = `/v1/user/${encodeURIComponent(event.params.username)}/live`;
  const upstream = await api(event, path, { headers: { accept: 'text/event-stream' } });
  if (!upstream.ok) {
    return new Response(upstream.body, {
      status: upstream.status,
      headers: { 'content-type': 'application/json' },
    });
  }
  return new Response(upstream.body, {
    headers: {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      'x-accel-buffering': 'no',
    },
  });
};
```

and the page opens `new EventSource('/live/' + username)`, which reconnects
by itself.

- Return `upstream.body` as it is, as `text/event-stream`, with no
  compression on the route: Caddy, Cloudflare and the API then deliver each
  event at once.
- A browser that leaves must end the web app's call to the API too, or it
  keeps one of that user's stream slots. Returning `upstream.body` does
  that: SvelteKit cancels it when the browser goes away, and the API frees
  the slot within a couple of seconds (no `AbortSignal` needed). A stream
  of your own wrapping it must cancel `upstream.body` in its `cancel()`.
- Don't put a timeout on the route: the comments keep proxies from closing
  it (Cloudflare closes after 100 silent seconds).

This was run end to end (Caddy, SvelteKit 3 with adapter-node 6, the API):
each event reached the browser as it was published.

## Uploads

`POST /v1/user/me/avatar`, `/v1/artist/{id}/image` and
`/v1/album/{id}/image` take multipart with an `image` field: JPEG, PNG or
WebP, up to 8 MiB. Pass the browser's body through as it is:

```ts
await api(event, '/v1/user/me/avatar', {
  method: 'POST',
  headers: { 'content-type': event.request.headers.get('content-type')! },
  body: event.request.body,
  duplex: 'half',
} as RequestInit);
```

Image URLs in the API's answers are absolute (`https://cdn.scrobblr.app/…`):
use them as they are.

## Pages the API sends browsers to

`WEB_APP_URL` is `https://scrobblr.app`; the API sends browsers to these
paths on it, so they must exist. Each one signs the user in first if needed,
coming back with the same query.

### `/scrobbler/authorize`

A Last.fm-style player (mpris-scrobbler, some desktop apps) asks to scrobble
as the user; its query is `?api_key=…&token=…` or `?api_key=…&cb=…`.

- With `token`: `GET /v1/scrobbler/authorizations/{token}` →
  `{ api_key, status, expires_at }`, `status` being `pending`, `approved` or
  `expired`. Show which app asks (`api_key`); on approval,
  `POST /v1/scrobbler/authorizations/{token}/approve` (`204`), then tell the
  user to go back to the app.
- With `cb`: show the app and the host of `cb`, where the user will be sent;
  on approval, `POST /v1/scrobbler/authorizations` `{ api_key, callback: cb }`
  → `{ redirect_url }`, and send the browser there (303).

### `/connect/spotify/callback` and `/connect/lastfm/callback`

Linking an account: a button on the settings page calls
`GET /v1/connect/{provider}` (`spotify` or `lastfm`) and sends the browser
to the `authorize_url` it answers. The provider sends it back here, with
`?code=…&state=…` (Spotify; `?error=…` when the user declined) or
`?state=…&token=…` (Last.fm). The page calls
`POST /v1/connect/{provider}/callback` `{ state, code }` or
`{ state, token }` with the session and shows how it went: `200 { provider,
connected }`, `400` (declined, expired after 10 minutes, or started by
another account), `409` (that account is linked to another user).

Spotify's redirect URI, registered in its dashboard and set in
`SPOTIFY_REDIRECT_URI`, is `https://scrobblr.app/connect/spotify/callback`.

### `/user/{username}`

The profile page; the Last.fm-compatible API links there. Its numbers leave
out the scrobbles the classifier found to be the same listen reported
again, and say how much of the rest it could verify, so a flood of botted
plays doesn't pass for listening ([docs/rankings.md](rankings.md)):

- `GET /v1/user/{username}` has `scrobble_breakdown`: `verified` +
  `unverified` = `scrobble_count`, with `unverified` split into `suspect`
  (more listening than real time allows), `no_data` (tracks with no known
  length) and `pending` (not classified yet), and `duplicates` beside them.
  Show `scrobble_count` as before; show the unverified part next to it
  when it isn't small.
- Top artists and tracks have `unverified_count` per entry; recent
  scrobbles a `status` (`counted`, `suspect`, `no_data`, or `null` until
  classified), for a mark on suspect plays.

### Keeping these pages safe

- Approve, start a link and log out with POSTs (form actions), never on a
  GET; SvelteKit refuses form posts from other origins, so keep that on (in
  SvelteKit 2, `csrf.checkOrigin`). The callback pages finish a link on the
  provider's GET: the API's `state` check is what protects them.
- Don't let other sites frame them: `Content-Security-Policy:
  frame-ancestors 'none'`, or `X-Frame-Options: DENY`.
- An approval lets that app scrobble as the user, and a `cb` sends it a
  token: say plainly what is being approved and where the browser will go.

## Types

The OpenAPI spec is the contract: served at
`https://api.scrobblr.app/api.json` (browsable at `/docs`), and committed as
`openapi.json` in this repo, where CI fails when it no longer matches the
code. Generate types from it:

```bash
npx openapi-typescript https://api.scrobblr.app/api.json -o src/lib/api/schema.d.ts
```

and use them with the helper above, or with
[openapi-fetch](https://openapi-ts.dev/openapi-fetch/). The spec covers
every request and response body. `@scrobblr/types` on npm has only the
shared models, and will give way to the spec.

## CORS

The API sends browsers no CORS headers (`CORS_ALLOWED_ORIGINS` unset):
pages get nothing from it directly. Should the web app ever need that, set
`CORS_ALLOWED_ORIGINS=https://scrobblr.app` on the API.
