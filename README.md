# Dodo Payments — Invoice & Payment Service

A minimal Invoice & Payment Service built in Rust (Axum + PostgreSQL).

## Demo Video

> 📹 **[PASTE LOOM LINK HERE]**

---

## Stack

- **Runtime:** Rust + Tokio
- **Framework:** Axum 0.7
- **Database:** PostgreSQL 16 (sqlx with compile-time queries)
- **Mock PSP:** Separate Axum binary
- **Webhooks:** Background delivery worker with exponential backoff

---

## Quick Start

```bash
# 1. Clone the repo
git clone <your-repo-url>
cd dodo-payments

# 2. Start everything — app, database, mock PSP
docker compose up --build

# The app is now running on http://localhost:3000
# The mock PSP is on http://localhost:3001
```

No manual steps required. Migrations run automatically on startup.

---

## Seed Your First Business & API Key

```bash
curl -s -X POST http://localhost:3000/admin/businesses \
  -H "Content-Type: application/json" \
  -d '{"name": "Acme Corp"}' | jq .
```

**Save the `api_key` from the response — it is only shown once.**

```json
{
  "business_id": "xxxxxxxx-...",
  "api_key": "dodo_abc123...",
  "key_prefix": "dodo_ab12",
  "note": "Save this API key — it will never be shown again"
}
```

Export it for the examples below:
```bash
export API_KEY="dodo_abc123..."
```

---

## curl Examples

### 1. Create a Customer

```bash
curl -s -X POST http://localhost:3000/customers \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Jane Smith",
    "email": "jane@example.com"
  }' | jq .
```

Export the customer ID:
```bash
export CUSTOMER_ID="<id from response>"
```

### 2. Create an Invoice

```bash
curl -s -X POST http://localhost:3000/invoices \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d "{
    \"customer_id\": \"$CUSTOMER_ID\",
    \"due_date\": \"2026-12-31\",
    \"line_items\": [
      { \"description\": \"Pro Plan — Monthly\", \"quantity\": 1, \"unit_amount_cents\": 4999 },
      { \"description\": \"Setup Fee\", \"quantity\": 1, \"unit_amount_cents\": 10000 }
    ]
  }" | jq .
```

Export the invoice ID:
```bash
export INVOICE_ID="<id from response>"
```

### 3. Successful Payment

```bash
curl -s -X POST http://localhost:3000/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$(uuidgen)" \
  -d '{"card_token": "tok_success"}' | jq .
```

Expected: `status: "succeeded"`, invoice transitions to `paid`.

### 4. Failed Payment (Card Declined)

```bash
# Create a fresh invoice first (paid invoices cannot be paid again)
export INVOICE_ID2="<new invoice id>"

curl -s -X POST http://localhost:3000/invoices/$INVOICE_ID2/pay \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$(uuidgen)" \
  -d '{"card_token": "tok_card_declined"}' | jq .
```

Expected: `status: "failed"`, `failure_code: "card_declined"`, invoice stays `open`.

### 5. Register a Webhook Endpoint

```bash
curl -s -X POST http://localhost:3000/webhooks \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"url": "https://webhook.site/your-unique-url"}' | jq .
```

---

## Tests

```bash
# Run integration tests (requires a running PostgreSQL)
DATABASE_URL=postgres://dodo:secret@localhost:5432/dodo cargo test -- --test-threads=1
```

The three required tests cover:
1. **Concurrency** — N concurrent `POST /pay` → exactly 1 succeeds
2. **Idempotency** — same key retry → same result, no duplicate attempt
3. **PSP failure** — `tok_timeout` → invoice stays `open`, not corrupted

---

## Project Structure

```
dodo-payments/
├── src/
│   ├── main.rs              # App entry point, router setup
│   ├── errors.rs            # Unified AppError → consistent JSON responses
│   ├── webhook.rs           # Background delivery worker
│   ├── middleware/auth.rs   # Bearer API key authentication
│   ├── models/              # DB structs and request/response types
│   └── routes/              # HTTP handlers
│       ├── customers.rs
│       ├── invoices.rs
│       ├── payments.rs      # POST /pay — idempotency, concurrency, PSP
│       ├── webhooks.rs
│       └── admin.rs         # Seed businesses/keys
├── mock_psp/src/main.rs     # Separate Axum binary — mock PSP
├── migrations/              # SQL migrations (run automatically on startup)
├── tests/integration_tests.rs
├── docker-compose.yml
├── Dockerfile
├── mock_psp/Dockerfile
├── DESIGN.md                # Primary deliverable
├── AI_USAGE.md
└── api-docs.yaml            # OpenAPI 3.0
```
