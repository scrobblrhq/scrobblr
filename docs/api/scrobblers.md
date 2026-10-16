# Scrobblers

What a settings page needs to manage third-party scrobblers (the
**Scrobbler protocols** section), all with a session, never an API token:

- `POST /v1/scrobbler/tokens` with a `name` creates a scrobbler token,
  shown once in the answer (`token`). `legacy_auth` is `false` when the
  server can't keep the MD5 that Audioscrobbler 1.2 players need: such a
  token works everywhere else.
- `GET /v1/scrobbler/credentials` lists the tokens and the Last.fm
  sessions apps obtained (name, kind, API key, dates), never their
  secrets; `DELETE /v1/scrobbler/credentials/{id}` revokes one, signing its
  app out at once.
- **Approving an app in the browser.** A Last.fm-style app sends the user
  to `/api/auth/`, which forwards to the web app's
  `/scrobbler/authorize` with the same query. With `?api_key=…&token=…`
  (desktop apps), the page shows `GET /v1/scrobbler/authorizations/{token}`
  (which API key asks, and whether the request is pending) and approves
  with `POST /v1/scrobbler/authorizations/{token}/approve`; the app's next
  `auth.getSession` gets its session key. With `?api_key=…&cb=…` (web
  apps), `POST /v1/scrobbler/authorizations` with `api_key` and
  `callback` approves at once and answers the `redirect_url` to send the
  browser to.
