# Images

Users upload their avatar and propose artwork for artists and albums.
Every upload is a multipart body with one `image` file field:

```http
POST /v1/artist/97/image HTTP/1.1
Authorization: Bearer <token>
Content-Type: multipart/form-data; boundary=x

--x
Content-Disposition: form-data; name="image"; filename="cover.png"
Content-Type: image/png

<bytes>
--x--
```

- **Formats**: JPEG, PNG or WebP, recognised from the file's contents, at
  most 8 MiB, 12,000 pixels a side and 40 megapixels. Anything else is a
  `400`, a larger body a `413`.
- **What is stored**: the server re-encodes every image as a JPEG, at
  most 512 pixels a side for avatars and 1,024 for artwork, transparency
  on white; nothing of the original file survives (EXIF and location data
  included). Image fields hold its URL, which never changes; a new image
  gets a new URL.
- **Limits**: 20 uploads an hour per account, 30 per address, refused
  ones included (`429`).

**Avatars** (`POST /v1/user/me/avatar`, `write`) replace the account's
avatar at once and answer with the updated profile; the previous uploaded
avatar is deleted.

**Artwork** works as on Last.fm: an upload
(`POST /v1/{artist,album}/{id}/image`, `write`) adds a candidate, which
its uploader likes. `GET /v1/{artist,album}/{id}/images` lists the
candidates, most liked first, with `has_voted` for the signed-in viewer and
`is_default` for the one shown; `POST` and `DELETE /v1/image/{id}/vote`
(`write`) like a candidate or take the like back, answering with the
refreshed list. The most liked candidate becomes the entity's image once
enough people like it, and then stays: a like taken back never takes an
image down, and the server's own lookups never replace it. A candidate
shows its uploader's username whatever the profile's privacy.
