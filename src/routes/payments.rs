use axum::{
    extract::{Extension, Path, State},
    http::HeaderMap,
    Json,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::{
    errors::{AppError, Result},
    middleware::AuthenticatedBusiness,
    models::{PayInvoiceRequest, PaymentAttempt, PspResponse},
    webhook::enqueue_webhook,
};

pub async fn pay_invoice(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<PayInvoiceRequest>,
) -> Result<Json<PaymentAttempt>> {
    // --- Extract idempotency key ---
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Validation("Idempotency-Key header is required".into()))?
        .to_string();

    if req.card_token.trim().is_empty() {
        return Err(AppError::Validation("card_token is required".into()));
    }

    // --- Compute request hash for idempotency body mismatch detection ---
    let request_body = serde_json::json!({ "card_token": req.card_token });
    let mut hasher = Sha256::new();
    hasher.update(request_body.to_string().as_bytes());
    let request_hash = hex::encode(hasher.finalize());

    // --- Idempotency check: return early if key was seen before ---
    if let Some(existing) = sqlx::query_as!(
        PaymentAttempt,
        r#"SELECT id, invoice_id, idempotency_key, card_token, status,
                  psp_ref, failure_code, request_hash, created_at, updated_at
           FROM payment_attempts WHERE idempotency_key = $1"#,
        idempotency_key
    )
    .fetch_optional(&pool)
    .await?
    {
        // Key exists — check for body mismatch
        if existing.request_hash != request_hash {
            return Err(AppError::IdempotencyConflict);
        }
        // Same body: return the existing attempt (idempotent)
        tracing::info!(idempotency_key = %idempotency_key, "Returning cached idempotent response");
        return Ok(Json(existing));
    }

    // --- BEGIN TRANSACTION: lock invoice row, validate state, insert attempt ---
    let mut tx = pool.begin().await?;

    // SELECT FOR UPDATE — row-level lock prevents concurrent double-pays
    let invoice = sqlx::query!(
        "SELECT id, business_id, status FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
        invoice_id,
        auth.business_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Invoice {} not found", invoice_id)))?;

    // State machine guard: only 'open' invoices can be paid
    if invoice.status != "open" {
        return Err(AppError::InvalidStateTransition {
            current: invoice.status.clone(),
            attempted: "pay".into(),
        });
    }

    // Insert payment attempt as 'pending' before calling PSP
    // This ensures idempotency even if we crash after PSP responds
    let attempt = sqlx::query_as!(
        PaymentAttempt,
        r#"INSERT INTO payment_attempts
               (invoice_id, idempotency_key, request_hash, card_token, status)
           VALUES ($1, $2, $3, $4, 'pending')
           RETURNING id, invoice_id, idempotency_key, card_token, status,
                     psp_ref, failure_code, request_hash, created_at, updated_at"#,
        invoice_id,
        idempotency_key,
        request_hash,
        req.card_token.trim()
    )
    .fetch_one(&mut *tx)
    .await?;

    // Commit the pending attempt BEFORE calling PSP
    // This ensures idempotency key is stored even if the service crashes
    tx.commit().await?;

    // --- Call PSP with a timeout ---
    // tok_timeout sleeps 30s — we timeout at 10s and mark the attempt failed
    let psp_url = std::env::var("PSP_BASE_URL").unwrap_or_else(|_| "http://localhost:3001".into());

    let psp_result = timeout(
        Duration::from_secs(10),
        reqwest::Client::new()
            .post(format!("{}/charge", psp_url))
            .json(&serde_json::json!({
                "card_token": req.card_token,
                "amount_cents": 0i64, // total from invoice — simplified for demo
                "currency": "USD",
                "idempotency_key": idempotency_key
            }))
            .send(),
    )
    .await;

    // --- Process PSP result and update invoice state ---
    let (final_status, psp_ref, failure_code, invoice_status) = match psp_result {
        // Timeout — PSP took too long
        Err(_) => {
            tracing::warn!(invoice_id = %invoice_id, "PSP timed out");
            (
                "failed",
                None::<String>,
                Some("psp_timeout".to_string()),
                None, // Invoice stays 'open' — not corrupted
            )
        }
        // Network/HTTP error
        Ok(Err(e)) => {
            tracing::error!(error = %e, "PSP network error");
            (
                "failed",
                None,
                Some("psp_network_error".to_string()),
                None,
            )
        }
        // Got a response — parse it
        Ok(Ok(resp)) => {
            if !resp.status().is_success() {
                tracing::error!(status = %resp.status(), "PSP returned error status");
                (
                    "failed",
                    None,
                    Some(format!("psp_http_error_{}", resp.status().as_u16())),
                    None,
                )
            } else {
                match resp.json::<PspResponse>().await {
                    Ok(psp) if psp.status == "succeeded" => (
                        "succeeded",
                        psp.psp_ref,
                        None,
                        Some("paid"),
                    ),
                    Ok(psp) => (
                        "failed",
                        None,
                        psp.code,
                        None, // Invoice stays 'open' on payment failure
                    ),
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to parse PSP response");
                        ("failed", None, Some("psp_parse_error".to_string()), None)
                    }
                }
            }
        }
    };

    // --- Update payment attempt and invoice in a single transaction ---
    let mut tx2 = pool.begin().await?;

    let updated_attempt = sqlx::query_as!(
        PaymentAttempt,
        r#"UPDATE payment_attempts
           SET status = $1, psp_ref = $2, failure_code = $3, updated_at = NOW()
           WHERE id = $4
           RETURNING id, invoice_id, idempotency_key, card_token, status,
                     psp_ref, failure_code, request_hash, created_at, updated_at"#,
        final_status,
        psp_ref,
        failure_code,
        attempt.id
    )
    .fetch_one(&mut *tx2)
    .await?;

    if let Some(new_invoice_status) = invoice_status {
        sqlx::query!(
            "UPDATE invoices SET status = $1, updated_at = NOW() WHERE id = $2",
            new_invoice_status,
            invoice_id
        )
        .execute(&mut *tx2)
        .await?;
    }

    tx2.commit().await?;

    // --- Enqueue webhook (non-blocking — does not affect API response) ---
    let event_type = if final_status == "succeeded" {
        "invoice.paid"
    } else {
        "invoice.payment_failed"
    };

    enqueue_webhook(
        pool.clone(),
        auth.business_id,
        event_type,
        serde_json::json!({
            "event": event_type,
            "invoice_id": invoice_id,
            "payment_attempt_id": updated_attempt.id,
            "business_id": auth.business_id,
            "status": final_status,
            "failure_code": failure_code,
        }),
    );

    Ok(Json(updated_attempt))
}

pub async fn list_payment_attempts(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<Vec<PaymentAttempt>>> {
    // Verify invoice belongs to this business
    let exists = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM invoices WHERE id = $1 AND business_id = $2)",
        invoice_id,
        auth.business_id
    )
    .fetch_one(&pool)
    .await?
    .unwrap_or(false);

    if !exists {
        return Err(AppError::NotFound(format!("Invoice {} not found", invoice_id)));
    }

    let attempts = sqlx::query_as!(
        PaymentAttempt,
        r#"SELECT id, invoice_id, idempotency_key, card_token, status,
                  psp_ref, failure_code, request_hash, created_at, updated_at
           FROM payment_attempts WHERE invoice_id = $1 ORDER BY created_at DESC"#,
        invoice_id
    )
    .fetch_all(&pool)
    .await?;

    Ok(Json(attempts))
}
