use axum::{extract::Json, routing::post, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

#[derive(Deserialize)]
struct ChargeRequest {
    card_token: String,
    amount_cents: i64,
    currency: String,
    idempotency_key: String,
}

#[derive(Serialize)]
struct ChargeResponse {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

async fn charge(Json(req): Json<ChargeRequest>) -> Json<ChargeResponse> {
    tracing::info!(
        token = %req.card_token,
        amount = req.amount_cents,
        idempotency_key = %req.idempotency_key,
        "PSP charge request"
    );

    match req.card_token.as_str() {
        "tok_success" => {
            sleep(Duration::from_millis(100)).await;
            Json(ChargeResponse {
                status: "succeeded".into(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            })
        }
        "tok_insufficient_funds" => {
            sleep(Duration::from_millis(100)).await;
            Json(ChargeResponse {
                status: "failed".into(),
                psp_ref: None,
                code: Some("insufficient_funds".into()),
            })
        }
        "tok_card_declined" => {
            sleep(Duration::from_millis(100)).await;
            Json(ChargeResponse {
                status: "failed".into(),
                psp_ref: None,
                code: Some("card_declined".into()),
            })
        }
        "tok_timeout" => {
            // Sleeps 30 seconds — the invoice service MUST timeout before this resolves
            sleep(Duration::from_secs(30)).await;
            Json(ChargeResponse {
                status: "succeeded".into(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            })
        }
        "tok_network_error" => {
            // Return 500 — handled by the invoice service's error path
            Json(ChargeResponse {
                status: "failed".into(),
                psp_ref: None,
                code: Some("network_error".into()),
            })
        }
        _ => {
            sleep(Duration::from_millis(100)).await;
            Json(ChargeResponse {
                status: "failed".into(),
                psp_ref: None,
                code: Some("unknown_token".into()),
            })
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new().route("/charge", post(charge));

    let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
    tracing::info!("Mock PSP listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
