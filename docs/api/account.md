# Account

A user can take their data with them and delete their account. Both
routes take a session, never an API token: a leaked token mustn't carry
the account away or delete it.

**Export** (`GET /v1/user/me/export`) streams a ZIP archive of everything
the server holds about the account, read as it is sent:

| file | |
| --- | --- |
| `README.txt` | what the files are |
| `account.json` | the profile and settings, follows, blocks made and followers removed, comments, uploads and likes, sessions, API tokens and scrobbler credentials (without their values), linked accounts (without their tokens), imports, now playing, the similarity setting, recommendations and taste neighbours as last computed, skipped recommendations, reports made, and `not_included`: what is left out, and why |
| `scrobbles.csv`, `scrobbles.json` | every scrobble, oldest first, with its source, client, import and `status` |
| `loved_tracks.csv`, `loved_tracks.json` | every loved track, with when and how it was loved |
| `tags.json` | every tag the user put on something |
| `bio_edits.json` | every bio revision the user wrote, with its text and status |
| `uploads/` | the avatar and artwork the user uploaded |

Nothing is stored on the server; a broken download is asked for again.
3 exports a day per account, 10 per address (`429`); while other exports
are running the answer can be `503`: try again in a few minutes. The
answer is `application/zip` with `Cache-Control: no-store`.

**Deletion** (`POST /v1/user/me/delete`) takes the account's `password`
and its username typed out as `confirm`:

```json
{ "password": "…", "confirm": "alice" }
```

It answers `202` with `delete_after`, when the account will be erased. A
wrong password is a `403`, a `confirm` that isn't the username a `400`; 5
attempts an hour per account, 10 per address.

- **At once**: every session, API token, scrobbler credential and linked
  account is revoked, imports stop, and the account disappears: its
  profile and routes answer `404`, and it leaves every list.
- **Until `delete_after`** (7 days by default): logging in with the
  password (`POST /v1/auth/login`) restores the account, history, follows
  and loves included, and says so (`account_restored: true`); credentials
  and linked accounts have to be made again. Scrobbler apps' logins never
  restore it. The username and email stay taken.
- **Then** the server erases everything about the account. Its plays
  leave catalog counts and rankings; published bio revisions stay without
  an author; artwork it uploaded that an artist or album shows stays as
  the catalog's. The username and email can be taken again.
