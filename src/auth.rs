use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

/// `None` means auth is disabled (dev mode - loudly warned about at
/// startup). `Some(token)` means every request except `/health` must
/// present `Authorization: Bearer <token>`.
pub type AuthState = Arc<Option<String>>;

pub async fn require_bearer_token(
    State(auth): State<AuthState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = auth.as_ref() else {
        // Auth disabled - pass through. (Startup already logged a warning.)
        return Ok(next.run(req).await);
    };

    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Avoid a timing side-channel on token comparison - not the biggest
/// risk in a personal side project, but it costs nothing to do right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
