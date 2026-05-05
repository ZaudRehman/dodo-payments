use axum::{
    extract::{Extension, Path, State},
    Json,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    errors::{AppError, Result},
    middleware::AuthenticatedBusiness,
    models::{CreateCustomerRequest, Customer},
};

pub async fn create_customer(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Json(req): Json<CreateCustomerRequest>,
) -> Result<Json<Customer>> {
    if req.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    if req.email.trim().is_empty() || !req.email.contains('@') {
        return Err(AppError::Validation("valid email is required".into()));
    }

    let customer = sqlx::query_as!(
        Customer,
        r#"INSERT INTO customers (business_id, name, email)
           VALUES ($1, $2, $3)
           RETURNING id, business_id, name, email, created_at"#,
        auth.business_id,
        req.name.trim(),
        req.email.trim().to_lowercase()
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint() == Some("customers_business_id_email_key") => {
            AppError::Validation("A customer with this email already exists".into())
        }
        _ => AppError::Database(e),
    })?;

    Ok(Json(customer))
}

pub async fn get_customer(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
    Path(customer_id): Path<Uuid>,
) -> Result<Json<Customer>> {
    let customer = sqlx::query_as!(
        Customer,
        "SELECT id, business_id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
        customer_id,
        auth.business_id
    )
    .fetch_optional(&pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Customer {} not found", customer_id)))?;

    Ok(Json(customer))
}

pub async fn list_customers(
    State(pool): State<PgPool>,
    Extension(auth): Extension<AuthenticatedBusiness>,
) -> Result<Json<Vec<Customer>>> {
    let customers = sqlx::query_as!(
        Customer,
        "SELECT id, business_id, name, email, created_at FROM customers WHERE business_id = $1 ORDER BY created_at DESC",
        auth.business_id
    )
    .fetch_all(&pool)
    .await?;

    Ok(Json(customers))
}
