use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Enqueue a webhook event for all active endpoints of a business.
/// Fire-and-forget — tokio::spawn so it never blocks the API response path.
pub fn enqueue_webhook(
    pool: PgPool,
    business_id: Uuid,
    event_type: &'static str,
    payload: serde_json::Value,
) {
    tokio::spawn(async move {
        if let Err(e) = enqueue_webhook_inner(pool, business_id, event_type, payload).await {
            tracing::error!(error = %e, "Failed to enqueue webhook");
        }
    });
}

async fn enqueue_webhook_inner(
    pool: PgPool,
    business_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    let endpoints = sqlx::query!(
        "SELECT id FROM webhook_endpoints WHERE business_id = $1 AND active = true",
        business_id
    )
    .fetch_all(&pool)
    .await?;

    for endpoint in endpoints {
        sqlx::query!(
            r#"INSERT INTO webhook_deliveries (endpoint_id, event_type, payload, next_attempt_at)
               VALUES ($1, $2, $3, NOW())"#,
            endpoint.id,
            event_type,
            payload
        )
        .execute(&pool)
        .await?;
    }

    Ok(())
}

/// Background worker that delivers pending webhooks with exponential backoff.
/// Retry intervals: 10s, 30s, 2m, 10m, 1h (5 attempts max, ~75 min total budget)
pub async fn run_webhook_worker(pool: PgPool) {
    tracing::info!("Webhook delivery worker started");
    loop {
        if let Err(e) = process_pending_deliveries(&pool).await {
            tracing::error!(error = %e, "Webhook worker error");
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn process_pending_deliveries(pool: &PgPool) -> anyhow::Result<()> {
    let pending = sqlx::query!(
        r#"SELECT wd.id, wd.endpoint_id, wd.event_type, wd.payload, wd.attempts,
                  we.url, we.secret
           FROM webhook_deliveries wd
           JOIN webhook_endpoints we ON wd.endpoint_id = we.id
           WHERE wd.status = 'pending' AND wd.next_attempt_at <= NOW()
           LIMIT 20"#
    )
    .fetch_all(pool)
    .await?;

    for delivery in pending {
        let pool = pool.clone();
        tokio::spawn(async move {
            deliver_webhook(
                pool,
                delivery.id,
                &delivery.url,
                &delivery.secret,
                &delivery.event_type,
                &delivery.payload,
                delivery.attempts,
            )
            .await;
        });
    }

    Ok(())
}

/// Retry schedule (backoff intervals in seconds): 10, 30, 120, 600, 3600
const RETRY_INTERVALS: [i64; 5] = [10, 30, 120, 600, 3600];
const MAX_ATTEMPTS: i32 = 5;

async fn deliver_webhook(
    pool: PgPool,
    delivery_id: Uuid,
    url: &str,
    secret: &str,
    event_type: &str,
    payload: &serde_json::Value,
    current_attempts: i32,
) {
    let timestamp = chrono::Utc::now().timestamp();
    let body = payload.to_string();
    let signed_payload = format!("{}.{}", timestamp, body);

    // HMAC-SHA256 signing: sign timestamp + "." + body
    let signature = sign_payload(secret, &signed_payload);

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        reqwest::Client::new()
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Dodo-Event", event_type)
            .header("X-Dodo-Timestamp", timestamp.to_string())
            .header("X-Dodo-Signature", format!("t={},v1={}", timestamp, signature))
            .body(body)
            .send(),
    )
    .await;

    let new_attempts = current_attempts + 1;
    let success = matches!(result, Ok(Ok(ref r)) if r.status().is_success());

    if success {
        tracing::info!(delivery_id = %delivery_id, url = %url, "Webhook delivered successfully");
        let _ = sqlx::query!(
            "UPDATE webhook_deliveries SET status = 'delivered', attempts = $1 WHERE id = $2",
            new_attempts,
            delivery_id
        )
        .execute(&pool)
        .await;
    } else {
        let error_msg = match result {
            Err(_) => "delivery timeout".to_string(),
            Ok(Err(e)) => e.to_string(),
            Ok(Ok(r)) => format!("HTTP {}", r.status()),
        };

        tracing::warn!(delivery_id = %delivery_id, attempts = new_attempts, error = %error_msg, "Webhook delivery failed");

        if new_attempts >= MAX_ATTEMPTS {
            // Exhausted all retries — mark permanently failed
            tracing::error!(delivery_id = %delivery_id, "Webhook delivery exhausted all retries");
            let _ = sqlx::query!(
                "UPDATE webhook_deliveries SET status = 'failed', attempts = $1, last_error = $2 WHERE id = $3",
                new_attempts,
                error_msg,
                delivery_id
            )
            .execute(&pool)
            .await;
        } else {
            // Schedule next retry with exponential backoff
            let next_interval = RETRY_INTERVALS
                .get(new_attempts as usize)
                .copied()
                .unwrap_or(3600);

            let _ = sqlx::query!(
                r#"UPDATE webhook_deliveries
                   SET attempts = $1, last_error = $2,
                       next_attempt_at = NOW() + ($3 || ' seconds')::interval
                   WHERE id = $4"#,
                new_attempts,
                error_msg,
                next_interval.to_string(),
                delivery_id
            )
            .execute(&pool)
            .await;
        }
    }
}

fn sign_payload(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
