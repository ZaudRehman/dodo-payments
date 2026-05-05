use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

#[derive(Clone, Debug)]
pub struct AuthenticatedBusiness {
    pub business_id: Uuid,
}

pub async fn auth_middleware(
    State(pool): State<PgPool>,
    mut req: Request,
    next: Next,
) -> std::result::Result<Response, AppError> {
    let api_key = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthorized)?
        .to_string();

    // Hash the provided key and look it up
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());

    let row = sqlx::query!(
        "SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
        key_hash
    )
    .fetch_optional(&pool)
    .await?
    .ok_or(AppError::Unauthorized)?;

    req.extensions_mut().insert(AuthenticatedBusiness {
        business_id: row.business_id,
    });

    Ok(next.run(req).await)
}
