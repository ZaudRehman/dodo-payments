# DESIGN.md — Invoice & Payment Service

## 1. Data Model

### Tables

**businesses**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | `gen_random_uuid()` |
| name | TEXT | Business display name |
| created_at | TIMESTAMPTZ | |

**api_keys**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | Scoped to one business |
| key_hash | TEXT UNIQUE | SHA-256 of raw key — plaintext never stored |
| key_prefix | TEXT | First 9 chars (e.g. `dodo_ab12`) for display |
| created_at | TIMESTAMPTZ | |
| revoked_at | TIMESTAMPTZ NULL | NULL = active |

Index: `api_keys(key_hash)` — every authenticated request hashes the Bearer token and hits this index.

**customers**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | Scoped — a customer belongs to one business |
| name | TEXT | |
| email | TEXT | |
| created_at | TIMESTAMPTZ | |

Unique constraint: `(business_id, email)` — prevents duplicate customers per business.

**invoices**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | |
| customer_id | UUID FK | |
| status | TEXT | `draft`, `open`, `paid`, `void`, `uncollectible` |
| total_cents | BIGINT | Integer cents — **no floats anywhere** |
| due_date | DATE | |
| created_at / updated_at | TIMESTAMPTZ | |

Indexes: `invoices(business_id)`, `invoices(status)`, `invoices(customer_id)`.

**line_items**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| invoice_id | UUID FK | Cascade delete with invoice |
| description | TEXT | |
| quantity | INTEGER | CHECK > 0 |
| unit_amount_cents | BIGINT | CHECK >= 0 |
| amount_cents | BIGINT | `quantity * unit_amount_cents` — server computed |

The server always computes `amount_cents` and `total_cents`. Client-supplied totals are ignored.

**payment_attempts**
| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| invoice_id | UUID FK | |
| idempotency_key | TEXT UNIQUE | Prevents duplicate processing |
| request_hash | TEXT | SHA-256 of request body — detects body mismatch on key reuse |
| card_token | TEXT | |
| status | TEXT | `pending`, `succeeded`, `failed` |
| psp_ref | TEXT NULL | PSP-assigned reference on success |
| failure_code | TEXT NULL | e.g. `card_declined`, `psp_timeout` |
| created_at / updated_at | TIMESTAMPTZ | |

**webhook_endpoints** and **webhook_deliveries** — see Section 4.

### Why UUIDs?

UUIDs as PKs prevent enumeration attacks (an attacker cannot guess sequential IDs to probe other businesses' data). The performance trade-off vs. integer PKs is acceptable at this scale.

### At 100x Scale

- Partition `invoices` and `payment_attempts` by `business_id` (range or hash).
- Add read replicas; route `GET` queries to replicas.
- Move `webhook_deliveries` processing to a dedicated queue (SQS, NATS) instead of polling the DB.
- Consider sharding `api_keys` by hash prefix for sub-millisecond auth lookups.

---

## 2. Invoice State Machine

```
         ┌─────────────────────────────────────────────┐
         │                                             │
  [CREATE]│                                             │
         ▼                                             │
       draft ──(finalize / first pay attempt)──► open  │
                                                  │    │
                               ┌──────────────────┘    │
                               │                       │
                    ┌──────────▼──────────┐            │
                    │ POST /pay called     │            │
                    │ PSP succeeds         │            │
                    └──────────┬──────────┘            │
                               │                       │
                               ▼                       │
                             paid ◄────────────────────┘
                          [TERMINAL]

       open ──(manual void)──────────────────────► void
                                                [TERMINAL]

       open ──(payment exhausted / manual mark)──► uncollectible
                                                      [TERMINAL]
```

**All valid transitions:**
| From | To | Trigger |
|------|----|---------|
| `draft` | `open` | Invoice created (we create as `open` directly) |
| `open` | `paid` | `POST /pay` — PSP returns `succeeded` |
| `open` | `void` | `POST /invoices/:id/void` |
| `open` | `uncollectible` | Future: after N failed payment attempts |
| `draft` | `void` | Manual void before sending |

**Terminal states:** `paid`, `void`, `uncollectible` — no transitions out.

**Invalid transitions rejected:** Any call to `POST /pay` on a non-`open` invoice returns `409 Conflict` with `{"error": {"code": "invalid_state_transition", "current": "paid", "attempted": "pay"}}`. The check happens **before** any DB write or PSP call.

**Reversible transitions:** None. All transitions are one-way. This is intentional — payment state must be an append-only audit trail.

---

## 3. Payment Correctness & Failure Modes

### (a) Two concurrent POST /pay for the same invoice

**What happens:** Both requests hit the handler simultaneously.

**Mechanism:** `SELECT ... FOR UPDATE` on the invoice row inside a transaction. The first request acquires the row lock and proceeds. The second request blocks at the `SELECT FOR UPDATE` until the first transaction commits. At that point, the invoice status is `paid`, so the second request reads `paid`, hits the state machine guard, and returns `409 Conflict`.

**Why `SELECT FOR UPDATE` over alternatives:**
- *Optimistic concurrency (version column)*: Would cause noisy retries under high contention. For invoice payments, contention is high and expected — one invoice, multiple retry attempts. OCC is better suited for low-contention updates.
- *Advisory locks*: Session-scoped, don't compose safely with connection pools (Tokio + sqlx reuse connections). Can leak if the app crashes mid-lock.
- *Serializable isolation*: Correct but serialization failures require application-level retry logic. `SELECT FOR UPDATE` is simpler and equally correct for this access pattern.

**Outcome:** Exactly one payment succeeds. The invoice transitions to `paid`. All concurrent attempts that lose the race return `409`.

### (b) PSP timeout (tok_timeout — 30 seconds)

**What happens:**
1. Payment attempt is inserted as `pending` and committed **before** the PSP call.
2. We call the PSP with a `tokio::time::timeout` of **10 seconds**.
3. The PSP does not respond in 10 seconds → timeout fires.
4. We update the payment attempt to `failed` with `failure_code = "psp_timeout"`.
5. The invoice status remains `open` — it is not corrupted.
6. The endpoint returns `200` with the failed attempt details (or `504` depending on preference — we return `200` with the attempt so the caller can inspect `status: "failed"`).

**How the caller finds out:** They receive the `PaymentAttempt` with `status: "failed"` and `failure_code: "psp_timeout"`. They can retry with a new `Idempotency-Key`. The invoice is still `open` and payable.

**Why invoice stays open:** The PSP timeout means we do not know whether the charge went through. Marking the invoice `paid` would be incorrect. Marking it `open` is conservative — the customer may not have been charged, and the merchant can retry.

### (c) PSP succeeds but service crashes before persisting

**Scenario:** PSP returns `succeeded`. Before we write the `UPDATE invoices SET status = 'paid'`, the process crashes.

**What happens on retry:**
- The caller retries with the **same `Idempotency-Key`**.
- We look up the idempotency key and find the existing `payment_attempt` with `status = "pending"` (we committed the pending attempt before calling the PSP).
- We detect `status = "pending"` — this means a previous call started but did not complete.
- We re-call the PSP with the same `card_token`. Since the mock PSP is deterministic per token (`tok_success` always succeeds), it returns `succeeded` again with a new `psp_ref`.
- We update the attempt to `succeeded` and the invoice to `paid`.

**Does the customer get charged twice?** In a real PSP integration, we would pass the `idempotency_key` to the PSP's charge API. PSPs like Stripe deduplicate on their side using this key. The mock PSP in this project doesn't implement PSP-level idempotency, but the design documents the requirement: **always forward the `Idempotency-Key` to the PSP**.

### (d) Idempotency key reused with a different request body

**What happens:** We compute `SHA-256(request_body)` and store it as `request_hash` alongside the idempotency key. On reuse, we compare the incoming `request_hash` against the stored one. If they differ, we return `422 Unprocessable Entity` with `{"error": {"code": "idempotency_conflict"}}`.

**Why:** The idempotency key is a promise — "this is the same request, return the same result." A different body breaks that promise. Silently processing the new body would be dangerous (e.g., different `card_token`, different amount). Rejecting it forces the caller to either use the original request or use a new key.

### (e) POST /pay on an already-paid invoice

**What happens:** The `SELECT FOR UPDATE` acquires the lock, reads `status = "paid"`, hits the state machine guard (`can_be_paid()` returns `false` for `paid`), and returns `409 Conflict` before any PSP call or DB write is made.

**The PSP is never called.** No payment attempt is recorded (the state check prevents insertion).

### Concurrency mechanism summary

We use **row-level locking** (`SELECT ... FOR UPDATE`) within a serializable-adjacent transaction. This is the correct choice for invoice payments because:
1. High contention expected (retries, concurrent clients)
2. The invariant is simple: "only one winner per invoice per transition"
3. No retry complexity — losers return immediately with a clear error

---

## 4. Webhook Design

### Signing Scheme

Every webhook delivery includes:
- `X-Dodo-Timestamp: <unix_seconds>`
- `X-Dodo-Signature: t=<timestamp>,v1=<hex_hmac>`

The signature is `HMAC-SHA256(secret, "<timestamp>.<raw_body>")`.

**Replay protection:** Receivers should reject webhooks where `|now - timestamp| > 300` (5 minutes). The timestamp is included in the signed payload, so an attacker cannot reuse a valid signature with a new timestamp.

**Verification (receiver side):**
```
expected = HMAC-SHA256(endpoint_secret, f"{timestamp}.{body}")
actual   = signature from X-Dodo-Signature header (v1= part)
valid    = hmac.compare_digest(expected, actual) AND abs(now - timestamp) < 300
```

### Retry Policy

| Attempt | Delay after previous failure |
|---------|------------------------------|
| 1 | immediate |
| 2 | 10 seconds |
| 3 | 30 seconds |
| 4 | 2 minutes |
| 5 | 10 minutes |

After 5 failed attempts (~12 min 40 sec total budget), the delivery is marked permanently `failed`.

### After Exhaustion

Exhausted deliveries are stored in `webhook_deliveries` with `status = 'failed'`. Businesses can:
1. Call `GET /webhook-deliveries?status=failed` to list all failed deliveries with their payloads.
2. Re-register a working endpoint and use the payload data to reconcile missed events.
3. In a production system, we would expose a "replay delivery" endpoint — noted as a future addition.

### Decoupling from API Response

Webhook enqueuing is done via `tokio::spawn` immediately after the transaction commits. The API response is returned before any webhook HTTP call is made. The delivery worker is a separate background loop polling `webhook_deliveries` every 5 seconds. This means:
- A slow or down receiver never slows the API.
- Webhook delivery failures never affect the payment response.
- Deliveries are durable — stored in PostgreSQL and retried across restarts.

---

## 5. API Key Model

| Concern | Decision |
|---------|----------|
| **Generation** | `rand::random::<[u8; 32]>()` → hex → prefix with `dodo_` → 69-char key |
| **Storage** | Only `SHA-256(raw_key)` stored. Plaintext never persists. |
| **Display** | First 9 chars (`key_prefix`, e.g. `dodo_ab12`) stored for identification without exposing the secret |
| **Transmission** | `Authorization: Bearer <key>` over HTTPS only |
| **Rotation** | Create a new key (`POST /admin/businesses`), then revoke the old one (`PATCH /api-keys/:id/revoke`) |
| **Revocation** | Set `revoked_at = NOW()`. Auth middleware checks `revoked_at IS NULL`. Takes effect immediately on next request. |
| **Blast radius** | A leaked key gives full business-scoped access. It cannot access other businesses' data (all queries are scoped to `business_id`). Mitigation: immediate revocation + new key. |

**Why SHA-256 (not bcrypt)?** API keys are long, high-entropy random strings — they don't benefit from the slow-hashing protection bcrypt provides (which is designed for low-entropy human passwords). SHA-256 is fast, correct, and avoids unnecessary latency on every authenticated request.

---

## 6. What We Cut and Why

1. **Refunds** — Requires a `refunded` terminal state, PSP reversal API, and partial refund accounting. Complexity is disproportionate to the scope. Would add: `POST /invoices/:id/refund`, new `payment_attempts.type` column (`charge` | `refund`), and a `refunded_cents` field on the invoice.

2. **Rate limiting** — Would add per-API-key token bucket (Redis `INCR` + TTL or the `governor` crate). Left out because the assignment explicitly lists it as out of scope. Would sit as an Axum middleware layer before the auth middleware.

3. **Subscriptions / recurring billing** — Entirely separate domain model (plans, intervals, proration, dunning). Out of scope per assignment.

4. **Email notifications** — Logging `[WOULD SEND EMAIL to customer@example.com: Invoice #xyz paid]` instead of actually sending. Would use a transactional email provider (Postmark, Resend) in production.

5. **Audit log** — Every state change should be event-sourced to an `invoice_events` table in production (immutable append-only record of who changed what and when). Skipped for time but is the first thing I would add.

---

## 7. Production Readiness Gap

1. **Observability** — No metrics (Prometheus counters for `payment.attempted`, `payment.succeeded`, `webhook.delivered`), no distributed tracing (OpenTelemetry spans), no structured log correlation IDs. In production, you cannot debug payment failures without this.

2. **Rate limiting** — The API has no per-key request throttling. A misbehaving or compromised client could exhaust DB connections or PSP quotas. Would implement token-bucket rate limiting per `business_id` using Redis.

3. **Idempotency TTL / cleanup** — `payment_attempts` and `webhook_deliveries` grow forever. In production, rows older than 90 days should be archived to cold storage and deleted from the hot table. A background job or pg_partman partition rotation would handle this.
