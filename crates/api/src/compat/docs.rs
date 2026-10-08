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
        `track.updateNowPlaying` (POST), `user.getInfo` and `user.getRecentTracks`. Parameters \
        come as a query string or a form body. Authenticated with a Scrobblr scrobbler \
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
        "mbid": string("MusicBrainz recording id, used as an enrichment hint"),
        "username": string("auth.getMobileSession: the Scrobblr username"),
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
        "400": { "description": "Invalid parameters (Last.fm error 6) or method (3)" },
        "403": { "description": "Invalid API key (10), session (9), signature (13) or authentication (4)" },
        "429": { "description": "Rate limit exceeded (29): requests, logins or the daily scrobble limit" },
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
                `duration`, `recording_mbid` (an enrichment hint) and the submitting client.",
            "parameters": [{ "name": "Authorization", "in": "header", "required": true,
                             "description": "`Token {scrobbler token}`", "schema": { "type": "string" } }],
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
                    "type": "object", "properties": { "status": { "type": "string", "enum": ["ok"] } } } } } },
                "400": listenbrainz_error("Malformed submission"),
                "401": listenbrainz_error("Missing or invalid token"),
                "429": listenbrainz_error("Too many requests, or the daily scrobble limit"),
                "503": listenbrainz_error("Temporarily unavailable")
            }
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
         drop the play), `l[i]`, `b[i]`, `n[i]`, `m[i]` for up to 50 plays.",
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
    ];
    let documented = api.paths.get_or_insert_with(Default::default);
    for (path, item) in paths {
        let item: PathItem = serde_json::from_value(item).expect("valid path item");
        documented
            .paths
            .insert(path.to_string(), ReferenceOr::Item(item));
    }
}
