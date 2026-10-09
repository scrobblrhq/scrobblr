use aide::axum::routing::patch_with;
use aide::transform::{TransformOpenApi, TransformOperation};
use aide::util::iter_operations_mut;
use aide::{
    axum::{
        ApiRouter, IntoApiResponse,
        routing::{delete_with, get_with, post_with},
    },
    openapi::{OpenApi, ReferenceOr, SecurityRequirement, StatusCode},
    scalar::Scalar,
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderValue, StatusCode as HttpStatus, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use shared::media::UploadKey;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    catch_panic::CatchPanicLayer, compression::CompressionLayer, services::ServeDir,
    set_header::SetResponseHeaderLayer, trace::TraceLayer,
};

use crate::{
    compat,
    errors::ErrorJson,
    handlers::{auth, community, connected_accounts, imports, scrobbles, tracks, uploads, users},
    limits,
    middleware::{
        app_signature::require_app_signature,
        auth::{Access, Scope, optional_auth, require_auth},
        cors,
        rate_limit::{rate_limit, upload_limit},
    },
    state::AppState,
};

#[cfg(test)]
mod openapi_tests;
#[cfg(test)]
mod tests;

/// The upload directory, for when no static server is in front (not part of
/// the OpenAPI surface). Keys are never reused, so a file never changes. Any
/// page may read the pixels, as from the uploads host.
fn uploads_service(root: &Path) -> Router {
    let cache = |response: &Response<_>| {
        response
            .status()
            .is_success()
            .then(|| HeaderValue::from_static("public, max-age=31536000, immutable"))
    };
    Router::new().fallback_service(
        ServiceBuilder::new()
            .layer(middleware::from_fn(upload_keys_only))
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                cache,
            ))
            .layer(SetResponseHeaderLayer::overriding(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            ))
            .service(ServeDir::new(root)),
    )
}

/// Nothing under the root but uploads: no directories, no `.tmp/`.
async fn upload_keys_only(request: Request, next: Next) -> Response {
    match UploadKey::parse(request.uri().path().trim_start_matches('/')) {
        Some(_) => next.run(request).await,
        None => HttpStatus::NOT_FOUND.into_response(),
    }
}

async fn serve_api(Extension(api): Extension<Arc<OpenApi>>) -> impl IntoApiResponse {
    Json(api)
}
pub fn build(state: AppState) -> Router {
    // Authenticated routes, grouped by what they need of the credential
    // (`Access`, checked by `require_auth`): a session may do anything, an
    // API token only what its scopes cover, and only a session may manage
    // credentials. A new route goes in the group that fits it, and in the
    // list in `router/tests.rs`, which fails until it's there.

    // Scrobbling: all a player's `scrobble` token needs.
    let scrobble_routes = ApiRouter::new()
        .api_route(
            "/v1/scrobble",
            post_with(scrobbles::scrobble, scrobbles::_scrobble_doc),
        )
        .api_route(
            "/v1/now-playing",
            post_with(
                scrobbles::update_now_playing,
                scrobbles::_update_now_playing_doc,
            ),
        );

    // Reading the account's own data.
    let read_routes = ApiRouter::new()
        .api_route(
            "/v1/connect",
            get_with(
                connected_accounts::list_connected_accounts,
                connected_accounts::_list_connected_accounts_doc,
            ),
        )
        .api_route(
            "/v1/imports",
            get_with(imports::list_imports, imports::_list_imports_doc),
        )
        .api_route(
            "/v1/imports/{id}",
            get_with(imports::get_import, imports::_get_import_doc),
        )
        .api_route(
            "/v1/user/me",
            get_with(users::get_own_profile, users::_get_own_profile_doc),
        );

    // Changing the account and posting as it.
    let write_routes = ApiRouter::new()
        // Connected accounts (Spotify, Last.fm)
        .api_route(
            "/v1/connect/{provider}",
            delete_with(
                connected_accounts::disconnect,
                connected_accounts::_disconnect_doc,
            ),
        )
        // History imports (Last.fm)
        .api_route(
            "/v1/import/lastfm",
            post_with(
                imports::start_lastfm_import,
                imports::_start_lastfm_import_doc,
            ),
        )
        .api_route(
            "/v1/imports/{id}",
            delete_with(imports::cancel_import, imports::_cancel_import_doc),
        )
        // User profile
        .api_route(
            "/v1/user/me",
            patch_with(users::update_settings, users::_update_settings_doc),
        )
        // Social
        .api_route(
            "/v1/user/{username}/follow",
            post_with(users::follow, users::_follow_doc),
        )
        .api_route(
            "/v1/user/{username}/follow",
            delete_with(users::unfollow, users::_unfollow_doc),
        )
        // Catalog metadata refresh
        .api_route(
            "/v1/track/{id}/refresh",
            post_with(tracks::refresh_track, tracks::_refresh_track_doc),
        )
        .api_route(
            "/v1/artist/{id}/refresh",
            post_with(tracks::refresh_artist, tracks::_refresh_artist_doc),
        )
        .api_route(
            "/v1/album/{id}/refresh",
            post_with(tracks::refresh_album, tracks::_refresh_album_doc),
        )
        // Community: image votes
        .api_route(
            "/v1/image/{id}/vote",
            post_with(community::vote_image, community::_vote_image_doc),
        )
        .api_route(
            "/v1/image/{id}/vote",
            delete_with(community::unvote_image, community::_unvote_image_doc),
        )
        // Community: posting comments
        .api_route(
            "/v1/artist/{id}/comments",
            post_with(
                community::add_artist_comment,
                community::_add_artist_comment_doc,
            ),
        )
        .api_route(
            "/v1/track/{id}/comments",
            post_with(
                community::add_track_comment,
                community::_add_track_comment_doc,
            ),
        )
        .api_route(
            "/v1/comments/{id}",
            delete_with(community::delete_comment, community::_delete_comment_doc),
        );

    // Image uploads: `write` like the routes above, but with a larger body
    // limit than the default 2 MiB (axum caps multipart at DefaultBodyLimit)
    // and limits of their own.
    let upload_routes = ApiRouter::new()
        .api_route(
            "/v1/user/me/avatar",
            post_with(uploads::upload_avatar, uploads::_upload_avatar_doc),
        )
        .api_route(
            "/v1/artist/{id}/image",
            post_with(
                uploads::upload_artist_image,
                uploads::_upload_artist_image_doc,
            ),
        )
        .api_route(
            "/v1/album/{id}/image",
            post_with(
                uploads::upload_album_image,
                uploads::_upload_album_image_doc,
            ),
        )
        .with_path_items(|mut item| {
            for (_, op) in iter_operations_mut(item.inner_mut()) {
                let _ = TransformOperation::new(op).response_with::<429, ErrorJson, _>(|r| {
                    r.description(&format!(
                        "Over {} uploads an hour for this account or {} from this address, \
                         or over 60 requests a minute from this address",
                        limits::UPLOADS_PER_USER,
                        limits::UPLOADS_PER_IP
                    ))
                });
            }
            item
        })
        .layer(middleware::from_fn_with_state(state.clone(), upload_limit))
        .layer(DefaultBodyLimit::max(uploads::MAX_UPLOAD_BYTES));

    // Credentials and account links: sessions only, so that a leaked API
    // token can't mint a credential (or link an account that scrobbles)
    // that outlives its revocation, nor revoke the user's others.
    let session_routes = ApiRouter::new()
        // Connected accounts (Spotify, Last.fm)
        .api_route(
            "/v1/connect/{provider}",
            get_with(
                connected_accounts::connect_provider,
                connected_accounts::_connect_provider_doc,
            ),
        )
        // API tokens
        .api_route(
            "/v1/auth/tokens",
            post_with(auth::create_api_token, auth::_create_api_token_doc),
        )
        .api_route(
            "/v1/auth/tokens/{id}",
            delete_with(auth::delete_api_token, auth::_delete_api_token_doc),
        )
        // Logout
        .api_route(
            "/v1/auth/logout",
            post_with(auth::logout, auth::_logout_doc),
        )
        // Third-party scrobblers' credentials and browser authorization
        .api_route(
            "/v1/scrobbler/credentials",
            get_with(
                compat::credentials::list_credentials,
                compat::credentials::_list_credentials_doc,
            ),
        )
        .api_route(
            "/v1/scrobbler/credentials/{id}",
            delete_with(
                compat::credentials::delete_credential,
                compat::credentials::_delete_credential_doc,
            ),
        )
        .api_route(
            "/v1/scrobbler/tokens",
            post_with(
                compat::credentials::create_token,
                compat::credentials::_create_token_doc,
            ),
        )
        .api_route(
            "/v1/scrobbler/authorizations",
            post_with(
                compat::credentials::authorize_callback,
                compat::credentials::_authorize_callback_doc,
            ),
        )
        .api_route(
            "/v1/scrobbler/authorizations/{token}",
            get_with(
                compat::credentials::get_authorization,
                compat::credentials::_get_authorization_doc,
            ),
        )
        .api_route(
            "/v1/scrobbler/authorizations/{token}/approve",
            post_with(
                compat::credentials::approve_authorization,
                compat::credentials::_approve_authorization_doc,
            ),
        );

    // Any credential: an API token lists only itself here, which is how a
    // client (the browser extension) checks the token it was given.
    let any_credential_routes = ApiRouter::new().api_route(
        "/v1/auth/tokens",
        get_with(auth::list_api_tokens, auth::_list_api_tokens_doc),
    );

    // Credential endpoints, gated on a first-party app signature whenever
    // AUTH_APP_KEYS is configured. Kept in their own group so the layer
    // applies to exactly these two routes and nothing else.
    let auth_public = ApiRouter::new()
        .api_route(
            "/v1/auth/register",
            post_with(auth::register, auth::_register_doc),
        )
        .api_route("/v1/auth/login", post_with(auth::login, auth::_login_doc))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_app_signature,
        ));

    // Public routes
    let public = ApiRouter::new()
        // Connected accounts: the provider redirects here with no Scrobblr
        // session, so these callbacks must be public (see handler doc comments).
        .api_route(
            "/v1/connect/spotify/callback",
            get_with(
                connected_accounts::spotify_callback,
                connected_accounts::_spotify_callback_doc,
            ),
        )
        .api_route(
            "/v1/connect/lastfm/callback",
            get_with(
                connected_accounts::lastfm_callback,
                connected_accounts::_lastfm_callback_doc,
            ),
        )
        // Catalog
        .api_route(
            "/v1/track/{id}",
            get_with(tracks::get_track, tracks::_get_track_doc),
        )
        .api_route(
            "/v1/artist/{id}",
            get_with(tracks::get_artist, tracks::_get_artist_doc),
        )
        .api_route(
            "/v1/artist/{id}/top-tracks",
            get_with(tracks::artist_top_tracks, tracks::_artist_top_tracks_doc),
        )
        .api_route(
            "/v1/artist/{id}/listeners",
            get_with(tracks::artist_listeners, tracks::_artist_listeners_doc),
        )
        .api_route(
            "/v1/track/{id}/listeners",
            get_with(tracks::track_listeners, tracks::_track_listeners_doc),
        )
        // Community: reading comments (public)
        .api_route(
            "/v1/artist/{id}/comments",
            get_with(
                community::list_artist_comments,
                community::_list_artist_comments_doc,
            ),
        )
        .api_route(
            "/v1/track/{id}/comments",
            get_with(
                community::list_track_comments,
                community::_list_track_comments_doc,
            ),
        )
        .api_route("/v1/search", get_with(tracks::search, tracks::_search_doc))
        // Health
        .api_route(
            "/health",
            get_with(health, |r| r.hidden(true).description("Health check xD")),
        );

    let optional_authed_users = ApiRouter::new()
        // User profiles
        .api_route(
            "/v1/user/{username}",
            get_with(users::get_profile, users::_get_profile_doc),
        )
        .api_route(
            "/v1/user/{username}/friends",
            get_with(users::get_friends, users::_get_friends_doc),
        )
        // Community: image candidates (optional auth flags the viewer's votes)
        .api_route(
            "/v1/artist/{id}/images",
            get_with(
                community::list_artist_images,
                community::_list_artist_images_doc,
            ),
        )
        .api_route(
            "/v1/album/{id}/images",
            get_with(
                community::list_album_images,
                community::_list_album_images_doc,
            ),
        )
        // User scrobble data
        .api_route(
            "/v1/user/{username}/recent",
            get_with(
                scrobbles::recent_scrobbles,
                scrobbles::_recent_scrobbles_doc,
            ),
        )
        .api_route(
            "/v1/user/{username}/live",
            get_with(
                scrobbles::live_now_playing,
                scrobbles::_live_now_playing_doc,
            ),
        )
        .api_route(
            "/v1/user/{username}/top-artists",
            get_with(scrobbles::top_artists, scrobbles::_top_artists_doc),
        )
        .api_route(
            "/v1/user/{username}/top-tracks",
            get_with(scrobbles::top_tracks, scrobbles::_top_tracks_doc),
        )
        .api_route(
            "/v1/user/{username}/heatmap",
            get_with(
                scrobbles::activity_heatmap,
                scrobbles::_activity_heatmap_doc,
            ),
        )
        .with_path_items(|mut item| {
            for (_, op) in iter_operations_mut(item.inner_mut()) {
                op.security = vec![
                    SecurityRequirement::default(),
                    [(BEARER.to_string(), vec![])].into_iter().collect(),
                ];
            }
            item
        })
        .layer(middleware::from_fn_with_state(state.clone(), optional_auth));

    let mut api = OpenApi::default();

    let native = ApiRouter::new()
        .route("/docs", Scalar::new("/api.json").axum_route())
        .merge(authed(
            &state,
            Access::Scope(Scope::Scrobble),
            scrobble_routes,
        ))
        .merge(authed(&state, Access::Scope(Scope::Read), read_routes))
        .merge(authed(&state, Access::Scope(Scope::Write), write_routes))
        .merge(authed(&state, Access::Scope(Scope::Write), upload_routes))
        .merge(authed(&state, Access::Session, session_routes))
        .merge(authed(&state, Access::Any, any_credential_routes))
        .merge(auth_public)
        .merge(public)
        .merge(optional_authed_users);
    let native = match state.cors.layer() {
        Some(cors) => native.layer(cors),
        None => native,
    };
    let mut router = native
        .merge(ApiRouter::from(
            compat::router().layer(cors::protocol_layer()),
        ))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit));
    if let Some(root) = state.media.local_root() {
        router = router.nest_service("/uploads", uploads_service(root));
    }
    let router = router
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .route("/api.json", axum::routing::get(serve_api))
        .finish_api_with(&mut api, api_docs);
    document_rate_limit(&mut api);
    compat::docs::document(&mut api);
    router.layer(Extension(Arc::new(api))).with_state(state)
}

const BEARER: &str = "bearer";

/// A handler panicked: the panic hook has logged it, the client gets the
/// usual opaque 500.
fn panic_response(_: Box<dyn std::any::Any + Send>) -> Response {
    (
        HttpStatus::INTERNAL_SERVER_ERROR,
        axum::Json(crate::errors::ErrorBody {
            error: "internal server error".into(),
        }),
    )
        .into_response()
}

/// The global per-address limit can answer 429 to anything.
fn document_rate_limit(api: &mut OpenApi) {
    let Some(paths) = &mut api.paths else { return };
    for (_, item) in paths.paths.iter_mut() {
        let ReferenceOr::Item(item) = item else {
            continue;
        };
        for (_, op) in iter_operations_mut(item) {
            let has_429 = op
                .responses
                .as_ref()
                .is_some_and(|r| r.responses.contains_key(&StatusCode::Code(429)));
            if !has_429 {
                let _ = TransformOperation::new(op).response_with::<429, ErrorJson, _>(|r| {
                    r.description("Over 60 requests a minute from this address")
                });
            }
            if let Some(responses) = &mut op.responses {
                responses.responses.sort_keys();
            }
        }
    }
}

/// Puts `routes` behind [`require_auth`] with `access`, and documents the
/// credential they take, and the 401 and 403 a request without it gets.
fn authed(state: &AppState, access: Access, routes: ApiRouter<AppState>) -> ApiRouter<AppState> {
    let denied = match access {
        Access::Any => None,
        Access::Scope(scope) => Some(format!(
            "Needs a session or an API token with the `{scope}` scope"
        )),
        Access::Session => Some("Needs a session: API tokens can't do this".to_string()),
    };
    routes
        .with_path_items(|mut item| {
            for (_, op) in iter_operations_mut(item.inner_mut()) {
                let op = TransformOperation::new(op)
                    .security_requirement(BEARER)
                    .response_with::<401, ErrorJson, _>(|r| {
                        r.description("No credential, or an invalid or expired one")
                    });
                if let Some(denied) = &denied {
                    let _ = op.response_with::<403, ErrorJson, _>(|r| r.description(denied));
                }
            }
            item
        })
        .layer(middleware::from_fn_with_state(
            (state.clone(), access),
            require_auth,
        ))
}

async fn health() -> &'static str {
    "ok"
}

fn api_docs(api: TransformOpenApi) -> TransformOpenApi {
    api.title("Scrobblr API")
        .version(env!("CARGO_PKG_VERSION"))
        .description("The API of Scrobblr, a self-hosted music scrobbling service.")
        .security_scheme(
            BEARER,
            aide::openapi::SecurityScheme::Http {
                scheme: "bearer".into(),
                bearer_format: Some("session token or API token".into()),
                description: Some(
                    "`Authorization: Bearer {token}`: a session token from login or \
                     registration (may do anything), or an API token (only what its scopes \
                     allow, and nothing that takes a session)."
                        .into(),
                ),
                extensions: Default::default(),
            },
        )
}
