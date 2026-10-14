//! The scrobbler-compatible endpoints in the OpenAPI spec. Their handlers
//! read raw requests in other services' formats, so they are described
//! here by hand rather than inferred; docs/scrobbler-clients.md has the
//! client setup.

use aide::openapi::{OpenApi, PathItem, ReferenceOr};
use serde_json::{Value, json};

const TAG: &str = "Scrobbler protocols";

fn text(name: &str, description: &str) -> Value {
    json!({ "name": name, "in": "query", "description": description, "schema": { "type": "string" } })
}

fn form(fields: Value) -> Value {
    json!({
        "content": {
            "application/x-www-form-urlencoded": {
                "schema": { "type": "object", "properties": fields, "additionalProperties": true }
            }
        }
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn plain(description: &str) -> Value {
    json!({ "description": description, "content": { "text/plain": { "schema": { "type": "string" } } } })
}

fn lastfm() -> Value {
    let description = "Last.fm API 2.0 for scrobblers: `auth.getToken`, `auth.getSession`, \
        `auth.getMobileSession` (POST), `track.scrobble` (POST, up to 50 as `artist[i]` …), \
        `track.updateNowPlaying` (POST), `track.love` and `track.unlove` (POST), `track.getInfo` \
        (by `artist` and `track` or `mbid`; with `username` or a session, `userloved` and \
        `userplaycount` where that user's profile may be seen; no `listeners`, and `playcount` \
        only past `RANKING_MIN_LISTENERS`), `user.getInfo`, \
        `user.getRecentTracks` (with `extended=1`, the artist as an object and `loved`) and \
        `user.getLovedTracks`. Parameters come as a query string or a form body. Authenticated with a Scrobblr scrobbler \
        credential as `sk` (a session key, or a scrobbler token given as the password to \
        `auth.getMobileSession`), never with native API tokens. `api_sig` is checked for API \
        keys the server knows the secret of. Answers in Last.fm's XML, or JSON with \
        `format=json`; errors carry Last.fm's codes.";
    let params = json!({
        "method": string("Last.fm method name, e.g. track.scrobble"),
        "api_key": string("The client's Last.fm API key (at most 64 characters)"),
        "api_sig": string("Last.fm request signature"),
        "sk": string("Session key (a scrobbler credential)"),
        "format": string("`json` for JSON answers; XML otherwise"),
        "artist": string("Artist; `artist[i]` in batches"),
        "track": string("Track; `track[i]` in batches"),
        "album": string("Album; `album[i]` in batches"),
        "timestamp": string("Unix seconds the play started; `timestamp[i]` in batches"),
        "duration": string("Track length in seconds"),
        "mbid": string("MusicBrainz recording id: an enrichment hint when scrobbling, the track to look up in track.getInfo"),
        "username": string("track.getInfo: whose `userloved` and `userplaycount` to show; also auth.getMobileSession's Scrobblr username"),
        "autocorrect": string("track.getInfo: accepted and ignored; names match as scrobbling matches them (case and surrounding spaces aside)"),
        "user": string("user.getInfo, user.getRecentTracks, user.getLovedTracks: the Scrobblr username; the session's when absent"),
        "limit": string("user.getRecentTracks (200 at most), user.getLovedTracks (1000 at most): entries a page, 50 by default"),
        "page": string("Page number, from 1"),
        "extended": string("user.getRecentTracks: `1` for the artist as an object and `loved`"),
        "password": string("auth.getMobileSession: a scrobbler token, or the account password when allowed"),
        "token": string("auth.getSession: the authorized token"),
    });
    let responses = json!({
        "200": {
            "description": "Last.fm-shaped answer",
            "content": {
                "text/xml": { "schema": { "type": "string" } },
                "application/json": { "schema": { "type": "object" } }
            }
        },
        "400": { "description": "Invalid parameters (Last.fm error 6, also a love past 50,000 loved tracks) or method (3)" },
        "403": { "description": "Invalid API key (10), session (9), signature (13) or authentication (4)" },
        "429": { "description": "Rate limit exceeded (29): requests, logins, loves or the daily scrobble limit" },
        "503": { "description": "Temporarily unavailable (16)" }
    });
    json!({
        "get": {
            "tags": [TAG], "summary": "Last.fm API 2.0 (read methods)", "description": description,
            "parameters": [text("method", "Last.fm method name"), text("api_key", "The client's API key"), text("format", "`json` for JSON answers")],
            "responses": responses
        },
        "post": {
            "tags": [TAG], "summary": "Last.fm API 2.0", "description": description,
            "requestBody": form(params), "responses": responses
        }
    })
}

fn browser_authorization() -> Value {
    json!({
        "get": {
            "tags": [TAG],
            "summary": "Last.fm browser authorization",
            "description": "Where Last.fm-API clients send the user's browser: `?api_key=…&token=…` \
                (desktop flow) or `?api_key=…&cb=…` (web flow). Redirects, with the same query, to \
                the web app's `/scrobbler/authorize` (`WEB_APP_URL`), which approves through \
                `/v1/scrobbler/authorizations`.",
            "parameters": [
                text("api_key", "The client's Last.fm API key"),
                text("token", "Desktop flow: the token from auth.getToken"),
                text("cb", "Web flow: where to send the browser back to")
            ],
            "responses": {
                "303": { "description": "To `{WEB_APP_URL}/scrobbler/authorize?{query}`" },
                "503": { "description": "WEB_APP_URL isn't set (an HTML page explains token logins)",
                         "content": { "text/html": { "schema": { "type": "string" } } } }
            }
        }
    })
}

fn handshake() -> Value {
    json!({
        "get": {
            "tags": [TAG],
            "summary": "Audioscrobbler 1.2 handshake",
            "description": "`?hs=true&p=1.2.1&c=…&v=…&u=USER&t=UNIX&a=md5(md5(TOKEN)+t)`, where TOKEN \
                is a scrobbler token made while TOKEN_ENCRYPTION_KEY was set; or the web-services \
                form with `api_key` and `sk`. Answers `OK`, a session id and the now-playing and \
                submission URLs, or `BADAUTH`, `BADTIME`, `FAILED …`. Without `hs=true`: 404.",
            "parameters": [
                text("hs", "`true`"), text("p", "Protocol version, 1.2 or 1.2.1"),
                text("c", "Client id"), text("v", "Client version"), text("u", "Username"),
                text("t", "Unix time, within 5 minutes of the server's"),
                text("a", "md5(md5(token) + t)"), text("api_key", "Web-services form: API key"),
                text("sk", "Web-services form: session key")
            ],
            "responses": { "200": plain("`OK\\n{session}\\n{now-playing URL}\\n{submissions URL}`, or a failure line"), "404": { "description": "Not a handshake (no `hs=true`)" } }
        }
    })
}

fn as12_post(summary: &str, description: &str, fields: Value) -> Value {
    json!({
        "post": {
            "tags": [TAG], "summary": summary, "description": description,
            "requestBody": form(fields),
            "responses": { "200": plain("`OK`, `BADSESSION` or `FAILED …`") }
        }
    })
}

fn listenbrainz_error(description: &str) -> Value {
    json!({
        "description": description,
        "content": { "application/json": { "schema": {
            "type": "object",
            "properties": { "code": { "type": "integer" }, "error": { "type": "string" } }
        } } }
    })
}

fn submit_listens() -> Value {
    json!({
        "post": {
            "tags": [TAG],
            "summary": "ListenBrainz: submit listens",
            "description": "ListenBrainz's submission API with a scrobbler token as the user token \
                (`Authorization: Token …`). `listen_type` is `single`, `playing_now` or `import` \
                (up to 1000 listens). `track_metadata.additional_info` may carry `duration_ms` or \
                `duration`, `recording_mbid` (an enrichment hint) and the submitting client. \
                With `return_msid`, a `playing_now` answer carries the track's `recording_msid`, \
                for loving it through the feedback API.",
            "parameters": [
                { "name": "Authorization", "in": "header", "required": true,
                  "description": "`Token {scrobbler token}`", "schema": { "type": "string" } },
                text("return_msid", "Any value but an empty one: a `playing_now` answer carries `recording_msid`")
            ],
            "requestBody": { "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["listen_type", "payload"],
                "properties": {
                    "listen_type": { "type": "string", "enum": ["single", "playing_now", "import"] },
                    "payload": { "type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "listened_at": { "type": "integer", "description": "Unix seconds; not for playing_now" },
                            "track_metadata": { "type": "object", "required": ["artist_name", "track_name"], "properties": {
                                "artist_name": { "type": "string" },
                                "track_name": { "type": "string" },
                                "release_name": { "type": "string" },
                                "additional_info": { "type": "object", "additionalProperties": true }
                            } }
                        }
                    } }
                }
            } } } },
            "responses": {
                "200": { "description": "Accepted", "content": { "application/json": { "schema": {
                    "type": "object", "properties": {
                        "status": { "type": "string", "enum": ["ok"] },
                        "recording_msid": { "type": "string", "format": "uuid", "description": "With `return_msid`, for `playing_now`" }
                    } } } } },
                "400": listenbrainz_error("Malformed submission"),
                "401": listenbrainz_error("Missing or invalid token"),
                "429": listenbrainz_error("Too many requests, or the daily scrobble limit"),
                "503": listenbrainz_error("Temporarily unavailable")
            }
        }
    })
}

fn recording_feedback() -> Value {
    json!({
        "post": {
            "tags": [TAG],
            "summary": "ListenBrainz: love or unlove a recording",
            "description": "ListenBrainz's feedback with a scrobbler token. `score` 1 loves the \
                recording's track, 0 takes the love back; -1 (hate) takes it back too and is not \
                stored. The recording is a `recording_mbid` (the track MusicBrainz enrichment \
                matched, else one a client sent it for) or a `recording_msid` this server handed \
                out (`submit-listens` with `return_msid`, or the read side). A love of a recording \
                the catalog doesn't have is refused (404), never created. Counted against the love \
                limits shared with every API (300 an hour per account, 1,000 per address), whatever \
                the score; 50,000 loved tracks at most. Loves feed nothing: no ranking, \
                recommendation or similarity reads them.",
            "parameters": [{ "name": "Authorization", "in": "header", "required": true,
                             "description": "`Token {scrobbler token}`", "schema": { "type": "string" } }],
            "requestBody": { "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["score"],
                "additionalProperties": false,
                "properties": {
                    "recording_mbid": { "type": "string", "format": "uuid" },
                    "recording_msid": { "type": "string", "format": "uuid" },
                    "score": { "type": "integer", "enum": [-1, 0, 1] }
                }
            } } } },
            "responses": {
                "200": { "description": "Done, or nothing to do", "content": { "application/json": { "schema": {
                    "type": "object", "properties": { "status": { "type": "string", "enum": ["ok"] } } } } } },
                "400": listenbrainz_error("Malformed feedback: a bad score, no recording, an invalid UUID or another key"),
                "401": listenbrainz_error("Missing or invalid token"),
                "404": listenbrainz_error("A love of a recording the catalog doesn't have"),
                "409": listenbrainz_error("The account loves 50,000 tracks already"),
                "429": listenbrainz_error("Too many requests, or loves and unloves"),
                "503": listenbrainz_error("Temporarily unavailable")
            }
        }
    })
}

fn feedback_entry() -> Value {
    json!({ "type": "object", "properties": {
        "created": { "type": ["integer", "null"], "description": "Unix seconds the love was made" },
        "recording_mbid": { "type": ["string", "null"], "format": "uuid" },
        "recording_msid": { "type": ["string", "null"], "format": "uuid" },
        "score": { "type": "integer", "enum": [0, 1] },
        "track_metadata": { "type": ["object", "null"], "additionalProperties": true },
        "user_id": { "type": "string", "description": "The username" }
    } })
}

fn feedback_reads_common() -> (Value, Value) {
    let params = json!([
        { "name": "user_name", "in": "path", "required": true, "schema": { "type": "string" } },
        { "name": "Authorization", "in": "header",
          "description": "`Token {scrobbler token}`: optional, names the viewer (its own private profile)",
          "schema": { "type": "string" } }
    ]);
    let errors = json!({
        "403": listenbrainz_error("The profile is private"),
        "404": listenbrainz_error("No such user, one waiting to be deleted, or one blocking the token's account"),
        "429": listenbrainz_error("Too many requests")
    });
    (params, errors)
}

fn get_feedback() -> Value {
    let (mut params, errors) = feedback_reads_common();
    let params = params.as_array_mut().expect("an array");
    params.extend([
        text(
            "score",
            "`1` for loves (the default lists them too); `-1` lists nothing: hates aren't kept",
        ),
        text("count", "Entries, 25 by default, 1000 at most"),
        text("offset", "Entries to skip"),
        text("metadata", "`true` for each track's `track_metadata`"),
    ]);
    let mut responses = json!({
        "200": { "description": "The user's loves, newest first", "content": { "application/json": { "schema": {
            "type": "object", "properties": {
                "feedback": { "type": "array", "items": feedback_entry() },
                "count": { "type": "integer" },
                "total_count": { "type": "integer" },
                "offset": { "type": "integer" }
            } } } } },
        "400": listenbrainz_error("A score other than 1 or -1, or a bad count or offset")
    });
    responses
        .as_object_mut()
        .expect("an object")
        .extend(errors.as_object().cloned().unwrap_or_default());
    json!({
        "get": {
            "tags": [TAG],
            "summary": "ListenBrainz: a user's loved recordings",
            "description": "A user's loved tracks as ListenBrainz feedback (score 1), newest first, as \
                visible as the profile. Each carries `recording_msid` (this server's, for \
                unloving), and `recording_mbid` when MusicBrainz enrichment matched the track.",
            "parameters": params,
            "responses": responses
        }
    })
}

fn get_feedback_for_recordings() -> Value {
    let (params, errors) = feedback_reads_common();
    let mut get_params = params.clone();
    get_params.as_array_mut().expect("an array").extend([
        text(
            "recording_msids",
            "Comma-separated MSIDs (`recordings`, deprecated, also)",
        ),
        text("recording_mbids", "Comma-separated MBIDs"),
    ]);
    let mut responses = json!({
        "200": { "description": "One entry per recording asked about, in order: score 1 if loved, else 0",
                 "content": { "application/json": { "schema": {
            "type": "object", "properties": { "feedback": { "type": "array", "items": feedback_entry() } } } } } },
        "400": listenbrainz_error("No recording, an invalid UUID, or more than 1000")
    });
    responses
        .as_object_mut()
        .expect("an object")
        .extend(errors.as_object().cloned().unwrap_or_default());
    let description = "The user's score for each recording asked about: 1 for a loved track, \
        0 otherwise (unknown recordings included), as visible as the profile.";
    json!({
        "get": {
            "tags": [TAG],
            "summary": "ListenBrainz: a user's scores for recordings",
            "description": description,
            "parameters": get_params,
            "responses": responses
        },
        "post": {
            "tags": [TAG],
            "summary": "ListenBrainz: a user's scores for recordings",
            "description": description,
            "parameters": params,
            "requestBody": { "content": { "application/json": { "schema": {
                "type": "object", "properties": {
                    "recording_msids": { "type": "array", "items": { "type": "string", "format": "uuid" } },
                    "recording_mbids": { "type": "array", "items": { "type": "string", "format": "uuid" } }
                }
            } } } },
            "responses": responses
        }
    })
}

fn validate_token() -> Value {
    json!({
        "get": {
            "tags": [TAG],
            "summary": "ListenBrainz: validate a token",
            "description": "Whether a scrobbler token is valid, and whose it is.",
            "parameters": [
                { "name": "Authorization", "in": "header", "description": "`Token {scrobbler token}`", "schema": { "type": "string" } },
                text("token", "Deprecated: the token as a query parameter")
            ],
            "responses": {
                "200": { "description": "`valid` says whether the token is good", "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "integer" }, "message": { "type": "string" },
                        "valid": { "type": "boolean" }, "user_name": { "type": "string" }
                    }
                } } } },
                "400": listenbrainz_error("No token given")
            }
        }
    })
}

/// Adds the protocol routes `compat::router` serves to `api`.
pub fn document(api: &mut OpenApi) {
    let as12_now_playing = as12_post(
        "Audioscrobbler 1.2: now playing",
        "Form fields `s` (session), `a`, `t`, `b`, `l` (seconds), `n`, `m` (MusicBrainz recording id).",
        json!({
            "s": string("Session id from the handshake"), "a": string("Artist"), "t": string("Track"),
            "b": string("Album"), "l": string("Length in seconds"), "m": string("MusicBrainz recording id"),
        }),
    );
    let as12_submissions = as12_post(
        "Audioscrobbler 1.2: submissions",
        "Form fields `s`, then `a[i]`, `t[i]`, `i[i]` (Unix seconds), `o[i]`, `r[i]` (`S` or `B` \
         drop the play, `L` loves its track), `l[i]`, `b[i]`, `n[i]`, `m[i]` for up to 50 plays.",
        json!({
            "s": string("Session id from the handshake"), "a[0]": string("Artist"), "t[0]": string("Track"),
            "i[0]": string("Unix seconds the play started"), "b[0]": string("Album"),
            "l[0]": string("Length in seconds"), "m[0]": string("MusicBrainz recording id"),
        }),
    );
    let paths = [
        ("/2.0/", lastfm()),
        ("/2.0", lastfm()),
        ("/api/auth/", browser_authorization()),
        ("/api/auth", browser_authorization()),
        ("/", handshake()),
        ("/1.2/", handshake()),
        ("/1.2", handshake()),
        ("/1.2/nowplaying", as12_now_playing),
        ("/1.2/submissions", as12_submissions),
        ("/1/submit-listens", submit_listens()),
        ("/1/validate-token", validate_token()),
        ("/1/feedback/recording-feedback", recording_feedback()),
        ("/1/feedback/user/{user_name}/get-feedback", get_feedback()),
        (
            "/1/feedback/user/{user_name}/get-feedback-for-recordings",
            get_feedback_for_recordings(),
        ),
    ];
    let documented = api.paths.get_or_insert_with(Default::default);
    for (path, item) in paths {
        let item: PathItem = serde_json::from_value(item).expect("valid path item");
        documented
            .paths
            .insert(path.to_string(), ReferenceOr::Item(item));
    }
}
