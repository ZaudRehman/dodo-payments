use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    errors::{AppError, Result},
    middleware::AuthenticatedBusiness,
    models::{RegisterWebhookRequest, WebhookDelivery, WebhookEndpoint},
};

pub async fn register_webhook(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Json(req): Json<RegisterWebhookRequest>,
) -> Result<Json<WebhookEndpoint>> {
    if req.url.trim().is_empty() || !req.url.starts_with("http") {
        return Err(AppError::Validation("url must be a valid HTTP/HTTPS URL".into()));
    }

    // Generate a random signing secret
    let secret: String = {
        use rand::Rng;
        let bytes: [u8; 32] = rand::thread_rng().gen();
        hex::encode(bytes)
    };

    let endpoint = sqlx::query_as!(
        WebhookEndpoint,
        r#"INSERT INTO webhook_endpoints (business_id, url, secret)
           VALUES ($1, $2, $3)
           RETURNING id, business_id, url, active, created_at"#,
        auth.business_id,
        req.url.trim(),
        secret
    )
    .fetch_one(&pool)
    .await?;

    // Return endpoint WITH the secret (only time it's shown)
    Ok(Json(endpoint))
}

pub async fn list_webhooks(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
) -> Result<Json<Vec<WebhookEndpoint>>> {
    let endpoints = sqlx::query_as!(
        WebhookEndpoint,
        "SELECT id, business_id, url, active, created_at FROM webhook_endpoints WHERE business_id = $1 ORDER BY created_at DESC",
        auth.business_id
    )
    .fetch_all(&pool)
    .await?;

    Ok(Json(endpoints))
}

pub async fn delete_webhook(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(endpoint_id): Path<Uuid>,
) -> Result<axum::http::StatusCode> {
    let result = sqlx::query!(
        "DELETE FROM webhook_endpoints WHERE id = $1 AND business_id = $2",
        endpoint_id,
        auth.business_id
    )
    .execute(&pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Webhook endpoint {} not found", endpoint_id)));
    }

    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DeliveryQuery {
    pub status: Option<String>,
}

pub async fn list_deliveries(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Query(params): Query<DeliveryQuery>,
) -> Result<Json<Vec<WebhookDelivery>>> {
    // Used for reconciliation — businesses can replay failed deliveries
    let deliveries = if let Some(status) = params.status {
        sqlx::query_as!(
            WebhookDelivery,
            r#"SELECT wd.id, wd.endpoint_id, wd.event_type, wd.payload, wd.status,
                      wd.attempts, wd.next_attempt_at, wd.last_error, wd.created_at
               FROM webhook_deliveries wd
               JOIN webhook_endpoints we ON wd.endpoint_id = we.id
               WHERE we.business_id = $1 AND wd.status = $2
               ORDER BY wd.created_at DESC"#,
            auth.business_id,
            status
        )
        .fetch_all(&pool)
        .await?
    } else {
        sqlx::query_as!(
            WebhookDelivery,
            r#"SELECT wd.id, wd.endpoint_id, wd.event_type, wd.payload, wd.status,
                      wd.attempts, wd.next_attempt_at, wd.last_error, wd.created_at
               FROM webhook_deliveries wd
               JOIN webhook_endpoints we ON wd.endpoint_id = we.id
               WHERE we.business_id = $1
               ORDER BY wd.created_at DESC"#,
            auth.business_id
        )
        .fetch_all(&pool)
        .await?
    };

    Ok(Json(deliveries))
}
