# Social

**Follows** take no approval and open nothing: a private profile stays
its owner's alone, followers included.

| route | access | |
| --- | --- | --- |
| `POST /v1/user/{username}/follow` | `write` | `204`, also when already following; `400` yourself; `404` no such account; `409` at the cap, an account you block, or one that removed you from its followers in the last 30 days |
| `DELETE /v1/user/{username}/follow` | `write` | `204` |
| `GET /v1/user/{username}/followers`, `/following` | public | a page, newest follow first (`FollowPage`) |
| `GET /v1/user/{username}/friends` | public | the 100 most recent of each list |

An account may follow 2,000 others. Follow lists page by cursor (`limit`
50, at most 200; `before`); each entry is a profile with `followed_at`,
and for a signed-in viewer `is_following` (the viewer follows it) and
`follows_you` (it follows the viewer).

**Blocks** (session only, never an API token):

| route | |
| --- | --- |
| `POST /v1/user/{username}/block` | `204`, also when already blocking; `400` yourself; `404` no such account; `409` at the cap (10,000) |
| `DELETE /v1/user/{username}/block` | `204`, also when not blocking |
| `GET /v1/user/me/blocks` | the accounts you block, newest first (`BlockPage`); a private one shows its username only |
| `DELETE /v1/user/me/followers/{username}` | remove a follower without blocking it: `204` |

A block ends the follows between the two accounts both ways, and neither
can follow the other while it stands. The blocked account, signed in,
finds no such account: every route of the blocker answers `404` as for a
missing account, and the blocker is left out of every list it reads. It
is never told, but a public profile is anyone's to see signed out: a
block hides an account from an account, not from a person. A removed
follower keeps seeing your profile but can't follow you again for 30
days.

**The feed** (`GET /v1/user/me/feed`, `read`): the last 14 days of what
the accounts you follow played, newest first, from the public ones you
may see. Plays found suspect and duplicates are left out; plays not
checked yet are in with `status: null`.

```json
{
  "entries": [
    { "id": 912873, "played_at": "2026-10-13T10:41:07Z",
      "user": { "id": 70, "username": "ana", "display_name": "Ana", "image_url": null },
      "track_id": 1841, "track_title": "Teardrop", "artist_id": 97,
      "artist_name": "Massive Attack", "album_id": 31, "album_title": "Mezzanine",
      "album_image": null, "artist_image": null, "status": "counted", "loved": false }
  ],
  "next": "1760352067000000.912873"
}
```

Pages go by cursor (`limit` 50, at most 100; `before`) and stay where
they are however many plays arrive: poll the first page, and stop paging
back at an entry you already have (`id` is the scrobble's). `next` is
`null` once a page reaches the 14th day. There is no stream.

Community charts, of the accounts you follow, are in the **Charts**
section.
