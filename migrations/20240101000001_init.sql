-- Enable UUID generation
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- Businesses
CREATE TABLE businesses (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- API Keys (hashed storage — never store plaintext)
CREATE TABLE api_keys (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    key_hash    TEXT NOT NULL UNIQUE,    -- SHA-256 of the raw key
    key_prefix  TEXT NOT NULL,           -- First 8 chars for display (e.g. "dodo_ab12")
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at  TIMESTAMPTZ             -- NULL = active
);
CREATE INDEX idx_api_keys_hash ON api_keys(key_hash);
CREATE INDEX idx_api_keys_business ON api_keys(business_id);

-- Customers (scoped to a business)
CREATE TABLE customers (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    email       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (business_id, email)
);
CREATE INDEX idx_customers_business ON customers(business_id);

-- Invoices
CREATE TABLE invoices (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id     UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    customer_id     UUID NOT NULL REFERENCES customers(id),
    status          TEXT NOT NULL DEFAULT 'draft'
                        CHECK (status IN ('draft','open','paid','void','uncollectible')),
    total_cents     BIGINT NOT NULL DEFAULT 0 CHECK (total_cents >= 0),  -- integer cents only
    due_date        DATE NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_invoices_business ON invoices(business_id);
CREATE INDEX idx_invoices_status ON invoices(status);
CREATE INDEX idx_invoices_customer ON invoices(customer_id);

-- Line Items (computed total stored on invoice — server always computes)
CREATE TABLE line_items (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id          UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    description         TEXT NOT NULL,
    quantity            INTEGER NOT NULL CHECK (quantity > 0),
    unit_amount_cents   BIGINT NOT NULL CHECK (unit_amount_cents >= 0),
    amount_cents        BIGINT NOT NULL CHECK (amount_cents >= 0)  -- quantity * unit_amount_cents
);
CREATE INDEX idx_line_items_invoice ON line_items(invoice_id);

-- Payment Attempts
CREATE TABLE payment_attempts (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id          UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    idempotency_key     TEXT NOT NULL UNIQUE,
    request_hash        TEXT NOT NULL,   -- SHA-256 of request body — detect body mismatch on reuse
    card_token          TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending','succeeded','failed')),
    psp_ref             TEXT,            -- Reference from PSP on success
    failure_code        TEXT,            -- e.g. "card_declined", "psp_timeout"
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_payment_attempts_invoice ON payment_attempts(invoice_id);
CREATE INDEX idx_payment_attempts_idempotency ON payment_attempts(idempotency_key);

-- Webhook Endpoints registered by businesses
CREATE TABLE webhook_endpoints (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id UUID NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    secret      TEXT NOT NULL,   -- HMAC signing secret (stored, used for HMAC-SHA256)
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_webhook_endpoints_business ON webhook_endpoints(business_id);

-- Webhook Delivery log
CREATE TABLE webhook_deliveries (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    endpoint_id     UUID NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,
    event_type      TEXT NOT NULL,       -- e.g. "invoice.created", "invoice.paid"
    payload         JSONB NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending','delivered','failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_webhook_deliveries_status ON webhook_deliveries(status, next_attempt_at);
CREATE INDEX idx_webhook_deliveries_endpoint ON webhook_deliveries(endpoint_id);
