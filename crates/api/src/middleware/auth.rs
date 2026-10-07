use std::fmt;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use fred::{interfaces::KeysInterface, types::Expiration};
use uuid::Uuid;

use crate::{errors::AppError, state::AppState};
use db::queries::auth as auth_db;
use db::queries::users as users_db;

/// What an API token may be allowed to do; a session may do all of it.
/// Scopes don't imply one another: a token that should read and write is
/// created with both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Submitting scrobbles and now playing, all a player needs.
    Scrobble,
    /// Reading the account's own data: profile, imports, connected accounts.
    Read,
    /// Changing the account and posting as it: profile, follows, comments,
    /// votes, uploads, imports, catalog refreshes.
    Write,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Scrobble, Scope::Read, Scope::Write];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Scrobble => "scrobble",
            Scope::Read => "read",
            Scope::Write => "write",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == name)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a group of routes needs of the credential, checked by
/// [`require_auth`]. Every authenticated group in `router.rs` names one.
#[derive(Clone, Copy, Debug)]
pub enum Access {
    /// Any credential.
    Any,
    /// A session, or an API token holding the scope.
    Scope(Scope),
    /// A session only. For what manages credentials or links accounts, so
    /// that no API token can mint a credential that outlives its revocation.
    Session,
}

/// The credential a request authenticated with.
#[derive(Clone, Debug)]
pub enum Credential {
    /// A login session, which may do anything.
    Session,
    /// An API token, with those of its scopes this server knows.
    ApiToken { id: Uuid, scopes: Vec<Scope> },
}

impl Credential {
    /// Whether this credential may use routes that need `access`.
    pub fn check(&self, access: Access) -> Result<(), AppError> {
        match (self, access) {
            (Credential::Session, _) | (_, Access::Any) => Ok(()),
            (Credential::ApiToken { scopes, .. }, Access::Scope(scope)) => {
                if scopes.contains(&scope) {
                    Ok(())
                } else {
                    Err(AppError::MissingScope(scope))
                }
            }
            (Credential::ApiToken { .. }, Access::Session) => Err(AppError::SessionRequired),
        }
    }
}

/// Authenticated user injected into request extensions.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: i64,
    #[expect(dead_code)] // temporary
    pub username: String,
    pub credential: Credential,
}

/// Middleware that validates the `Authorization: Bearer <token>` header and
/// that its credential has the `access` the routes behind it need (403
/// otherwise).
///
/// Accepts two token formats:
/// - **Session UUID** — validated against `user_sessions` (cached in Redis).
///   A session may do anything.
/// - **API token** — SHA-256 hashed, validated against `api_tokens`. It may
///   do what its scopes cover, and nothing that takes a session.
///
/// In both cases `last_used_at` is updated in a background task so it does
/// not block the request.
pub async fn require_auth(
    State((state, access)): State<(AppState, Access)>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let token = extract_bearer_token(&req)?;

    let auth_user = if let Ok(session_id) = Uuid::parse_str(&token) {
        let user_id = resolve_session_cached(&state, session_id).await?;
        let user = users_db::find_by_id(&state.db, user_id)
            .await?
            .ok_or(AppError::Unauthorized)?;

        let pool = state.db.clone();
        tokio::spawn(async move {
            let _ = auth_db::touch_session(&pool, session_id).await;
        });

        AuthUser {
            id: user.id,
            username: user.username,
            credential: Credential::Session,
        }
    } else {
        let hash = auth_db::hash_api_token(&token);
        let api_token = auth_db::find_api_token_by_hash(&state.db, &hash)
            .await?
            .ok_or(AppError::Unauthorized)?;

        let user = users_db::find_by_id(&state.db, api_token.user_id)
            .await?
            .ok_or(AppError::Unauthorized)?;

        let pool = state.db.clone();
        let token_id = api_token.id;
        tokio::spawn(async move {
            let _ = auth_db::touch_api_token(&pool, token_id).await;
        });

        AuthUser {
            id: user.id,
            username: user.username,
            credential: Credential::ApiToken {
                id: api_token.id,
                // Names stored before creation checked them grant nothing.
                scopes: api_token
                    .scopes
                    .iter()
                    .filter_map(|name| Scope::parse(name))
                    .collect(),
            },
        }
    };

    auth_user.credential.check(access)?;
    req.extensions_mut().insert(auth_user);
    Ok(next.run(req).await)
}

/// Attempts to resolve a session token to an [`AuthUser`], returning `None`
/// on any failure (invalid UUID, no matching session, user not found, or a
/// transient DB/Redis error).
async fn try_authenticate_session(state: &AppState, token: &str) -> Option<AuthUser> {
    let session_id = Uuid::parse_str(token).ok()?;
    let user_id = resolve_session_cached(state, session_id).await.ok()?;
    let user = users_db::find_by_id(&state.db, user_id).await.ok()??;

    Some(AuthUser {
        id: user.id,
        username: user.username,
        credential: Credential::Session,
    })
}

/// Middleware that *optionally* authenticates a request via session token.
///
/// Unlike [`require_auth`], a missing, malformed, or invalid token is not an
/// error — the request simply proceeds with no [`AuthUser`] in its
/// extensions. This also means a transient failure (DB or Redis hiccup) is
/// indistinguishable from "not logged in"; only use this where that's okay.
///
/// Only session tokens are accepted here, not API tokens.
pub async fn optional_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Ok(token) = extract_bearer_token(&req)
        && let Some(user) = try_authenticate_session(&state, &token).await
    {
        req.extensions_mut().insert(user);
    }

    next.run(req).await
}

/// Resolves a session UUID to a `user_id`, using Redis as a read-through cache.
///
/// TTL is set to the remaining lifetime of the session so the cache entry
/// never outlives the underlying row.
async fn resolve_session_cached(state: &AppState, session_id: Uuid) -> Result<i64, AppError> {
    let cache_key = format!("session:{session_id}");

    if let Ok(Some(uid_str)) = state.redis.get::<Option<String>, _>(&cache_key).await
        && let Ok(uid) = uid_str.parse::<i64>()
    {
        return Ok(uid);
    }

    let session = auth_db::get_session(&state.db, session_id)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let ttl_secs = (session.expires_at - chrono::Utc::now())
        .num_seconds()
        .max(1);
    let _ = state
        .redis
        .set::<(), _, _>(
            &cache_key,
            session.user_id.to_string(),
            Some(Expiration::EX(ttl_secs)),
            None,
            false,
        )
        .await;

    Ok(session.user_id)
}

fn extract_bearer_token(req: &Request) -> Result<String, AppError> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;

    header
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
        .ok_or(AppError::Unauthorized)
}
