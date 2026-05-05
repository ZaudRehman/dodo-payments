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
    models::{CreateInvoiceRequest, Invoice, InvoiceResponse, LineItem},
    webhook::enqueue_webhook,
};

#[derive(Deserialize)]
pub struct InvoiceListQuery {
    pub status: Option<String>,
}

pub async fn create_invoice(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Json(req): Json<CreateInvoiceRequest>,
) -> Result<Json<InvoiceResponse>> {
    if req.line_items.is_empty() {
        return Err(AppError::Validation("At least one line item is required".into()));
    }

    // Validate all line items
    for item in &req.line_items {
        if item.quantity <= 0 {
            return Err(AppError::Validation("quantity must be positive".into()));
        }
        if item.unit_amount_cents < 0 {
            return Err(AppError::Validation("unit_amount_cents must be non-negative".into()));
        }
        if item.description.trim().is_empty() {
            return Err(AppError::Validation("line item description is required".into()));
        }
    }

    // Server-side total computation — never trust client
    let total_cents: i64 = req
        .line_items
        .iter()
        .map(|item| item.quantity as i64 * item.unit_amount_cents)
        .sum();

    // Verify customer belongs to this business
    let customer_exists = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)",
        req.customer_id,
        auth.business_id
    )
    .fetch_one(&pool)
    .await?
    .unwrap_or(false);

    if !customer_exists {
        return Err(AppError::NotFound(format!("Customer {} not found", req.customer_id)));
    }

    let mut tx = pool.begin().await?;

    let invoice = sqlx::query_as!(
        Invoice,
        r#"INSERT INTO invoices (business_id, customer_id, status, total_cents, due_date)
           VALUES ($1, $2, 'open', $3, $4)
           RETURNING id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at"#,
        auth.business_id,
        req.customer_id,
        total_cents,
        req.due_date
    )
    .fetch_one(&mut *tx)
    .await?;

    let mut line_items = Vec::new();
    for item in &req.line_items {
        let amount_cents = item.quantity as i64 * item.unit_amount_cents;
        let li = sqlx::query_as!(
            LineItem,
            r#"INSERT INTO line_items (invoice_id, description, quantity, unit_amount_cents, amount_cents)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING id, invoice_id, description, quantity, unit_amount_cents, amount_cents"#,
            invoice.id,
            item.description.trim(),
            item.quantity,
            item.unit_amount_cents,
            amount_cents
        )
        .fetch_one(&mut *tx)
        .await?;
        line_items.push(li);
    }

    tx.commit().await?;

    // Enqueue webhook (non-blocking)
    enqueue_webhook(
        pool.clone(),
        auth.business_id,
        "invoice.created",
        serde_json::json!({
            "event": "invoice.created",
            "invoice_id": invoice.id,
            "business_id": auth.business_id,
            "status": invoice.status,
            "total_cents": invoice.total_cents,
        }),
    );

    Ok(Json(InvoiceResponse { invoice, line_items }))
}

pub async fn get_invoice(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceResponse>> {
    let invoice = sqlx::query_as!(
        Invoice,
        "SELECT id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at
         FROM invoices WHERE id = $1 AND business_id = $2",
        invoice_id,
        auth.business_id
    )
    .fetch_optional(&pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Invoice {} not found", invoice_id)))?;

    let line_items = sqlx::query_as!(
        LineItem,
        "SELECT id, invoice_id, description, quantity, unit_amount_cents, amount_cents
         FROM line_items WHERE invoice_id = $1 ORDER BY id",
        invoice_id
    )
    .fetch_all(&pool)
    .await?;

    Ok(Json(InvoiceResponse { invoice, line_items }))
}

pub async fn list_invoices(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Query(params): Query<InvoiceListQuery>,
) -> Result<Json<Vec<Invoice>>> {
    let invoices = if let Some(status) = params.status {
        let valid_statuses = ["draft", "open", "paid", "void", "uncollectible"];
        if !valid_statuses.contains(&status.as_str()) {
            return Err(AppError::Validation(format!(
                "Invalid status '{}'. Valid: draft, open, paid, void, uncollectible",
                status
            )));
        }
        sqlx::query_as!(
            Invoice,
            "SELECT id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at
             FROM invoices WHERE business_id = $1 AND status = $2 ORDER BY created_at DESC",
            auth.business_id,
            status
        )
        .fetch_all(&pool)
        .await?
    } else {
        sqlx::query_as!(
            Invoice,
            "SELECT id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at
             FROM invoices WHERE business_id = $1 ORDER BY created_at DESC",
            auth.business_id
        )
        .fetch_all(&pool)
        .await?
    };

    Ok(Json(invoices))
}

pub async fn void_invoice(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<Invoice>> {
    let mut tx = pool.begin().await?;

    let invoice = sqlx::query_as!(
        Invoice,
        "SELECT id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at
         FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
        invoice_id,
        auth.business_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Invoice {} not found", invoice_id)))?;

    if invoice.status != "draft" && invoice.status != "open" {
        return Err(AppError::InvalidStateTransition {
            current: invoice.status.clone(),
            attempted: "void".into(),
        });
    }

    let updated = sqlx::query_as!(
        Invoice,
        "UPDATE invoices SET status = 'void', updated_at = NOW() WHERE id = $1
         RETURNING id, business_id, customer_id, status, total_cents, due_date, created_at, updated_at",
        invoice_id
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Json(updated))
}
