# Auth

Registering, logging in and out, and the account's API tokens (the
introduction's **Authentication** section explains sessions, tokens and
scopes).

- `POST /v1/auth/register` takes `username` (2 to 30 ASCII letters,
  digits and `_ - .`, starting and ending with a letter or digit), `email`,
  `password` (12 to 128 characters mixing at least three of lowercase,
  uppercase, digits and symbols, not containing the username or email)
  and an optional `display_name`; it answers `201` with a session, `409`
  when the username or email is taken.
- `POST /v1/auth/login` takes `username` and `password` and answers a
  session (`token`, `expires_at`); `account_restored` is `true` when the
  login cancelled the account's deletion. A wrong password or unknown user
  is a `401`.
- `POST /v1/auth/logout` ends the session (`?all=true`: every session of
  the account).
- `POST /v1/auth/tokens` (session) creates an API token: `name` (1 to 100
  characters), `scopes` (default `["scrobble"]`; an unknown scope or an
  empty list is a `400`) and `expires_days` (1 to 3,650; never expires
  when left out). The `token` is in this answer only.
  `GET /v1/auth/tokens` lists them without their values;
  `DELETE /v1/auth/tokens/{id}` (session) revokes one at once.

```json
{ "token": "3d0b1f7c-1c2e-4f61-9a43-5d6e2b8f9a10", "user_id": 812, "username": "alice",
  "expires_at": "2026-11-15T10:41:07Z", "account_restored": false }
```
