use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub idempotency_key: String,
    pub request_hash: String,
    pub card_token: String,
    pub status: String,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct PayInvoiceRequest {
    pub card_token: String,
}

#[derive(Debug, Deserialize)]
pub struct PspResponse {
    pub status: String,
    pub psp_ref: Option<String>,
    pub code: Option<String>,
}
