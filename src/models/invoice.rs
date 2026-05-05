use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text")]
pub enum InvoiceStatus {
    #[serde(rename = "draft")]
    Draft,
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "paid")]
    Paid,
    #[serde(rename = "void")]
    Void,
    #[serde(rename = "uncollectible")]
    Uncollectible,
}

impl InvoiceStatus {
    pub fn as_str(&self) -> &str {
        match self {
            InvoiceStatus::Draft => "draft",
            InvoiceStatus::Open => "open",
            InvoiceStatus::Paid => "paid",
            InvoiceStatus::Void => "void",
            InvoiceStatus::Uncollectible => "uncollectible",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, InvoiceStatus::Paid | InvoiceStatus::Void | InvoiceStatus::Uncollectible)
    }

    pub fn can_be_paid(&self) -> bool {
        matches!(self, InvoiceStatus::Open)
    }
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LineItem {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Debug, Deserialize)]
pub struct LineItemRequest {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemRequest>,
}

#[derive(Debug, Serialize)]
pub struct InvoiceResponse {
    pub invoice: Invoice,
    pub line_items: Vec<LineItem>,
}
