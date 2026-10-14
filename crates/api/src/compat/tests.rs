//! End-to-end tests of the scrobbler-compatible APIs through the router,
//! replaying the requests real clients send (their parameters, encodings
//! and signing as their source code does it). `#[ignore]`d: they need
//! Postgres (a throwaway database per test) and Redis (`just test-db`).

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};

use super::CompatConfig;
use crate::test_app::{BASE_URL, PASSWORD, TestApp, with_app, with_custom_app};
use db::queries::auth as auth_db;
use shared::lastfm::md5_hex;

fn form(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Pano Scrobbler's `toFormParametersWithSig`: drops empty values and
/// `format`, sorts the rest by name (a Kotlin TreeMap), appends the
/// secret, MD5s, then adds `api_sig` and `format=json`.
fn pano_signed(params: &[(&str, &str)], secret: &str) -> Vec<(String, String)> {
    let mut kept: Vec<(String, String)> = params
        .iter()
        .filter(|(k, v)| !v.is_empty() && !k.eq_ignore_ascii_case("format"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    kept.sort();
    let mut base: String = kept.iter().map(|(k, v)| format!("{k}{v}")).collect();
    base.push_str(secret);
    kept.push(("api_sig".into(), md5_hex(&base)));
    kept.push(("format".into(), "json".into()));
    kept
}

/// Last.fm's documented signature, as C clients (mpris-scrobbler) and
/// libraries build it: every parameter but `format`, sorted, then secret.
fn documented_signed(params: &[(&str, &str)], secret: &str) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut signed: Vec<&(String, String)> = all.iter().filter(|(k, _)| k != "format").collect();
    signed.sort();
    let mut base: String = signed.iter().map(|(k, v)| format!("{k}{v}")).collect();
    base.push_str(secret);
    all.push(("api_sig".into(), md5_hex(&base)));
    all
}

fn pairs(owned: &[(String, String)]) -> Vec<(&str, &str)> {
    owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

impl TestApp {
    async fn new_token(&self, name: &str) -> String {
        let (status, created) = self
            .api(
                Method::POST,
                "/v1/scrobbler/tokens",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["credential"]["legacy_auth"], true);
        created["token"].as_str().unwrap().to_string()
    }

    async fn listen_brainz(&self, auth: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/1/submit-listens")
            .header(header::AUTHORIZATION, auth)
            .header(header::CONTENT_TYPE, "application/json; charset=UTF-8")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _, body) = self.send(req).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn scrobbles(&self) -> Vec<(String, String, Option<String>, Option<bool>)> {
        sqlx::query_as(
            r#"
            SELECT t.title, s.source, c.name, c.verified
            FROM scrobbles s
            JOIN tracks t ON t.id = s.track_id
            LEFT JOIN scrobble_clients c ON c.id = s.client_id
            ORDER BY s.played_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

fn ago(minutes: i64) -> String {
    (Utc::now() - TimeDelta::minutes(minutes))
        .timestamp()
        .to_string()
}

/// Pano Scrobbler's GNU FM mode: `auth.getMobileSession` with the account
/// password, then now playing, a scrobble and a cached batch, retried.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn pano_scrobbler_logs_in_scrobbles_and_retries_without_duplicates() {
    with_app(CompatConfig::default(), |app| async move {
        let login = pano_signed(
            &[
                ("method", "auth.getMobileSession"),
                ("api_key", "panoScrobbler"),
                ("username", &app.username),
                ("password", PASSWORD),
            ],
            "panoScrobbler",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&login)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let session: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(session["session"]["name"], app.username.as_str());
        let sk = session["session"]["key"].as_str().unwrap().to_string();

        let now_playing = pano_signed(
            &[
                ("method", "track.updateNowPlaying"),
                ("artist", "Slowdive"),
                ("track", "Alison"),
                ("duration", "227"),
                ("album", "Souvlaki"),
                ("trackNumber", "1"),
                ("albumArtist", ""),
                ("sk", &sk),
                ("api_key", "panoScrobbler"),
                ("format", "json"),
            ],
            "panoScrobbler",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&now_playing)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let np: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(np["nowplaying"]["ignoredMessage"]["code"], "0");
        assert_eq!(np["nowplaying"]["ignoredMessage"]["#text"], "");
        assert_eq!(np["nowplaying"]["track"]["#text"], "Alison");
        let playing: String = sqlx::query_scalar("SELECT source FROM now_playing")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(playing, "lastfm");

        let single = pano_signed(
            &[
                ("method", "track.scrobble"),
                ("artist", "Slowdive"),
                ("track", "Alison"),
                ("duration", "227"),
                ("album", "Souvlaki"),
                ("trackNumber", "1"),
                ("timestamp", &ago(10)),
                ("chosenByUser", "1"),
                ("sk", &sk),
                ("api_key", "panoScrobbler"),
                ("format", "json"),
            ],
            "panoScrobbler",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&single)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(result["scrobbles"]["@attr"]["accepted"], 1);
        assert_eq!(result["scrobbles"]["@attr"]["ignored"], 0);
        assert_eq!(
            result["scrobbles"]["scrobble"]["ignoredMessage"]["code"],
            "0"
        );

        // Pano's offline cache, flushed as one indexed batch.
        let (t0, t1, t2) = (ago(400), ago(396), ago(392));
        let mut batch = vec![
            ("method", "track.scrobble"),
            ("sk", sk.as_str()),
            ("api_key", "panoScrobbler"),
            ("format", "json"),
        ];
        let tracks = [("Machine Gun", &t0), ("40 Days", &t1), ("Sing", &t2)];
        let names: Vec<[String; 6]> = (0..3)
            .map(|i| {
                [
                    format!("artist[{i}]"),
                    format!("track[{i}]"),
                    format!("duration[{i}]"),
                    format!("album[{i}]"),
                    format!("timestamp[{i}]"),
                    format!("chosenByUser[{i}]"),
                ]
            })
            .collect();
        for (i, (track, at)) in tracks.iter().enumerate() {
            batch.extend([
                (names[i][0].as_str(), "Slowdive"),
                (names[i][1].as_str(), *track),
                (names[i][2].as_str(), "240"),
                (names[i][3].as_str(), "Souvlaki"),
                (names[i][4].as_str(), at.as_str()),
                (names[i][5].as_str(), "1"),
            ]);
        }
        let batch = pano_signed(&batch, "panoScrobbler");
        for _ in 0..2 {
            let (status, body) = app.post_form("/2.0/", &pairs(&batch)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let result: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(result["scrobbles"]["@attr"]["accepted"], 3, "{body}");
            assert_eq!(result["scrobbles"]["scrobble"].as_array().unwrap().len(), 3);
        }

        let stored = app.scrobbles().await;
        assert_eq!(stored.len(), 4);
        assert!(stored.iter().all(|(_, source, client, verified)| {
            source == "lastfm"
                && client.as_deref() == Some("panoScrobbler")
                && *verified == Some(true)
        }));
        // The same pipeline as native scrobbles: classification and enrichment.
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM classification_queue")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert!(queued >= 1);
        let enrichment: i64 = sqlx::query_scalar("SELECT count(*) FROM enrichment_jobs")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert!(enrichment >= 4);
    })
    .await;
}

/// A desktop client (auth.getToken, browser approval, auth.getSession)
/// with an API key the server doesn't know, answered in XML.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn desktop_authorization_through_the_browser_with_xml_answers() {
    let web =
        |state: &mut crate::state::AppState| state.web_app_url = Some("https://web.test".into());
    with_custom_app(web, |app| async move {
        let (key, secret) = ("0123456789abcdef0123456789abcdef", "packager-secret");
        let get_token = documented_signed(&[("method", "auth.getToken"), ("api_key", key)], secret);
        let query = form(&pairs(&get_token));
        let (status, headers, body) = app.get(&format!("/2.0/?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/xml")
        );
        let token = body
            .split("<token>")
            .nth(1)
            .and_then(|rest| rest.split("</token>").next())
            .unwrap()
            .to_string();

        let get_session = documented_signed(
            &[
                ("method", "auth.getSession"),
                ("api_key", key),
                ("token", &token),
            ],
            secret,
        );
        let query = form(&pairs(&get_session));
        let (status, _, body) = app.get(&format!("/2.0/?{query}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("<error code=\"14\">"), "{body}");

        // The browser is forwarded to the web app's approval page.
        let (status, headers, _) = app
            .get(&format!("/api/auth/?api_key={key}&token={token}"))
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            headers[header::LOCATION],
            format!("https://web.test/scrobbler/authorize?api_key={key}&token={token}").as_str()
        );
        let (status, request) = app
            .api(
                Method::GET,
                &format!("/v1/scrobbler/authorizations/{token}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (request["api_key"].as_str(), request["status"].as_str()),
            (Some(key), Some("pending"))
        );
        let (status, _) = app
            .api(
                Method::POST,
                &format!("/v1/scrobbler/authorizations/{token}/approve"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _, body) = app.get(&format!("/2.0/?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains(&format!("<name>{}</name>", app.username)),
            "{body}"
        );
        let sk = body
            .split("<key>")
            .nth(1)
            .and_then(|rest| rest.split("</key>").next())
            .unwrap()
            .to_string();
        // A token yields one session.
        let (status, _, body) = app.get(&format!("/2.0/?{query}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("<error code=\"4\">"), "{body}");

        let (recent, old) = (ago(5), ago(20 * 24 * 60));
        let scrobble = documented_signed(
            &[
                ("method", "track.scrobble"),
                ("api_key", key),
                ("sk", &sk),
                ("artist[0]", "Sigur Rós"),
                ("track[0]", "Hoppípolla"),
                ("timestamp[0]", &recent),
                ("album[0]", "Takk..."),
                ("artist[1]", "Sigur Rós"),
                ("track[1]", "Glósóli"),
                ("timestamp[1]", &old),
            ],
            secret,
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&scrobble)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("<scrobbles accepted=\"1\" ignored=\"1\">"),
            "{body}"
        );
        assert!(body.contains("<track corrected=\"0\">Hoppípolla</track>"));
        assert!(body.contains("<ignoredMessage code=\"3\">Timestamp was too old</ignoredMessage>"));

        let (status, _, body) = app
            .get(&format!(
                "/2.0/?method=user.getRecentTracks&user={}&api_key={key}",
                app.username
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<name>Hoppípolla</name>"), "{body}");
        assert!(body.contains("total=\"1\""), "{body}");

        let stored = app.scrobbles().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].2.as_deref(), Some(key));
        assert_eq!(stored[0].3, Some(false));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn signatures_keys_and_sessions_are_enforced() {
    let config = CompatConfig {
        api_keys: [("known".to_string(), "known-secret".to_string())].into(),
        strict_api_keys: true,
        ..Default::default()
    };
    with_app(config, |app| async move {
        let login = |key: &'static str, secret: &'static str| {
            documented_signed(
                &[
                    ("method", "auth.getMobileSession"),
                    ("api_key", key),
                    ("username", "someone"),
                    ("password", "x"),
                    ("format", "json"),
                ],
                secret,
            )
        };
        let (status, body) = app
            .post_form("/2.0/", &pairs(&login("known", "forged")))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 13);
        let (status, body) = app.post_form("/2.0/", &pairs(&login("other", "x"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 10);

        // A session works only with the key it was issued to, and only
        // while it exists.
        let session = documented_signed(
            &[
                ("method", "auth.getMobileSession"),
                ("api_key", "known"),
                ("username", &app.username),
                ("password", PASSWORD),
                ("format", "json"),
            ],
            "known-secret",
        );
        let (_, body) = app.post_form("/2.0/", &pairs(&session)).await;
        let sk = serde_json::from_str::<Value>(&body).unwrap()["session"]["key"]
            .as_str()
            .unwrap()
            .to_string();
        let now_playing = |sk: &str| {
            documented_signed(
                &[
                    ("method", "track.updateNowPlaying"),
                    ("api_key", "known"),
                    ("sk", sk),
                    ("artist", "A"),
                    ("track", "T"),
                    ("format", "json"),
                ],
                "known-secret",
            )
        };
        let (status, body) = app.post_form("/2.0/", &pairs(&now_playing(&sk))).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (_, listed) = app
            .api(Method::GET, "/v1/scrobbler/credentials", None)
            .await;
        let id = listed[0]["id"].as_str().unwrap().to_string();
        assert_eq!(listed[0]["kind"], "session");
        assert_eq!(listed[0]["api_key"], "known");
        let (status, _) = app
            .api(
                Method::DELETE,
                &format!("/v1/scrobbler/credentials/{id}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = app.post_form("/2.0/", &pairs(&now_playing(&sk))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 9);

        // Write methods must be POSTed.
        let query = form(&pairs(&now_playing("x")));
        let (status, _, _) = app.get(&format!("/2.0/?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn session_keys_are_bound_to_the_api_key_that_got_them() {
    with_app(CompatConfig::default(), |app| async move {
        let login = documented_signed(
            &[
                ("method", "auth.getMobileSession"),
                ("api_key", "first-client"),
                ("username", &app.username),
                ("password", PASSWORD),
                ("format", "json"),
            ],
            "unknown",
        );
        let (_, body) = app.post_form("/2.0/", &pairs(&login)).await;
        let sk = serde_json::from_str::<Value>(&body).unwrap()["session"]["key"]
            .as_str()
            .unwrap()
            .to_string();
        let scrobble = |key: &str| {
            documented_signed(
                &[
                    ("method", "track.scrobble"),
                    ("api_key", key),
                    ("sk", &sk),
                    ("artist", "A"),
                    ("track", "T"),
                    ("timestamp", &ago(3)),
                    ("format", "json"),
                ],
                "unknown",
            )
        };
        let (status, body) = app
            .post_form("/2.0/", &pairs(&scrobble("other-client")))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, body) = app
            .post_form("/2.0/", &pairs(&scrobble("first-client")))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;
}

/// Failed logins are counted per username and IP, whether or not the
/// account exists; past the limit even the right password is refused.
/// Without account passwords, only scrobbler tokens log in.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn password_logins_are_limited_and_tokens_work_without_passwords() {
    with_app(CompatConfig::default(), |app| async move {
        let login = |username: &str, password: &str| {
            pano_signed(
                &[
                    ("method", "auth.getMobileSession"),
                    ("api_key", "panoScrobbler"),
                    ("username", username),
                    ("password", password),
                ],
                "panoScrobbler",
            )
        };
        let error = |body: &str| serde_json::from_str::<Value>(body).unwrap()["error"].clone();

        let (_, body) = app
            .post_form("/2.0/", &pairs(&login("nobody-here", PASSWORD)))
            .await;
        assert_eq!(error(&body), 4);
        for _ in 0..crate::limits::LOGIN_ATTEMPTS_PER_USER_AND_IP {
            let (_, body) = app
                .post_form("/2.0/", &pairs(&login(&app.username, "wrong")))
                .await;
            assert_eq!(error(&body), 4);
        }
        let (status, body) = app
            .post_form("/2.0/", &pairs(&login(&app.username, PASSWORD)))
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error(&body), 29);
    })
    .await;

    let tokens_only = CompatConfig {
        password_login: false,
        ..Default::default()
    };
    with_app(tokens_only, |app| async move {
        let token = app.new_token("Pano on my phone").await;
        let login = |password: &str| {
            pano_signed(
                &[
                    ("method", "auth.getMobileSession"),
                    ("api_key", "panoScrobbler"),
                    ("username", &app.username),
                    ("password", password),
                ],
                "panoScrobbler",
            )
        };
        let (_, body) = app.post_form("/2.0/", &pairs(&login(PASSWORD))).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 4);
        let (status, body) = app.post_form("/2.0/", &pairs(&login(&token))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The token is the session: revoking it signs the client out.
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["session"]["key"],
            token.as_str()
        );
    })
    .await;
}

/// mpdscribe and other Audioscrobbler 1.2 clients.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn audioscrobbler_handshake_now_playing_and_submissions() {
    with_app(CompatConfig::default(), |app| async move {
        let token = app.new_token("mpdscribe").await;
        let handshake = |t: i64, password: &str| {
            format!(
                "/?hs=true&p=1.2.1&c=mpd&v=0.24&u={}&t={t}&a={}",
                app.username,
                md5_hex(&format!("{}{t}", md5_hex(password)))
            )
        };
        let now = Utc::now().timestamp();

        let (_, _, body) = app.get(&handshake(now - 3600, &token)).await;
        assert_eq!(body, "BADTIME\n");
        let (_, _, body) = app.get(&handshake(now, "wrong")).await;
        assert_eq!(body, "BADAUTH\n");
        let (status, headers, body) = app.get(&handshake(now, &token)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "{body}");
        assert_eq!(lines[0], "OK");
        assert_eq!(lines[2], format!("{BASE_URL}/1.2/nowplaying"));
        assert_eq!(lines[3], format!("{BASE_URL}/1.2/submissions"));
        let sid = lines[1].to_string();
        // A captured handshake can't be replayed.
        let (_, _, body) = app.get(&handshake(now, &token)).await;
        assert_eq!(body, "BADAUTH\n");

        let (_, body) = app
            .post_form(
                "/1.2/nowplaying",
                &[
                    ("s", &sid),
                    ("a", "Low"),
                    ("t", "Lullaby"),
                    ("b", "I Could Live in Hope"),
                    ("l", "586"),
                    ("n", "2"),
                    ("m", ""),
                ],
            )
            .await;
        assert_eq!(body, "OK\n");

        let (i0, i1) = (ago(30), ago(20));
        let submission = [
            ("s", sid.as_str()),
            ("a[0]", "Low"),
            ("t[0]", "Words"),
            ("i[0]", i0.as_str()),
            ("o[0]", "P"),
            ("r[0]", ""),
            ("l[0]", "362"),
            ("b[0]", "I Could Live in Hope"),
            ("n[0]", "1"),
            ("m[0]", ""),
            ("a[1]", "Low"),
            ("t[1]", "Lullaby"),
            ("i[1]", i1.as_str()),
            ("o[1]", "P"),
            ("r[1]", "L"),
            ("l[1]", "586"),
            ("b[1]", "I Could Live in Hope"),
            ("n[1]", "2"),
            ("m[1]", ""),
        ];
        for _ in 0..2 {
            let (_, body) = app.post_form("/1.2/submissions", &submission).await;
            assert_eq!(body, "OK\n");
        }
        let stored = app.scrobbles().await;
        assert_eq!(stored.len(), 2);
        assert!(stored.iter().all(|(_, source, client, verified)| {
            source == "audioscrobbler"
                && client.as_deref() == Some("mpd 0.24")
                && *verified == Some(false)
        }));
        // The play rated loved (`L`) loves its track.
        assert_eq!(
            loved(&app).await,
            [(
                "Lullaby".to_string(),
                "audioscrobbler".to_string(),
                Some("mpd 0.24".to_string()),
                Some(false)
            )]
        );

        let (_, body) = app.post_form("/1.2/submissions", &[("s", "nope")]).await;
        assert_eq!(body, "BADSESSION\n");
        let (_, listed) = app
            .api(Method::GET, "/v1/scrobbler/credentials", None)
            .await;
        let id = listed[0]["id"].as_str().unwrap().to_string();
        app.api(
            Method::DELETE,
            &format!("/v1/scrobbler/credentials/{id}"),
            None,
        )
        .await;
        let (_, body) = app.post_form("/1.2/submissions", &submission).await;
        assert_eq!(body, "BADSESSION\n");
    })
    .await;
}

/// Web Scrobbler's and Pano Scrobbler's ListenBrainz scrobblers, pointed
/// at this server.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn listenbrainz_clients_validate_and_submit() {
    with_app(CompatConfig::default(), |app| async move {
        let token = app.new_token("Web Scrobbler").await;
        for (auth, valid) in [
            (format!("Token {token}"), true),
            (format!("token {token}"), true),
            ("Token nope".to_string(), false),
        ] {
            let req = Request::builder()
                .uri("/1/validate-token")
                .header(header::AUTHORIZATION, auth)
                .body(Body::empty())
                .unwrap();
            let (status, _, body) = app.send(req).await;
            assert_eq!(status, StatusCode::OK);
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["valid"], valid);
        }
        let (_, _, body) = app.get(&format!("/1/validate-token?token={token}")).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["user_name"], app.username.as_str());
        let (status, _, _) = app.get("/1/validate-token").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let web_scrobbler_meta = json!({
            "artist_name": "Cocteau Twins",
            "track_name": "Heaven or Las Vegas",
            "release_name": "Heaven or Las Vegas",
            "additional_info": {
                "submission_client": "Web Scrobbler",
                "submission_client_version": "3.14.0",
                "music_service_name": "YouTube",
                "origin_url": "https://www.youtube.com/watch?v=x",
                "duration": 296,
            },
        });
        let auth = format!("Token {token}");
        let (status, body) = app
            .listen_brainz(&auth, json!({ "listen_type": "playing_now", "payload": [{ "track_metadata": web_scrobbler_meta }] }))
            .await;
        assert_eq!((status, &body["status"]), (StatusCode::OK, &json!("ok")));
        let listened_at = Utc::now().timestamp() - 300;
        let (status, _) = app
            .listen_brainz(&auth, json!({ "listen_type": "single", "payload": [{ "listened_at": listened_at, "track_metadata": web_scrobbler_meta }] }))
            .await;
        assert_eq!(status, StatusCode::OK);

        // Pano's cached scrobbles as an import, one of them too old.
        let pano = |track: &str, at: i64| {
            json!({
                "listened_at": at,
                "track_metadata": {
                    "artist_name": "Cocteau Twins",
                    "release_name": null,
                    "track_name": track,
                    "additional_info": {
                        "duration_ms": 214000,
                        "submission_client": "Pano Scrobbler",
                        "submission_client_version": "3.12",
                    },
                },
            })
        };
        let import = json!({
            "listen_type": "import",
            "payload": [
                pano("Iceblink Luck", listened_at - 900),
                pano("Fifty-Fifty Clown", listened_at - 600),
                pano("Cherry-Coloured Funk", listened_at - 30 * 86_400),
            ],
        });
        for _ in 0..2 {
            let (status, body) = app.listen_brainz(&format!("token {token}"), import.clone()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let stored = app.scrobbles().await;
        assert_eq!(
            stored.iter().map(|s| (s.0.as_str(), s.2.as_deref())).collect::<Vec<_>>(),
            [
                ("Iceblink Luck", Some("Pano Scrobbler 3.12")),
                ("Fifty-Fifty Clown", Some("Pano Scrobbler 3.12")),
                ("Heaven or Las Vegas", Some("Web Scrobbler 3.14.0")),
            ]
        );
        let length: Option<i32> = sqlx::query_scalar(
            "SELECT s.duration_ms FROM scrobbles s JOIN tracks t ON t.id = s.track_id WHERE t.title = 'Heaven or Las Vegas'",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(length, Some(296_000));

        let (status, _) = app.listen_brainz("Token nope", import.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let two_singles = json!({ "listen_type": "single", "payload": [pano("A", listened_at), pano("B", listened_at)] });
        let (status, _) = app.listen_brainz(&auth, two_singles).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn the_daily_limit_holds_across_protocols() {
    let config = CompatConfig {
        daily_limit: 3,
        ..Default::default()
    };
    with_app(config, |app| async move {
        let token = app.new_token("scripts").await;
        let mut params: Vec<(String, String)> = vec![
            ("method".into(), "track.scrobble".into()),
            ("api_key".into(), "k".into()),
            ("sk".into(), token.clone()),
            ("format".into(), "json".into()),
        ];
        for i in 0..5 {
            params.push((format!("artist[{i}]"), "Artist".into()));
            params.push((format!("track[{i}]"), format!("Song {i}")));
            params.push((format!("timestamp[{i}]"), ago(60 - 5 * i)));
        }
        let (status, body) = app.post_form("/2.0/", &pairs(&params)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(result["scrobbles"]["@attr"]["accepted"], 3);
        assert_eq!(result["scrobbles"]["scrobble"][4]["ignoredMessage"]["code"], "5");

        let listen = json!({
            "listen_type": "single",
            "payload": [{ "listened_at": Utc::now().timestamp() - 60, "track_metadata": { "artist_name": "Artist", "track_name": "More" } }],
        });
        let (status, _) = app.listen_brainz(&format!("Token {token}"), listen).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(app.scrobbles().await.len(), 3);
    })
    .await;
}

/// The web flow (`/api/auth/?api_key=…&cb=…`), and credential management
/// refused to anything but a session.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn web_flow_callbacks_and_who_may_manage_credentials() {
    with_app(CompatConfig::default(), |app| async move {
        let (status, _) = app
            .api(
                Method::POST,
                "/v1/scrobbler/authorizations",
                Some(json!({ "api_key": "web-app", "callback": "javascript:alert(1)" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, redirect) = app
            .api(
                Method::POST,
                "/v1/scrobbler/authorizations",
                Some(json!({ "api_key": "web-app", "callback": "https://client.example/done?state=1" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let token = redirect["token"].as_str().unwrap();
        assert_eq!(
            redirect["redirect_url"],
            format!("https://client.example/done?state=1&token={token}").as_str()
        );
        let session = documented_signed(
            &[("method", "auth.getSession"), ("api_key", "web-app"), ("token", token), ("format", "json")],
            "unknown",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&session)).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // A scrobble-scoped API token reaches the native API, but may not
        // manage scrobbler credentials.
        let raw = "f".repeat(64);
        sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash, scopes) SELECT id, 'app', $1, '{scrobble}' FROM users")
            .bind(auth_db::hash_api_token(&raw))
            .execute(&app.pool)
            .await
            .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/scrobbler/tokens")
            .header(header::AUTHORIZATION, format!("Bearer {raw}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "name": "x" }).to_string()))
            .unwrap();
        let (status, _, _) = app.send(req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Scrobbler credentials, in turn, never reach the native API.
        let token = app.new_token("player").await;
        let req = Request::builder()
            .uri("/v1/user/me")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = app.send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    })
    .await;
}

/// A player logging in again with the account password doesn't restore an
/// account waiting to be erased: only the native login does.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn password_logins_never_restore_a_deleted_account() {
    with_app(CompatConfig::default(), |app| async move {
        let (status, _) = app
            .api(
                Method::POST,
                "/v1/user/me/delete",
                Some(serde_json::json!({ "password": PASSWORD, "confirm": app.username })),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let login = pano_signed(
            &[
                ("method", "auth.getMobileSession"),
                ("api_key", "panoScrobbler"),
                ("username", &app.username),
                ("password", PASSWORD),
            ],
            "panoScrobbler",
        );
        let (_, body) = app.post_form("/2.0/", &pairs(&login)).await;
        assert!(body.contains("Invalid username or password"), "{body}");
        let pending: bool =
            sqlx::query_scalar("SELECT delete_after IS NOT NULL FROM users WHERE id = $1")
                .bind(app.user_id)
                .fetch_one(&app.pool)
                .await
                .unwrap();
        assert!(pending);
    })
    .await;
}

async fn loved(app: &TestApp) -> Vec<(String, String, Option<String>, Option<bool>)> {
    sqlx::query_as(
        r#"
        SELECT t.title, l.source, c.name, c.verified
        FROM loved_tracks l
        JOIN tracks t ON t.id = l.track_id
        LEFT JOIN scrobble_clients c ON c.id = l.client_id
        ORDER BY t.title
        "#,
    )
    .fetch_all(&app.pool)
    .await
    .unwrap()
}

/// Pano Scrobbler's GNU FM mode: `track.love` and `track.unlove` as signed
/// form posts, then its loved-tracks and recent-tracks screens, read with
/// `extended=1` and the session key, unsigned.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn pano_scrobbler_loves_unloves_and_reads_loved_tracks() {
    with_app(CompatConfig::default(), |app| async move {
        let sk = app.new_token("Pano Scrobbler").await;
        let love = |method: &'static str, artist: &'static str, track: &'static str| {
            pano_signed(
                &[
                    ("method", method),
                    ("artist", artist),
                    ("track", track),
                    ("sk", &sk),
                    ("api_key", "panoScrobbler"),
                    ("format", "json"),
                ],
                "panoScrobbler",
            )
        };
        let scrobble = pano_signed(
            &[
                ("method", "track.scrobble"),
                ("artist", "Slowdive"),
                ("track", "Alison"),
                ("timestamp", &ago(10)),
                ("sk", &sk),
                ("api_key", "panoScrobbler"),
                ("format", "json"),
            ],
            "panoScrobbler",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&scrobble)).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Loving twice is no error; a track never scrobbled is created.
        for (artist, track) in [
            ("Slowdive", "Alison"),
            ("slowdive", "ALISON "),
            ("Slowdive", "Machine Gun"),
        ] {
            let (status, body) = app
                .post_form("/2.0/", &pairs(&love("track.love", artist, track)))
                .await;
            assert_eq!((status, body.as_str()), (StatusCode::OK, "{}"));
        }
        assert_eq!(
            loved(&app).await,
            [
                (
                    "Alison".to_string(),
                    "lastfm".to_string(),
                    Some("panoScrobbler".to_string()),
                    Some(true)
                ),
                (
                    "Machine Gun".to_string(),
                    "lastfm".to_string(),
                    Some("panoScrobbler".to_string()),
                    Some(true)
                ),
            ]
        );

        let read = |method: &str, extra: &[(&str, &str)]| {
            let mut params = vec![
                ("method", method.to_string()),
                ("user", app.username.clone()),
                ("limit", "50".into()),
                ("page", "1".into()),
                ("extended", "1".into()),
                ("format", "json".into()),
                ("api_key", "panoScrobbler".into()),
                ("sk", sk.clone()),
            ];
            params.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
            let owned: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
            format!("/2.0/?{}", form(&owned))
        };
        let (status, _, body) = app.get(&read("user.getLovedTracks", &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        let tracks = page["lovedtracks"]["track"].as_array().unwrap();
        assert_eq!(page["lovedtracks"]["@attr"]["total"], "2");
        assert_eq!(page["lovedtracks"]["@attr"]["totalPages"], "1");
        // Newest love first: Machine Gun, then Alison.
        assert_eq!(tracks[0]["name"], "Machine Gun");
        assert_eq!(tracks[1]["name"], "Alison");
        assert_eq!(tracks[1]["artist"]["name"], "Slowdive");
        assert!(
            tracks[1]["date"]["uts"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .is_ok()
        );
        assert_eq!(tracks[1]["image"].as_array().unwrap().len(), 4);

        let (status, _, body) = app.get(&read("user.getRecentTracks", &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let recent: Value = serde_json::from_str(&body).unwrap();
        let first = &recent["recenttracks"]["track"][0];
        assert_eq!(
            (&first["name"], &first["artist"]["name"], &first["loved"]),
            (&json!("Alison"), &json!("Slowdive"), &json!("1"))
        );

        let (status, body) = app
            .post_form("/2.0/", &pairs(&love("track.unlove", "Slowdive", "Alison")))
            .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "{}"));
        // Unloving something never loved, or unknown, changes nothing and
        // creates nothing.
        let (status, _) = app
            .post_form(
                "/2.0/",
                &pairs(&love("track.unlove", "Nobody", "Never Heard")),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let unknown: i64 =
            sqlx::query_scalar("SELECT count(*) FROM tracks WHERE title = 'Never Heard'")
                .fetch_one(&app.pool)
                .await
                .unwrap();
        assert_eq!(unknown, 0);
        let (_, _, body) = app.get(&read("user.getLovedTracks", &[])).await;
        let page: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["lovedtracks"]["@attr"]["total"], "1");
        let (_, _, body) = app.get(&read("user.getRecentTracks", &[])).await;
        let recent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(recent["recenttracks"]["track"][0]["loved"], "0");

        // Errors as Last.fm codes them.
        let (status, body) = app
            .post_form("/2.0/", &pairs(&love("track.love", "Slowdive", "")))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 6);
        let wrong_sk = pano_signed(
            &[
                ("method", "track.love"),
                ("artist", "Slowdive"),
                ("track", "Alison"),
                ("sk", "not-a-session"),
                ("api_key", "panoScrobbler"),
                ("format", "json"),
            ],
            "panoScrobbler",
        );
        let (status, body) = app.post_form("/2.0/", &pairs(&wrong_sk)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 9);
        let love_get = form(&pairs(&love("track.love", "Slowdive", "Alison")));
        let (status, _, body) = app.get(&format!("/2.0/?{love_get}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    })
    .await;
}

/// Web Scrobbler's Last.fm request shape (a POST carrying its parameters,
/// signature and `format=json` in the query string) and pylast's (the
/// documented signature in a form body, answered in XML), with keys whose
/// secrets the server knows; the profile rules, the love limit and the cap.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn web_scrobbler_and_pylast_love_with_checked_signatures() {
    let (ws_key, ws_secret) = (
        "d9bb1870d3269646f740544d9def2c95",
        "2160733a567d4a1a69a73fad54c564b2",
    );
    let config = CompatConfig {
        api_keys: [
            (ws_key.to_string(), ws_secret.to_string()),
            ("pylast-key".to_string(), "pylast-secret".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    with_app(config, |app| async move {
        let sk = app.new_token("scripts").await;
        let web_scrobbler = |method: &str, secret: &str| {
            let signed = documented_signed(
                &[
                    ("method", method),
                    ("track", "Teardrop"),
                    ("artist", "Massive Attack"),
                    ("sk", &sk),
                    ("api_key", ws_key),
                    ("format", "json"),
                ],
                secret,
            );
            Request::builder()
                .method(Method::POST)
                .uri(format!("/2.0/?{}", form(&pairs(&signed))))
                .body(Body::empty())
                .unwrap()
        };
        let (status, _, body) = app.send(web_scrobbler("track.love", ws_secret)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "{}"));
        let (status, _, body) = app.send(web_scrobbler("track.love", "wrong")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], 13);
        assert_eq!(
            loved(&app).await,
            [(
                "Teardrop".to_string(),
                "lastfm".to_string(),
                Some(ws_key.to_string()),
                Some(true)
            )]
        );
        let (status, _, _) = app.send(web_scrobbler("track.unlove", ws_secret)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(loved(&app).await.is_empty());

        let pylast = |method: &str, track: &str| {
            documented_signed(
                &[
                    ("method", method),
                    ("artist", "Portishead"),
                    ("track", track),
                    ("api_key", "pylast-key"),
                    ("sk", &sk),
                ],
                "pylast-secret",
            )
        };
        let req = Request::builder()
            .method(Method::POST)
            .uri("/2.0/")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form(&pairs(&pylast("track.love", "Roads")))))
            .unwrap();
        let (status, headers, body) = app.send(req).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/xml")
        );
        assert!(body.contains("<lfm status=\"ok\"></lfm>"), "{body}");

        // Read in XML, unsigned and signed out, as pylast's
        // `User.get_loved_tracks` does.
        let path = format!(
            "/2.0/?method=user.getLovedTracks&user={}&api_key=pylast-key",
            app.username
        );
        let (status, _, body) = app.get(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("total=\"1\""), "{body}");
        assert!(body.contains("<name>Roads</name>"), "{body}");
        assert!(body.contains("<artist><name>Portishead</name>"), "{body}");

        // A private profile's loves are its owner's alone; a leaving
        // account has none to show.
        sqlx::query("UPDATE users SET is_private = TRUE")
            .execute(&app.pool)
            .await
            .unwrap();
        let (status, _, body) = app.get(&path).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("<error code=\"17\">"), "{body}");
        let (status, _, _) = app.get(&format!("{path}&sk={sk}")).await;
        assert_eq!(status, StatusCode::OK);
        sqlx::query(
            "UPDATE users SET is_private = FALSE, delete_after = NOW() + INTERVAL '7 days'",
        )
        .execute(&app.pool)
        .await
        .unwrap();
        let (status, _, body) = app.get(&path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("<error code=\"6\">User not found</error>"),
            "{body}"
        );
        sqlx::query("UPDATE users SET delete_after = NULL")
            .execute(&app.pool)
            .await
            .unwrap();

        // The cap and the hourly love limit, shared with the native API.
        sqlx::query(
            "WITH artist AS (INSERT INTO artists (name, name_normalized)
                             VALUES ('Many', 'many') RETURNING id),
                  made AS (INSERT INTO tracks (artist_id, title, title_normalized)
                           SELECT artist.id, 'T' || g, 't' || g
                           FROM artist, generate_series(1, $2 - 1) g RETURNING id)
             INSERT INTO loved_tracks (user_id, track_id, source)
             SELECT $1, id, 'scrobblr' FROM made",
        )
        .bind(app.user_id)
        .bind(db::queries::loves::MAX_LOVED_TRACKS)
        .execute(&app.pool)
        .await
        .unwrap();
        let (status, body) = app
            .post_form("/2.0/", &pairs(&pylast("track.love", "Glory Box")))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("<error code=\"6\">") && body.contains("50000"),
            "{body}"
        );
        let window = Utc::now().timestamp() / 3600;
        let _: () = fred::interfaces::KeysInterface::set(
            &app.redis,
            format!("love:user:{}:{window}", app.user_id),
            crate::limits::LOVES_PER_USER,
            None,
            None,
            false,
        )
        .await
        .unwrap();
        let (status, body) = app
            .post_form("/2.0/", &pairs(&pylast("track.unlove", "Roads")))
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("<error code=\"29\">"), "{body}");
    })
    .await;
}
