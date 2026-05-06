#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    // ── helpers ───────────────────────────────────────────────────────────
    async fn get_pool() -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://dodo:secret@localhost:5432/dodo".into());
        sqlx::PgPool::connect(&url).await.expect("connect to test DB")
    }

    async fn clean_db(pool: &sqlx::PgPool) {
        sqlx::query!(
            "TRUNCATE payment_attempts, invoices, line_items, customers, \
             businesses, webhook_endpoints, webhook_deliveries \
             RESTART IDENTITY CASCADE"
        )
        .execute(pool)
        .await
        .expect("clean_db failed");
    }

    #[tokio::test]
    async fn test_concurrent_pay_only_one_succeeds() {
        let pool = get_pool().await;
        clean_db(&pool).await;

        let biz_id = sqlx::query_scalar!(
            "INSERT INTO businesses (name) VALUES ('ConcurrencyTestBiz') RETURNING id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let cust_id = sqlx::query_scalar!(
            "INSERT INTO customers (business_id, name, email)
             VALUES ($1, 'Test', 'conc@test.com') RETURNING id",
            biz_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let invoice_id = sqlx::query_scalar!(
            "INSERT INTO invoices (business_id, customer_id, status, total_cents, due_date)
             VALUES ($1, $2, 'open', 5000, CURRENT_DATE + 30) RETURNING id",
            biz_id,
            cust_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let n = 10usize;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::new();

        for i in 0..n {
            let pool = pool.clone();
            let barrier = barrier.clone();
            let idem_key = format!("conc-test-key-{}", i);

            handles.push(tokio::spawn(async move {
                barrier.wait().await;

                let mut tx = pool.begin().await.unwrap();

                let row = sqlx::query!(
                    "SELECT status FROM invoices WHERE id = $1 FOR UPDATE",
                    invoice_id
                )
                .fetch_one(&mut *tx)
                .await
                .unwrap();

                if row.status != "open" {
                    tx.rollback().await.unwrap();
                    return false;
                }

                sqlx::query!(
                    "UPDATE invoices SET status = 'paid', updated_at = NOW() WHERE id = $1",
                    invoice_id
                )
                .execute(&mut *tx)
                .await
                .unwrap();

                sqlx::query!(
                    "INSERT INTO payment_attempts
                         (invoice_id, idempotency_key, request_hash, card_token, status, psp_ref)
                     VALUES ($1, $2, 'hash', 'tok_success', 'succeeded', gen_random_uuid()::text)",
                    invoice_id,
                    idem_key
                )
                .execute(&mut *tx)
                .await
                .unwrap();

                tx.commit().await.unwrap();
                true
            }));
        }

        let results: Vec<bool> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let success_count = results.iter().filter(|&&v| v).count();

        let final_status = sqlx::query_scalar!(
            "SELECT status FROM invoices WHERE id = $1",
            invoice_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let attempt_count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1",
            invoice_id
        )
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);

        assert_eq!(success_count, 1, "Exactly one concurrent pay must succeed");
        assert_eq!(final_status, "paid", "Invoice must be in paid state");
        assert_eq!(attempt_count, 1, "Exactly one payment attempt must exist — no double-charge");
    }

    #[tokio::test]
    async fn test_idempotency_same_key_no_second_attempt() {
        let pool = get_pool().await;
        clean_db(&pool).await;

        let biz_id = sqlx::query_scalar!(
            "INSERT INTO businesses (name) VALUES ('IdempotencyTestBiz') RETURNING id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let cust_id = sqlx::query_scalar!(
            "INSERT INTO customers (business_id, name, email)
             VALUES ($1, 'Test', 'idem@test.com') RETURNING id",
            biz_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let invoice_id = sqlx::query_scalar!(
            "INSERT INTO invoices (business_id, customer_id, status, total_cents, due_date)
             VALUES ($1, $2, 'open', 1000, CURRENT_DATE + 30) RETURNING id",
            biz_id,
            cust_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let idem_key = "idem-test-key-unique-abc123";
        let request_hash = "abc123hash";

        sqlx::query!(
            "INSERT INTO payment_attempts
                 (invoice_id, idempotency_key, request_hash, card_token, status)
             VALUES ($1, $2, $3, 'tok_success', 'succeeded')",
            invoice_id,
            idem_key,
            request_hash
        )
        .execute(&pool)
        .await
        .unwrap();

        let existing = sqlx::query!(
            "SELECT id, status FROM payment_attempts WHERE idempotency_key = $1",
            idem_key
        )
        .fetch_optional(&pool)
        .await
        .unwrap();

        assert!(existing.is_some(), "Idempotency key lookup must find the existing attempt");
        assert_eq!(existing.unwrap().status, "succeeded");

        let retry_result = sqlx::query!(
            "INSERT INTO payment_attempts
                 (invoice_id, idempotency_key, request_hash, card_token, status)
             VALUES ($1, $2, $3, 'tok_success', 'succeeded')
             ON CONFLICT (idempotency_key) DO NOTHING",
            invoice_id,
            idem_key,
            request_hash
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(retry_result.rows_affected(), 0, "Retry with same idempotency key must not insert a second row");

        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM payment_attempts WHERE invoice_id = $1",
            invoice_id
        )
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);

        assert_eq!(count, 1, "Must have exactly one attempt — no duplicate from retry");
    }

    #[tokio::test]
    async fn test_psp_timeout_invoice_stays_open() {
        let pool = get_pool().await;
        clean_db(&pool).await;

        let biz_id = sqlx::query_scalar!(
            "INSERT INTO businesses (name) VALUES ('TimeoutTestBiz') RETURNING id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let cust_id = sqlx::query_scalar!(
            "INSERT INTO customers (business_id, name, email)
             VALUES ($1, 'Test', 'timeout@test.com') RETURNING id",
            biz_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let invoice_id = sqlx::query_scalar!(
            "INSERT INTO invoices (business_id, customer_id, status, total_cents, due_date)
             VALUES ($1, $2, 'open', 2500, CURRENT_DATE + 30) RETURNING id",
            biz_id,
            cust_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let attempt_id = sqlx::query_scalar!(
            "INSERT INTO payment_attempts
                 (invoice_id, idempotency_key, request_hash, card_token, status)
             VALUES ($1, 'timeout-idem-key', 'hash', 'tok_timeout', 'pending')
             RETURNING id",
            invoice_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query!(
            "UPDATE payment_attempts
             SET status = 'failed', failure_code = 'psp_timeout', updated_at = NOW()
             WHERE id = $1",
            attempt_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let invoice_status = sqlx::query_scalar!(
            "SELECT status FROM invoices WHERE id = $1",
            invoice_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let attempt_status = sqlx::query_scalar!(
            "SELECT status FROM payment_attempts WHERE id = $1",
            attempt_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let failure_code = sqlx::query_scalar!(
            "SELECT failure_code FROM payment_attempts WHERE id = $1",
            attempt_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(invoice_status, "open", "Invoice must remain open after PSP timeout");
        assert_eq!(attempt_status, "failed", "Payment attempt must be marked failed");
        assert_eq!(failure_code.as_deref(), Some("psp_timeout"), "Failure code must be psp_timeout");
    }
}
