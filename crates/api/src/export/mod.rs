//! An account's export: a ZIP archive streamed as it is read, never stored.

pub mod zip;

use axum::body::Body;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use uuid::Uuid;

use crate::state::AppState;
use db::queries::{
    auth as auth_db, connected_accounts as connected_db, export as export_db,
    imports as imports_db, scrobblers as scrobblers_db, scrobbles as scrobbles_db,
    similarity as similarity_db, users as users_db,
};
use shared::media::UploadKey;
use shared::models::User;
use zip::ZipWriter;

const PAGE: i64 = 5_000;
const CHUNK: usize = 64 * 1024;
/// Chunks buffered for a slow client before reading pauses.
const IN_FLIGHT: usize = 4;
pub const FORMAT: &str = "scrobblr-export/1";

/// A failure once the response has started can only cut the archive short.
pub fn stream(
    state: AppState,
    user: User,
    session: Option<Uuid>,
    permit: OwnedSemaphorePermit,
) -> Body {
    let (tx, mut rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(IN_FLIGHT);
    tokio::spawn(async move {
        let _permit = permit;
        let user_id = user.id;
        let mut out = Out {
            zip: ZipWriter::new(),
            tx,
        };
        let started = std::time::Instant::now();
        match write(&state, user, session, &mut out).await {
            Ok(scrobbles) => tracing::info!(
                user_id,
                scrobbles,
                took_ms = started.elapsed().as_millis() as u64,
                "account exported"
            ),
            Err(Failure::ClientGone) => {
                tracing::info!(user_id, "account export abandoned by the client")
            }
            Err(Failure::Db(e)) => {
                tracing::error!(user_id, "account export failed: {e}");
                let _ = out
                    .tx
                    .send(Err(std::io::Error::other("export failed")))
                    .await;
            }
        }
    });
    Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

enum Failure {
    ClientGone,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

struct Out {
    zip: ZipWriter,
    tx: mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
}

impl Out {
    async fn flush_if_full(&mut self) -> Result<(), Failure> {
        if self.zip.buffered() >= CHUNK {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Failure> {
        let bytes = self.zip.take();
        if bytes.is_empty() {
            return Ok(());
        }
        self.tx
            .send(Ok(bytes))
            .await
            .map_err(|_| Failure::ClientGone)
    }

    async fn file(&mut self, name: &str, at: DateTime<Utc>, content: &[u8]) -> Result<(), Failure> {
        self.zip.start(name, at);
        for piece in content.chunks(CHUNK) {
            self.zip.write(piece);
            self.flush_if_full().await?;
        }
        self.zip.finish_entry();
        self.flush_if_full().await
    }
}

/// Returns how many scrobbles were written.
async fn write(
    state: &AppState,
    user: User,
    session: Option<Uuid>,
    out: &mut Out,
) -> Result<u64, Failure> {
    let now = Utc::now();
    out.file("README.txt", now, README.as_bytes()).await?;

    let (account, uploads) = account(state, &user, session, now).await?;
    let json = serde_json::to_vec_pretty(&account).expect("the account serializes");
    out.file("account.json", now, &json).await?;

    let written = scrobbles(state, user.id, out, Format::Csv, now).await?;
    scrobbles(state, user.id, out, Format::Json, now).await?;
    loved_tracks(state, user.id, out, now).await?;
    let tags = export_db::tags(&state.db, user.id).await?;
    let json = serde_json::to_vec_pretty(&tags).expect("tags serialize");
    out.file("tags.json", now, &json).await?;
    bio_edits(state, user.id, out, now).await?;

    for (path, key) in uploads {
        match state.media.read(&key).await {
            Ok(Some(bytes)) => {
                out.zip.add_stored(&path, now, &bytes);
                out.flush_if_full().await?;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(%key, "export: could not read an upload: {e}"),
        }
    }
    out.zip.finish();
    out.flush().await?;
    Ok(written)
}

#[derive(Clone, Copy)]
enum Format {
    Csv,
    Json,
}

const CSV_HEADER: &str = "played_at,artist,track,album,track_mbid,duration_ms,listened_ms,source,\
client_protocol,client_name,client_verified,import_id,status,reason,id\n";

async fn scrobbles(
    state: &AppState,
    user_id: i64,
    out: &mut Out,
    format: Format,
    now: DateTime<Utc>,
) -> Result<u64, Failure> {
    let (name, head, tail) = match format {
        Format::Csv => ("scrobbles.csv", CSV_HEADER, ""),
        Format::Json => ("scrobbles.json", "[", "\n]\n"),
    };
    out.zip.start(name, now);
    out.zip.write(head.as_bytes());
    let mut written = 0u64;
    let mut cursor = export_db::first_cursor(&state.db).await?;
    while let Some(after) = cursor {
        let page = export_db::scrobbles_page(&state.db, user_id, after, PAGE).await?;
        cursor = (page.len() as i64 == PAGE)
            .then(|| page.last().map(|s| (s.played_at, s.id)))
            .flatten();
        let mut text = String::with_capacity(page.len() * 200);
        for s in &page {
            match format {
                Format::Csv => csv_row(&mut text, s),
                Format::Json => {
                    text.push_str(if written == 0 { "\n" } else { ",\n" });
                    text.push_str(&serde_json::to_string(s).expect("a scrobble serializes"));
                }
            }
            written += 1;
        }
        out.zip.write(text.as_bytes());
        out.flush_if_full().await?;
    }
    out.zip.write(tail.as_bytes());
    out.zip.finish_entry();
    out.flush_if_full().await?;
    Ok(written)
}

fn csv_row(text: &mut String, s: &export_db::ExportedScrobble) {
    let opt = |v: Option<String>| v.unwrap_or_default();
    let fields = [
        s.played_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        s.artist.clone(),
        s.track.clone(),
        opt(s.album.clone()),
        opt(s.track_mbid.map(|m| m.to_string())),
        opt(s.duration_ms.map(|v| v.to_string())),
        opt(s.listened_ms.map(|v| v.to_string())),
        s.source.clone(),
        opt(s.client_protocol.clone()),
        opt(s.client_name.clone()),
        opt(s.client_verified.map(|v| v.to_string())),
        opt(s.import_id.map(|v| v.to_string())),
        opt(s.status.clone()),
        opt(s.reason.clone()),
        s.id.to_string(),
    ];
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        csv_field(text, field);
    }
    text.push('\n');
}

const LOVES_CSV_HEADER: &str = "loved_at,artist,track,album,track_mbid,source,client_protocol,\
client_name,client_verified,import_id\n";

async fn loved_tracks(
    state: &AppState,
    user_id: i64,
    out: &mut Out,
    now: DateTime<Utc>,
) -> Result<(), Failure> {
    let loves = export_db::loved_tracks(&state.db, user_id).await?;
    let mut csv = String::with_capacity(LOVES_CSV_HEADER.len() + loves.len() * 120);
    csv.push_str(LOVES_CSV_HEADER);
    let mut json = String::from("[");
    for (i, l) in loves.iter().enumerate() {
        let opt = |v: Option<String>| v.unwrap_or_default();
        let fields = [
            l.loved_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            l.artist.clone(),
            l.track.clone(),
            opt(l.album.clone()),
            opt(l.track_mbid.map(|m| m.to_string())),
            l.source.clone(),
            opt(l.client_protocol.clone()),
            opt(l.client_name.clone()),
            opt(l.client_verified.map(|v| v.to_string())),
            opt(l.import_id.map(|v| v.to_string())),
        ];
        for (j, field) in fields.iter().enumerate() {
            if j > 0 {
                csv.push(',');
            }
            csv_field(&mut csv, field);
        }
        csv.push('\n');
        json.push_str(if i == 0 { "\n" } else { ",\n" });
        json.push_str(&serde_json::to_string(l).expect("a love serializes"));
    }
    json.push_str("\n]\n");
    out.file("loved_tracks.csv", now, csv.as_bytes()).await?;
    out.file("loved_tracks.json", now, json.as_bytes()).await
}

/// Every bio revision the user wrote, a page of them a statement.
async fn bio_edits(
    state: &AppState,
    user_id: i64,
    out: &mut Out,
    now: DateTime<Utc>,
) -> Result<(), Failure> {
    out.zip.start("bio_edits.json", now);
    out.zip.write(b"[");
    let (mut after, mut first) = (0, true);
    loop {
        let page = export_db::revisions_page(&state.db, user_id, after, 500).await?;
        let mut text = String::new();
        for r in &page {
            text.push_str(if first { "\n" } else { ",\n" });
            text.push_str(&serde_json::to_string(r).expect("a revision serializes"));
            first = false;
        }
        out.zip.write(text.as_bytes());
        out.flush_if_full().await?;
        match page.last() {
            Some(r) if page.len() == 500 => after = r.id,
            _ => break,
        }
    }
    out.zip.write(b"\n]\n");
    out.zip.finish_entry();
    out.flush_if_full().await
}

/// A value a spreadsheet would run as a formula gets a `'` in front.
fn csv_field(text: &mut String, value: &str) {
    let formula = value.starts_with(['=', '+', '-', '@', '\t', '\r']);
    let quote = formula || value.contains([',', '"', '\n', '\r']);
    if quote {
        text.push('"');
    }
    if formula {
        text.push('\'');
    }
    if quote {
        text.push_str(&value.replace('"', "\"\""));
        text.push('"');
    } else {
        text.push_str(value);
    }
}

/// `account.json`, and the uploads to add as `(path, key)`.
async fn account(
    state: &AppState,
    user: &User,
    session: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<(Value, Vec<(String, UploadKey)>), sqlx::Error> {
    let db = &state.db;
    let breakdown = users_db::scrobble_breakdown(db, user.id).await?;
    let mut sessions = export_db::sessions(db, user.id).await?;
    for s in &mut sessions {
        s.current = Some(s.id) == session;
    }
    let imports: Vec<_> = imports_db::list(db, Some(user.id), i64::MAX)
        .await?
        .into_iter()
        .map(|(_, _, import)| import)
        .collect();

    let mut files = Vec::new();
    let mut file_for = |value: Option<&str>| -> Option<String> {
        let key = UploadKey::parse(value?)?;
        let path = format!("uploads/{}", key.as_str());
        files.push((path.clone(), key));
        Some(path)
    };
    let avatar_file = file_for(user.image_url.as_deref());
    let uploads: Vec<Value> = export_db::image_uploads(db, user.id)
        .await?
        .into_iter()
        .map(|u| {
            let file = file_for(Some(&u.url));
            let mut v = serde_json::to_value(&u).expect("an upload serializes");
            v["url"] = json!(shared::media::public_url(&u.url));
            v["file"] = json!(file);
            v
        })
        .collect();

    let similarity = similarity_db::setting(db, user.id).await?;
    let (recommendations, neighbours) = match similarity_db::version(db).await? {
        Some(version) => (
            similarity_db::recommendations(db, user.id, version.started_at)
                .await?
                .into_iter()
                .map(|r| {
                    json!({
                        "artist": r.name,
                        "because_of": r.because_name,
                        "computed_at": version.finished_at,
                    })
                })
                .collect(),
            similarity_db::taste_neighbours(db, user.id, None)
                .await?
                .into_iter()
                .map(|n| {
                    json!({
                        "username": n.username,
                        "shared_artists": n.shared_artists,
                        "computed_at": version.finished_at,
                    })
                })
                .collect(),
        ),
        None => (Vec::new(), Vec::new()),
    };

    let account = json!({
        "format": FORMAT,
        "generated_at": now,
        "server": state.public_base_url.as_ref(),
        "profile": {
            "id": user.id,
            "username": user.username,
            "email": user.email,
            "display_name": user.display_name,
            "bio": user.bio,
            "image_url": user.image_url.as_deref().map(shared::media::public_url),
            "image_file": avatar_file,
            "website_url": user.website_url,
            "country": user.country,
            "is_private": user.is_private,
            "is_verified": user.is_verified,
            "scrobble_count": user.scrobble_count,
            "scrobble_breakdown": breakdown,
            "last_seen_at": user.last_seen_at,
            "created_at": user.created_at,
            "updated_at": user.updated_at,
        },
        "following": export_db::follows(db, user.id, true).await?,
        "followers": export_db::follows(db, user.id, false).await?,
        "blocks": export_db::blocks(db, user.id).await?,
        "removed_followers": export_db::removed_followers(db, user.id).await?,
        "comments": export_db::comments(db, user.id).await?,
        "image_uploads": uploads,
        "image_votes": export_db::image_votes(db, user.id).await?,
        "sessions": sessions,
        "api_tokens": auth_db::list_api_tokens(db, user.id).await?,
        "scrobbler_credentials": scrobblers_db::list_credentials(db, user.id).await?,
        "scrobbler_authorizations": export_db::scrobbler_authorizations(db, user.id).await?,
        "connected_accounts": connected_db::list_connected_accounts(db, user.id).await?,
        "imports": imports,
        "now_playing": scrobbles_db::get_now_playing(db, user.id).await?,
        "registration_origin": export_db::origin_recorded_at(db, user.id).await?
            .map(|at| json!({ "recorded_at": at })),
        "similarity": similarity.map(|s| json!({
            "mode": s.mode,
            "changed_at": s.changed_at,
        })),
        "recommended_artists": recommendations,
        "taste_neighbours": neighbours,
        "skipped_recommendations": similarity_db::skips(db, user.id).await?,
        "reports": export_db::reports(db, user.id).await?,
        "editing_suspension": export_db::editing_suspension(db, user.id).await?,
        "not_included": NOT_INCLUDED
            .iter()
            .map(|(what, why)| json!({ "what": what, "why": why }))
            .collect::<Vec<_>>(),
    });
    Ok((account, files))
}

pub const NOT_INCLUDED: &[(&str, &str)] = &[
    (
        "Your password",
        "Only an Argon2 hash of it is stored, which is no use to you and would only help someone guess the password.",
    ),
    (
        "Session ids, API token values, scrobbler keys and authorization tokens",
        "They are working credentials (or hashes of them): the archive would sign in as you wherever it ends up. Their names, scopes and dates are listed.",
    ),
    (
        "Linked accounts' OAuth tokens",
        "Encrypted secrets that act on the other service (Spotify, Last.fm). Revoke Scrobblr's access there.",
    ),
    (
        "Your registration origin",
        "Keyed hashes (HMAC) of the address and network you registered from, kept for anti-abuse checks and deleted after 90 days by default. Without the server's key they are meaningless numbers, and the key can't be handed out without exposing everyone's. Only when it was recorded is listed.",
    ),
    (
        "Ranking weights, coordination factors and cohort membership",
        "Internal anti-abuse scores computed from your scrobbles, which are included with their labels. Publishing them would help tune botting. They never affect your profile.",
    ),
    (
        "Who blocks you, or removed you from their followers",
        "Their decisions, about you: the export holds what you did, and nobody is told who blocked or removed them.",
    ),
    (
        "The weight of your tags in the tag counts, and other people's revisions of the bios you edited",
        "The weight is computed from your account's age and the anti-abuse factors above; tags.json has every tag you put on something. Other people's revisions are theirs, and public in each bio's history.",
    ),
    (
        "Daily aggregates, catalog counters and album votes",
        "Totals computed from your scrobbles, and the album each track's last play naming one named, which counts towards the album the track is filed under: scrobbles.csv has them all.",
    ),
    (
        "Server logs, rate-limit counters and backups",
        "Operational copies outside the database's tables, which expire on their own (see the server's privacy notes).",
    ),
];

const README: &str = "\
Scrobblr account export
=======================

account.json     your profile and settings, follows, the accounts you block
                 and the followers you removed, comments, image uploads
                 and likes, sessions (without their ids), API tokens and
                 scrobbler credentials (names, scopes and dates, never their
                 values), linked accounts (never their OAuth tokens), imports,
                 now playing, your similarity setting, the artists
                 recommended to you and your taste neighbours as last
                 computed, the recommendations you skipped, your reports
                 of bio revisions and tags, an editing suspension if a
                 moderator set one, and what is not included and why
scrobbles.csv    every scrobble, oldest first: when, artist, track, album,
                 the lengths the client reported, the client (as it called
                 itself, and as the server identified it), the import that
                 brought it, and the classifier's label (counted, suspect,
                 duplicate, no_data; empty while not classified yet) with
                 its reason
scrobbles.json   the same scrobbles, as JSON
loved_tracks.csv the tracks you love, oldest love first: when, artist,
                 track, the album the catalog files it under, and how the
                 love came (scrobblr, lastfm, audioscrobbler or
                 lastfm_import) with the client or import that sent it
loved_tracks.json
                 the same loves, as JSON
tags.json        the tags you put on artists, albums and tracks, oldest
                 first, with how each came (scrobblr or lastfm)
bio_edits.json   every revision of an artist's or album's bio you wrote,
                 oldest first, with its text, whether it was published,
                 held for review or rejected, and whether a moderator hid
                 it; published ones stay in the bio's history, without
                 your name, after your account is deleted
uploads/         your avatar and the artist and album images you uploaded

Times are UTC. In the CSV files, a value that starts with = + - @ is written
with a ' in front, so a spreadsheet doesn't run it as a formula;
the JSON files have every value as they are.

What is not included, and why, is listed under not_included in account.json.
";
