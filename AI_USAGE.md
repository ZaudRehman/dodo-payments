# AI_USAGE.md

## Tools Used

- **Perplexity AI (Claude Sonnet)** — Used to scaffold the initial project structure, boilerplate handler code, and help draft the DESIGN.md structure. Acted as a senior code reviewer asking "what did you miss?"
- **Rust Analyzer (IDE)** — Compile-time type checking throughout development.

***

## Decisions Made Independently (Against or Beyond AI Suggestions)

### 1. SELECT FOR UPDATE over Optimistic Concurrency Control

The AI initially suggested using an optimistic concurrency approach with a `version` column on the `invoices` table. I rejected this because invoice payments are a high-contention scenario (retries, network issues, customer re-submits). OCC would produce a high rate of serialization failures requiring application-level retry logic, adding complexity without benefit. `SELECT FOR UPDATE` is the correct primitive here — it serializes access at the DB level, exactly one winner, clear semantics, and no retry loop in application code. I verified this by reasoning through the concurrent payment test scenario — two simultaneous requests, no retry loop on the caller side, one clear winner.

### 2. Committing the Pending Attempt BEFORE Calling the PSP

The AI's first draft called the PSP first, then inserted the payment attempt. I reversed this order: insert the `pending` attempt (committed) → call PSP → update attempt. The reason: if the service crashes after the PSP succeeds but before we persist the result, we have no idempotency anchor. With the pending attempt stored first, a retry finds it, detects `status = "pending"`, and can re-query or re-call the PSP safely. This is a standard pattern in payment systems but the AI didn't apply it unprompted.

### 3. Separate Mock PSP Binary (Not a Route Prefix)

The AI suggested adding the mock PSP as a `/mock-psp/` route prefix in the same binary. I chose a separate binary instead because it more accurately models a real external dependency (different process, different port, can be killed independently to test `tok_network_error`). The assignment says "treat it as a real external dependency" — a separate binary honors that.

***

## Things the AI Got Wrong

### 1. Webhook signing did not include the timestamp

The AI generated the `deliver_webhook` function without including the timestamp in the signed payload. The original code signed only the raw body: `HMAC-SHA256(secret, body)`. This is incorrect because it provides no replay protection — an attacker who captures a valid webhook payload can replay it indefinitely.

I corrected this to sign `"<timestamp>.<body>"` and include the timestamp in the `X-Dodo-Signature` header as `t=<timestamp>,v1=<signature>`. This matches the Stripe webhook signing scheme, which includes the timestamp in the signed payload so replays older than 5 minutes are detectable by the receiver.

### 2. sqlx query cache did not include test binaries

The initial `cargo sqlx prepare --workspace` command was missing the `-- --all-targets` flag. This meant queries in `tests/integration_tests.rs` were not cached — only `src/` was scanned. The build failed with `SQLX_OFFLINE=true` because the test binary's queries had no cached metadata.

The fix was identifying that sqlx only scans the default target set by default, and passing `-- --all-targets` to include test binaries in the cache generation run:

```bash
DATABASE_URL=postgres://... cargo sqlx prepare --workspace -- --all-targets
```

After this, all 54 queries were cached and the build succeeded cleanly.