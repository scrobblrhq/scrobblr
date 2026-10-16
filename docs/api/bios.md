# Bios

Artists and albums have a bio written by the community, as a wiki is:
every edit is a revision, kept for good with its author, time and summary,
and an earlier one can be restored.

| route | access | |
| --- | --- | --- |
| `GET /v1/{artist,album}/{id}/bio` | public | the current text (`Bio`) |
| `GET /v1/{artist,album}/{id}/bio/history` | public | published revisions, newest first (`BioHistoryPage`) |
| `GET /v1/bio/revisions/{id}` | public | one revision with its text |
| `PUT /v1/{artist,album}/{id}/bio` | `write` | an edit: `200` published, `202` held for review |
| `POST /v1/{artist,album}/{id}/bio/revert` | `write` | restore an earlier revision: `200` |
| `POST /v1/reports` | `write` | report a revision or a tag: `204` |

```json
{
  "entity_type": "artist", "entity_id": 97, "revision_id": 5012,
  "body": "Massive Attack are a trip hop group from Bristol.\n\nFormed in 1988 …",
  "kind": "edit",
  "author": { "id": 812, "username": "alice", "display_name": "Alice", "image_url": null },
  "updated_at": "2026-10-15T14:02:11.408211Z", "locked": false, "revisions": 7
}
```

A revision (`BioRevision`) has its `id`, `parent_id` (the revision
current when it was written), `kind` (`edit`, `revert`, `import` from
Last.fm, `moderation`), `author`, `summary`, `body` (`null` in history
pages: fetch the revision), `length`, `reverted_to`, `status`
(`published`, `held`, `rejected`), `held_reason`, `hidden`, `current` and
`created_at`. History pages go by cursor (`limit` 50, at most 200;
`before`). Hidden revisions keep their place with `summary` and `body`
`null`.

**Editing.** `PUT` takes `body`, an optional `summary` (one line, 200
characters) and `base_revision_id`: the `revision_id` of the bio the text
was written over, `null` when it had none. If another revision came in
between, nothing is written: `409`, saying which revision is current, and
the client reads the bio again and merges. Sending the current text again
is a `400`. A revert takes `revision_id` (an earlier published revision of
the same bio), `base_revision_id` and `summary`. Both answer with the new
revision.

**The text** is plain: no HTML or Markdown. Show it as text, a paragraph
at each blank line, with `http://` and `https://` URLs as links marked
`rel="nofollow ugc noopener"`, never embedded or previewed. The server
normalizes line breaks and spacing and refuses (`400`) an empty text, more
than 10,000 characters, more than 5 links, and control, invisible or
bidirectional formatting characters.

**Who may edit:** an account some days after registering (a `403` says
from when), unless it can't edit for now (a `403` saying so) or the bio is
locked (`403`, the bio says `locked: true`). An edit that looks like spam
is held (`202`, with `held_reason`) until a moderator approves it: the bio
doesn't change, the edit isn't in the history, and only its author can
read it (`GET /v1/bio/revisions/{id}`). Reverts are never held. Limits are
in the introduction's rate limits.

**Authors** are named whatever their profile's privacy; `author` is `null`
for imported text and a moderator's revisions, and for an author waiting
to be deleted, blocking the viewer or blocked by it. After an account is
erased its published revisions stay, without an author.

**Reports** (`POST /v1/reports`) take a `revision_id`, or a `tag` (on the
entity `artist_id`, `album_id` or `track_id` names, or anywhere), a
`reason` (`spam`, `vandalism`, `offensive`, `personal_data`, `copyright`,
`other`) and an optional `note` (one line, 500 characters). Only what can
be seen can be reported (a `404` otherwise); reporting the same thing
again while the first report is open changes nothing. Nobody is told who
reported, and the reporter isn't told the outcome.

`GET /v1/artist/{id}` carries the current text as `bio`; an album has no
`bio` field: read its bio route.
