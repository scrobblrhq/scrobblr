# Connected accounts

A user links accounts elsewhere: **Spotify**, which the server then polls
for what the user played and scrobbles it, and **Last.fm**, which proves
the user owns the account whose history they import. Linking is a browser
flow started and finished by the same session (never an API token):

1. `GET /v1/connect/{provider}` (`spotify` or `lastfm`) answers an
   `authorize_url`; send the user's browser there.
2. The provider sends the browser back to the web app's
   `/connect/{provider}/callback` with a `state` and Spotify's `code` or
   Last.fm's `token` in the query.
3. The web app finishes with `POST /v1/connect/{provider}/callback`,
   `{ "state": "…", "code": "…" }` (or `"token"`), within 10 minutes and
   with the session of the user who started: a `state` belonging to
   another user, used or expired is a `400`; an account already linked to
   another Scrobblr user a `409`.

`GET /v1/connect` (`read`) lists the linked accounts, never their tokens;
`DELETE /v1/connect/{provider}` (`write`) unlinks one, and polling stops.
A server without a provider's keys answers `503`.
