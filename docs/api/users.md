# Users

A profile (`GET /v1/user/{username}`) is the account's public face: its
names, avatar, bio, scrobble count and follow counts. Usernames match
whatever their case. `GET /v1/user/me` is the signed-in account's own,
private or not.

```json
{
  "id": 70, "username": "ana", "display_name": "Ana", "bio": null,
  "image_url": "https://scrobblr.example/uploads/avatars/3f/3f2a….jpg",
  "website_url": null, "country": null, "scrobble_count": 4127,
  "is_private": false, "is_verified": false,
  "last_seen_at": "2026-10-13T10:41:07Z", "created_at": "2025-02-01T18:20:00Z",
  "is_following": true, "follows_you": false, "blocking": false,
  "followers_count": 12, "following_count": 35,
  "scrobble_breakdown": { "verified": 3990, "unverified": 137, "suspect": 2,
                          "no_data": 120, "pending": 15, "duplicates": 41 }
}
```

- `is_following`, `follows_you` and `blocking` describe the signed-in
  viewer's relation to the account, and are `null` signed out and on the
  viewer's own profile.
- `followers_count` and `following_count` count every follow, private
  accounts' included; the lists name only public ones.
- `scrobble_count` leaves out duplicates (one listen reported more than
  once); `scrobble_breakdown` splits it into `verified` and `unverified`
  plays (`suspect`, `no_data`, and `pending`, not checked yet).

**Settings** (`PATCH /v1/user/me`, `write`): `display_name` (40
characters at most), `bio` (1,000), `image_url` and `is_private`. Fields
left out stay as they are; an empty string clears one. `image_url` takes
an `https://` URL elsewhere; an uploaded avatar goes through
`POST /v1/user/me/avatar` (a multipart `image` field, re-encoded as a
JPEG of 512 pixels a side at most; the **Images** section has the limits),
and sending back the current avatar's URL leaves it as it is. Turning
private takes effect at once: others' live streams of the profile close.

**Listening** (the **Scrobbles** section): recent scrobbles, top artists
and tracks, the activity heatmap and a live stream of now playing, each as
visible as the profile.
