//! Uploads through the router: what is stored, what the API answers and
//! serves, and what gets deleted. `#[ignore]`d: they need Postgres and Redis
//! (`just test-db`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};

use crate::compat::CompatConfig;
use crate::test_app::{BASE_URL, TestApp, UPLOADS_URL, with_app};
use db::queries::{auth as auth_db, tracks as tracks_db};

fn png() -> Vec<u8> {
    let image = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8, y as u8, 99]));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn multipart(path: &str, bearer: &str, file: &[u8]) -> Request<Body> {
    let boundary = "scrobblr-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"../../me.png\"\r\n\
         Content-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend(file);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

impl TestApp {
    async fn upload(&self, path: &str, file: &[u8]) -> (StatusCode, Value) {
        self.upload_as(&self.session.to_string(), path, file).await
    }

    async fn upload_as(&self, bearer: &str, path: &str, file: &[u8]) -> (StatusCode, Value) {
        let (status, _, body) = self.send(multipart(path, bearer, file)).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn stored_avatar(&self) -> Option<String> {
        sqlx::query_scalar("SELECT image_url FROM users WHERE id = $1")
            .bind(self.user_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Every file under the upload root, as paths relative to it.
    fn stored_files(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push(path.strip_prefix(root).unwrap().display().to_string());
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.uploads, &self.uploads, &mut files);
        files.sort();
        files
    }
}

/// The key a served URL points at.
fn key_of(url: &Value) -> String {
    url.as_str()
        .unwrap()
        .strip_prefix(&format!("{UPLOADS_URL}/"))
        .unwrap_or_else(|| panic!("{url} is not an upload URL"))
        .to_owned()
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn avatars_are_stored_by_key_and_deleted_once_replaced() {
    with_app(CompatConfig::default(), |app| async move {
        let (status, profile) = app.upload("/v1/user/me/avatar", &png()).await;
        assert_eq!(status, StatusCode::OK, "{profile}");
        let first = key_of(&profile["image_url"]);
        assert!(first.starts_with("avatars/"), "{first}");
        assert_eq!(app.stored_avatar().await.as_deref(), Some(first.as_str()));
        assert_eq!(app.stored_files(), [first.as_str()]);
        let path = app.uploads.join(&first);
        assert!(
            std::fs::read(&path)
                .unwrap()
                .starts_with(&[0xff, 0xd8, 0xff])
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        let (status, me) = app.api(Method::GET, "/v1/user/me", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["image_url"], profile["image_url"]);

        // Clients save the whole form, the avatar URL they were given too.
        let (status, me) = app
            .api(
                Method::PATCH,
                "/v1/user/me",
                Some(json!({ "bio": "hi", "image_url": profile["image_url"] })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{me}");
        assert_eq!(me["image_url"], profile["image_url"]);
        assert_eq!(app.stored_avatar().await.as_deref(), Some(first.as_str()));
        assert_eq!(app.stored_files(), [first.as_str()]);
        // Not someone else's upload, nor our own under the API's route.
        for other in [
            format!("{UPLOADS_URL}/avatars/00/00000000000000000000000000000000.jpg"),
            format!("{BASE_URL}/uploads/{first}"),
        ] {
            let (status, _) = app
                .api(
                    Method::PATCH,
                    "/v1/user/me",
                    Some(json!({ "image_url": other })),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{other}");
        }
        assert_eq!(app.stored_avatar().await.as_deref(), Some(first.as_str()));

        let (status, profile) = app.upload("/v1/user/me/avatar", &png()).await;
        assert_eq!(status, StatusCode::OK);
        let second = key_of(&profile["image_url"]);
        assert_ne!(first, second);
        assert_eq!(app.stored_files(), [second.as_str()]);

        let external = "https://example.com/me.jpg";
        let (status, profile) = app
            .api(
                Method::PATCH,
                "/v1/user/me",
                Some(json!({ "image_url": external })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(profile["image_url"], external);
        assert!(app.stored_files().is_empty());

        let (_, profile) = app.upload("/v1/user/me/avatar", &png()).await;
        key_of(&profile["image_url"]);
        let (status, profile) = app
            .api(
                Method::PATCH,
                "/v1/user/me",
                Some(json!({ "image_url": "" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(profile["image_url"], Value::Null);
        assert!(app.stored_files().is_empty());

        // A PATCH that leaves the avatar alone keeps its file.
        let (_, profile) = app.upload("/v1/user/me/avatar", &png()).await;
        let kept = key_of(&profile["image_url"]);
        let (status, _) = app
            .api(Method::PATCH, "/v1/user/me", Some(json!({ "bio": "hi" })))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(app.stored_files(), [kept]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn avatars_from_before_kinds_are_never_deleted() {
    with_app(CompatConfig::default(), |app| async move {
        let legacy = "0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg";
        std::fs::write(app.uploads.join(legacy), b"jpeg").unwrap();
        sqlx::query("UPDATE users SET image_url = $2 WHERE id = $1")
            .bind(app.user_id)
            .bind(legacy)
            .execute(&app.pool)
            .await
            .unwrap();
        let (_, me) = app.api(Method::GET, "/v1/user/me", None).await;
        assert_eq!(me["image_url"], format!("{UPLOADS_URL}/{legacy}"));

        let (status, _) = app.upload("/v1/user/me/avatar", &png()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(app.uploads.join(legacy).exists());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn refused_uploads_store_nothing() {
    with_app(CompatConfig::default(), |app| async move {
        for file in [
            &b"not an image"[..],
            b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
            b"",
        ] {
            let (status, body) = app.upload("/v1/user/me/avatar", file).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        let (status, body) = app
            .upload(
                "/v1/user/me/avatar",
                &vec![0; crate::handlers::uploads::MAX_UPLOAD_BYTES + 1],
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
        let (status, _) = app
            .api(Method::POST, "/v1/user/me/avatar", Some(json!({})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The 400s say nothing about where files are kept.
        let (_, body) = app.upload("/v1/user/me/avatar", b"junk").await;
        assert!(!body.to_string().contains(&*app.uploads.to_string_lossy()));

        assert!(app.stored_files().is_empty());
        assert_eq!(app.stored_avatar().await, None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn artwork_is_stored_under_its_kind_and_served_once_promoted() {
    with_app(CompatConfig::default(), |app| async move {
        let artist = tracks_db::find_or_create_artist(&app.pool, "Artist")
            .await
            .unwrap();
        let album = tracks_db::find_or_create_album(&app.pool, artist.id, "Album")
            .await
            .unwrap();

        let (status, candidates) = app
            .upload(&format!("/v1/album/{album}/image"), &png())
            .await;
        assert_eq!(status, StatusCode::OK, "{candidates}");
        assert!(key_of(&candidates[0]["url"]).starts_with("albums/"));

        let (status, candidates) = app
            .upload(&format!("/v1/artist/{}/image", artist.id), &png())
            .await;
        assert_eq!(status, StatusCode::OK, "{candidates}");
        let key = key_of(&candidates[0]["url"]);
        assert!(key.starts_with("artists/"), "{key}");
        let stored: String = sqlx::query_scalar("SELECT url FROM image_candidates WHERE id = $1")
            .bind(candidates[0]["id"].as_i64().unwrap())
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(stored, key);

        // Two more likes promote it to the artist's image.
        let candidate = candidates[0]["id"].as_i64().unwrap();
        for name in ["voter1", "voter2"] {
            let voter: i64 = sqlx::query_scalar(
                "INSERT INTO users (username, email, password_hash) \
                 VALUES ($1 || $2, $1 || $2 || '@test', 'x') RETURNING id",
            )
            .bind(&app.username)
            .bind(name)
            .fetch_one(&app.pool)
            .await
            .unwrap();
            let session = auth_db::create_session(&app.pool, voter, None, None)
                .await
                .unwrap()
                .id;
            let (status, _) = app
                .api_as(
                    &session.to_string(),
                    Method::POST,
                    &format!("/v1/image/{candidate}/vote"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, shown) = app
            .api(Method::GET, &format!("/v1/artist/{}", artist.id), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(shown["image_url"], format!("{UPLOADS_URL}/{key}"));
        let (_, candidates) = app
            .api(
                Method::GET,
                &format!("/v1/artist/{}/images", artist.id),
                None,
            )
            .await;
        assert_eq!(candidates[0]["is_default"], true);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn the_uploads_route_serves_keys_only() {
    with_app(CompatConfig::default(), |app| async move {
        let (_, profile) = app.upload("/v1/user/me/avatar", &png()).await;
        let key = key_of(&profile["image_url"]);

        let (status, headers, _) = app.get(&format!("/uploads/{key}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        std::fs::write(app.uploads.join(".tmp/x.jpg"), b"partial").unwrap();
        let directory = key.rsplit_once('/').unwrap().0;
        for path in [
            "/uploads/.tmp/x.jpg",
            &format!("/uploads/{directory}"),
            &format!("/uploads/{directory}/"),
            "/uploads/",
            "/uploads/%2e%2e/Cargo.toml",
            "/uploads/../Cargo.toml",
            "/uploads/avatars/00/missing.jpg",
        ] {
            let (status, headers, _) = app.get(path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert!(headers.get(header::CACHE_CONTROL).is_none(), "{path}");
        }
    })
    .await;
}
