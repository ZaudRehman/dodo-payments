# DESIGN.md — Invoice & Payment Service

***

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
| key_hash | TEXT UNIQUE | SHA-256 of raw key, plaintext never stored |
| key_prefix | TEXT | First 9 chars (e.g. `dodo_ab12`) for display only |
| created_at | TIMESTAMPTZ | |
| revoked_at | TIMESTAMPTZ NULL | NULL means active |

Index: `api_keys(key_hash)`. Every authenticated request hashes the Bearer token and hits this index. Fast lookup, no plaintext stored.

**customers**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | A customer belongs to one business |
| name | TEXT | |
| email | TEXT | |
| created_at | TIMESTAMPTZ | |

Unique constraint on `(business_id, email)`. Two businesses can share a customer email. Within the same business, duplicates are rejected.

**invoices**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | |
| customer_id | UUID FK | |
| status | TEXT | `open`, `paid`, `void`, `uncollectible` |
| total_cents | BIGINT | Integer cents, no floats anywhere |
| due_date | DATE | |
| created_at / updated_at | TIMESTAMPTZ | |

Indexes on `invoices(business_id)`, `invoices(status)`, and `invoices(customer_id)`. The status index matters for list-by-state queries. Without it, filtering open invoices across a large business is a full scan.

**line_items**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| invoice_id | UUID FK | Cascade delete with invoice |
| description | TEXT | |
| quantity | INTEGER | CHECK > 0 |
| unit_amount_cents | BIGINT | CHECK >= 0 |
| amount_cents | BIGINT | `quantity * unit_amount_cents`, server-computed |

The server always computes `amount_cents` and `total_cents`. Any client-supplied total is ignored. Trusting the client to do money math is the kind of bug that causes real financial damage.

**payment_attempts**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| invoice_id | UUID FK | |
| idempotency_key | TEXT UNIQUE | Deduplication key |
| request_hash | TEXT | SHA-256 of request body, detects key reuse with a different body |
| card_token | TEXT | |
| status | TEXT | `pending`, `succeeded`, `failed` |
| psp_ref | TEXT NULL | PSP reference on success |
| failure_code | TEXT NULL | e.g. `card_declined`, `psp_timeout` |
| created_at / updated_at | TIMESTAMPTZ | |

The `idempotency_key` unique index is hit on every pay request. The `request_hash` exists for one reason: if a caller reuses the same key with a different request body, that is a caller bug and should be rejected, not silently processed.

**webhook_endpoints**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| business_id | UUID FK | Scoped to one business |
| url | TEXT | Receiver endpoint URL |
| secret | TEXT | HMAC signing secret, generated at registration |
| active | BOOLEAN | Default true; set false to disable without deleting |
| created_at | TIMESTAMPTZ | |

**webhook_deliveries**

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| endpoint_id | UUID FK | References `webhook_endpoints` |
| event_type | TEXT | e.g. `invoice.created`, `invoice.paid`, `invoice.payment_failed` |
| payload | JSONB | Full event body stored for replay and reconciliation |
| status | TEXT | `pending`, `delivered`, `failed` |
| attempts | INTEGER | Incremented on each delivery attempt |
| next_attempt_at | TIMESTAMPTZ | NULL after terminal state |
| last_error | TEXT NULL | Last HTTP error or timeout message |
| created_at | TIMESTAMPTZ | |

Index on `webhook_deliveries(status, next_attempt_at)`. The background worker polls this index to find deliveries due for retry.

### Why UUIDs?

UUIDs as primary keys prevent enumeration. With sequential integers, an attacker who knows invoice `1001` exists can probe `1002`, `1003`, and so on. UUIDs remove that.

The downside is slightly worse index locality compared to integers. At this scale that is an acceptable trade-off, and most hot queries filter by `business_id` anyway.

### At 100x Scale

- Partition `invoices` and `payment_attempts` by `business_id`. Most queries are already scoped to one business, so partition pruning would give a lot back.
- Add read replicas. Route all `GET` queries there and keep writes on primary.
- Replace the DB polling webhook worker with a proper queue (SQS, NATS, or Postgres LISTEN/NOTIFY). Polling every 5 seconds works fine at low volume but does not scale cleanly.
- Move API key lookup to Redis if auth latency becomes a hot path, to avoid a DB round-trip on every request.

***

## 2. Invoice State Machine

```text
  [CREATE INVOICE]
         |
         v
       open
         |
         |-- POST /pay, PSP succeeds -------> paid          [TERMINAL]
         |
         |-- POST /void --------------------> void          [TERMINAL]
         |
         +-- manual mark ------------------> uncollectible  [TERMINAL]
```

Invoices are created directly in `open`. A `draft` state was considered for invoices not yet sent to the customer, but it adds a transition and a guard without being required by the spec. It is documented below as an extension, not implemented in the code.

**All valid transitions:**

| From | To | Trigger |
|------|----|---------|
| `draft` | `open` | Invoice finalized or sent to customer |
| `draft` | `void` | Invoice voided before sending |
| `open` | `paid` | `POST /invoices/:id/pay` succeeds |
| `open` | `void` | `POST /invoices/:id/void` |
| `open` | `uncollectible` | Manual mark after repeated failed attempts |

**Terminal states:** `paid`, `void`, `uncollectible`.

Nothing transitions out of these. Once an invoice reaches a terminal state, the system does not move it anywhere else. If a paid invoice needs to be undone, that is a refund modeled as a separate action, not a reverse transition back to `open`.

**Reversible transitions:** None. All transitions are one-way by design.

**Invalid transitions at the API level:**

Any `POST /pay` on a non-`open` invoice returns `409 Conflict` before any PSP call or payment attempt insert:

```json
{
  "error": {
    "code": "invalid_state_transition",
    "current": "paid",
    "attempted": "pay"
  }
}
```

***

## 3. Payment Correctness & Failure Modes

This was the hardest part of the assignment. The happy path is straightforward. The real work is in what happens when requests race, the PSP is slow, or the process dies at the wrong time.

### (a) Two clients POST /pay for the same invoice simultaneously

Both requests hit the handler at almost the same time.

**Mechanism:** `SELECT ... FOR UPDATE` on the invoice row inside a transaction.

The first request acquires the row lock and proceeds through the PSP call and status update. The second request blocks on the same row. Once the first transaction commits, the second request reads `status = "paid"`, hits the state machine guard, and returns `409 Conflict`.

**Why this over the alternatives:**

- *Optimistic concurrency* works well for low-contention updates. Payments on the same invoice are not low contention. You would get a lot of spurious retries under load.
- *Advisory locks* are session-scoped and interact awkwardly with connection pools. If the app crashes mid-lock, cleanup depends on the session being cleaned up, which is not guaranteed.
- *Serializable isolation* would also be correct, but it requires the application to handle serialization failures with retry logic. That felt heavier than necessary for this access pattern.

**Outcome:** At most one payment succeeds. The others lose the race cleanly with a `409`.

### (b) PSP timeout (`tok_timeout`, 30 seconds)

The service uses a 10-second timeout on the PSP call. The `payment_attempt` row is inserted with `status = "pending"` and committed **before** calling the PSP. This is the key invariant: every payment attempt is on record before any external call is made.

What happens step by step:

1. Insert a `payment_attempt` row with `status = "pending"` and commit it before calling the PSP.
2. Call the PSP wrapped in a 10-second timeout.
3. Timeout fires.
4. Update the attempt to `status = "failed"`, `failure_code = "psp_timeout"`.
5. Invoice stays in `open`. The pending attempt remains in the audit trail — it is not deleted.

The caller gets back a failed attempt and can retry with a new `Idempotency-Key`. The original pending-then-failed record stays on record so the full history of attempts against that invoice is preserved.

`tok_timeout` is handled identically to `tok_network_error` from the caller's perspective — a dropped connection and a timeout both result in a `failed` attempt with an appropriate `failure_code` and the invoice left in `open`.

### (c) PSP returns success but the service crashes before persisting

This is the worst failure mode.

The `payment_attempt` is inserted as `pending` before the PSP call (see section b). If the service crashes after the PSP returns success but before the status update is written, the attempt is left in `pending` on restart.

On retry with the same `Idempotency-Key`, the service finds the existing `pending` attempt and re-calls the PSP. With the mock PSP, `tok_success` always succeeds, so the retry works correctly.

In a real system the correct fix is to forward the same `Idempotency-Key` to the PSP's charge API. PSPs like Stripe deduplicate on their side, so a retry with the same key returns the original result without a second charge. The mock does not implement this, but it is the documented requirement for any real integration.

Local idempotency (our `idempotency_key` column) protects the app layer. PSP-side idempotency protects against double charges across crash-recovery boundaries.

### (d) Idempotency key reused with a different request body

Every pay request stores a `request_hash` (SHA-256 of the request body). On reuse, the incoming hash is compared against the stored one. If they differ, the service returns `422 Unprocessable Entity`:

```json
{"error": {"code": "idempotency_conflict"}}
```

Reusing a key with a different body is not a retry. It is a contract violation. Silently processing the new body — especially if it carries a different `card_token` — would undermine the entire idempotency guarantee.

### (e) POST /pay on an already-paid invoice

The row lock is acquired, the invoice status is read as `paid`, and the handler returns `409` immediately. The PSP is never called and no new payment attempt is inserted. Inserting a spurious attempt would muddy the audit trail.

### Concurrency mechanism summary

Row-level locking with `SELECT ... FOR UPDATE`. It fits here because:

1. The contention pattern is predictable.
2. The invariant is simple: only one transition to `paid` wins.
3. Losers get a clear error immediately, no retry logic needed on the caller side.

***

## 4. Webhook Design

### Implemented Events

Three event types are implemented: `invoice.created`, `invoice.paid`, and `invoice.payment_failed`. All three are fired from within the payment and invoice handler paths, enqueued via `tokio::spawn` after the transaction commits.

### Signing Scheme

Every delivery includes:

- `X-Dodo-Timestamp: <unix_seconds>`
- `X-Dodo-Signature: t=<timestamp>,v1=<hex_hmac>`

The signature is `HMAC-SHA256(endpoint_secret, "<timestamp>.<raw_body>")`.

Including the timestamp in the signed payload means an attacker cannot replay a captured signature — the timestamp would be stale and a correctly implemented receiver would reject it.

**Replay protection:**

Receivers should reject deliveries where `|now - timestamp| > 300` seconds. This is a receiver-side responsibility, documented as part of the contract rather than enforced server-side, because the server has no way to know whether a delivery is a replay from the receiver's perspective.

**Receiver verification:**

```text
expected = HMAC-SHA256(endpoint_secret, "{timestamp}.{body}")
actual   = v1 value from X-Dodo-Signature
valid    = hmac.compare_digest(expected, actual) AND abs(now - timestamp) < 300
```

A Stripe-style signing format is used because it is widely understood and straightforward to implement on the receiver side.

### Retry Policy

| Attempt | Delay after previous failure |
|---------|------------------------------|
| 1 | Immediate |
| 2 | 10 seconds |
| 3 | 30 seconds |
| 4 | 2 minutes |
| 5 | 10 minutes |
| 6 | 1 hour |

Total retry window is a little over 1 hour across 6 attempts. After the 6th failure the delivery is marked `failed` and no further retries happen.

Webhook delivery is DB-backed polling rather than a queue because it was the simplest mechanism that survives process restarts and fit within the time budget. A proper queue (SQS, NATS, Postgres LISTEN/NOTIFY) would be the next step at higher volume.

### After Exhaustion

Failed deliveries stay in `webhook_deliveries` with the full payload and last error intact. A business can query `GET /webhook-deliveries?status=failed` to see every delivery that did not make it through. To reconcile, they compare that log against their own event records to identify which state changes they missed, then use the stored payload on each delivery to replay the event. A `POST /webhook-deliveries/:id/replay` endpoint would be the obvious next addition to make this self-service.

### Decoupling from the API Response

Webhook enqueuing happens via `tokio::spawn` right after the payment transaction commits. The API response goes back to the caller before any outbound HTTP is attempted. The delivery worker is a background loop polling the database every 5 seconds.

This means a slow or unreachable receiver never affects API latency, and a delivery failure never changes the payment response the caller sees. Because deliveries are written to Postgres before any outbound HTTP is attempted, they also survive process restarts.

***

## 5. API Key Model

| Concern | Decision |
|---------|----------|
| **Generation** | 32 random bytes via `rand`, hex-encoded, prefixed with `dodo_` |
| **Storage** | Only `SHA-256(raw_key)` stored. Plaintext never persists after the creation response. |
| **Display** | Short prefix (`dodo_ab12`) stored for identification without exposing the secret |
| **Transmission** | `Authorization: Bearer <key>` over HTTPS |
| **Rotation** | Create a new key, update integrations, then revoke the old one |
| **Revocation** | `revoked_at = NOW()`. Auth middleware checks `revoked_at IS NULL`. Takes effect immediately on the next request. |
| **Blast radius** | A leaked key exposes one business. All queries are scoped to `business_id`, so other businesses are not affected. |

**Why SHA-256 and not bcrypt?**

bcrypt is designed for low-entropy secrets like human passwords. These keys are 32 random bytes — roughly 256 bits of entropy. Brute forcing that is not a realistic attack regardless of hash speed. Using bcrypt here would add 100–300ms of unnecessary latency to every authenticated request. SHA-256 is the right choice for high-entropy secrets.

***

## 6. What I Cut and Why

**Refunds**

Refunds require a new terminal state, a PSP reversal call, and careful accounting around partial vs full amounts. If adding this: `POST /invoices/:id/refund` with an optional `amount_cents` field, a `type` column on `payment_attempts` to distinguish charges from refunds, and a `refunded` terminal state on the invoice. The complexity is real and it is out of scope for this assignment.

**Rate limiting**

The assignment listed this as explicitly out of scope. In production this would be a token-bucket middleware layer keyed by `business_id`, backed by Redis for cross-instance enforcement. Without it, a compromised key can exhaust the DB connection pool or hit PSP rate limits.

**Subscriptions and recurring billing**

This is a different system entirely, not just another endpoint. It needs plans, billing intervals, proration logic, dunning, and scheduled jobs. Adding it here would have been fake complexity without addressing what the assignment was actually testing.

**Email notifications**

A "would send email" log message is used instead of wiring in a real provider. The integration would add setup ceremony without changing anything about the payment correctness questions being evaluated.

**Audit log**

This is the most significant deliberate omission. In production an append-only `invoice_events` table recording every state transition, the actor, and the timestamp would be essential. Without it, debugging a disputed payment or handling a compliance review is much harder than it needs to be. The current `updated_at` timestamps on `invoices` and `payment_attempts` are a weak substitute.

***

## 7. Production Readiness Gap

If this shipped tomorrow, these are the three things most worth worrying about.

**1. Observability**

There are structured logs but no metrics, no distributed tracing, and no correlation IDs on requests. Debugging a payment failure in production without those means grepping logs and hoping the relevant lines are there. The fix: Prometheus counters for payment outcomes and PSP errors, OpenTelemetry spans across the request path, and a request ID propagated through every log line.

**2. Rate limiting**

Nothing currently stops a bad client, or a caller with a leaked API key, from hammering the service. Per-key token bucket limiting in middleware would protect both the database and the PSP dependency from being exhausted by a single misbehaving caller.

**3. Data retention**

`payment_attempts` and `webhook_deliveries` grow indefinitely. That is fine for an assignment but not for a real service. At any real volume, query performance degrades and storage costs climb within months. A background archival job or time-based partitioning with `pg_partman` would handle this before it becomes a problem.