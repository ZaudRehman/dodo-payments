use axum::{extract::State, Json};
use rand::Rng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use serde_json::json;
use serde::Deserialize;

use crate::errors::{AppError, Result};

#[derive(Deserialize)]
pub struct CreateBusinessRequest {
    pub name: String,
}

pub async fn create_business(
    State(pool): State<PgPool>,
    Json(req): Json<CreateBusinessRequest>,
) -> Result<Json<serde_json::Value>> {
    if req.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }

    let business = sqlx::query!(
        "INSERT INTO businesses (name) VALUES ($1) RETURNING id",
        req.name.trim()
    )
    .fetch_one(&pool)
    .await?;

    // Generate raw API key: "dodo_" + 32 random bytes as hex
    let random_bytes: [u8; 32] = rand::thread_rng().gen();
    let raw_key = format!("dodo_{}", hex::encode(random_bytes));

    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    let key_hash = hex::encode(hasher.finalize());
    let key_prefix = raw_key[..9].to_string(); // "dodo_" + 4 chars

    sqlx::query!(
        "INSERT INTO api_keys (business_id, key_hash, key_prefix) VALUES ($1, $2, $3)",
        business.id,
        key_hash,
        key_prefix
    )
    .execute(&pool)
    .await?;

    tracing::info!(business_id = %business.id, "Created business with API key");

    Ok(Json(json!({
        "business_id": business.id,
        "api_key": raw_key,
        "key_prefix": key_prefix,
        "note": "Save this API key — it will never be shown again"
    })))
}
