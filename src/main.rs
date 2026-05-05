mod errors;
mod middleware;
mod models;
mod routes;
mod webhook;

use axum::{
    middleware as axum_middleware,
    routing::{delete, get, post},
    Router,
};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;

    // Run migrations on startup — idempotent
    sqlx::migrate!("./migrations").run(&pool).await?;
    tracing::info!("Migrations applied");

    // Start background webhook delivery worker
    let worker_pool = pool.clone();
    tokio::spawn(async move {
        webhook::run_webhook_worker(worker_pool).await;
    });

    // Protected routes — require Bearer API key
    let protected = Router::new()
        .route("/customers", post(routes::customers::create_customer))
        .route("/customers", get(routes::customers::list_customers))
        .route("/customers/:id", get(routes::customers::get_customer))
        .route("/invoices", post(routes::invoices::create_invoice))
        .route("/invoices", get(routes::invoices::list_invoices))
        .route("/invoices/:id", get(routes::invoices::get_invoice))
        .route("/invoices/:id/void", post(routes::invoices::void_invoice))
        .route("/invoices/:id/pay", post(routes::payments::pay_invoice))
        .route("/invoices/:id/payment-attempts", get(routes::payments::list_payment_attempts))
        .route("/webhooks", post(routes::webhooks::register_webhook))
        .route("/webhooks", get(routes::webhooks::list_webhooks))
        .route("/webhooks/:id", delete(routes::webhooks::delete_webhook))
        .route("/webhook-deliveries", get(routes::webhooks::list_deliveries))
        .layer(axum_middleware::from_fn_with_state(
            pool.clone(),
            middleware::auth_middleware,
        ))
        .with_state(pool.clone());

    // Public admin — seed businesses/keys (disable in production)
    let admin = Router::new()
        .route("/admin/businesses", post(routes::admin::create_business))
        .with_state(pool.clone());

    let app = Router::new()
        .merge(protected)
        .merge(admin)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    tracing::info!("Server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
