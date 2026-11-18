//! End-to-end tests: real requests through the router against a real PostgreSQL.
//!
//! `#[sqlx::test]` gives every test its own database with the migrations applied, so the tests run in
//! parallel and leave nothing behind. They need `DATABASE_URL` (see `.env.example`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use payment_matching_api::config::Config;
use payment_matching_api::domain::matching::MatchingConfig;
use payment_matching_api::http::{AppState, router};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789";

struct Api {
    app: Router,
    pool: PgPool,
}

struct Reply {
    status: StatusCode,
    content_type: String,
    request_id: String,
    body: Value,
}

impl Api {
    fn new(pool: PgPool) -> Self {
        let config = Config {
            database_url: String::new(),
            api_token: TOKEN.to_owned(),
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            db_max_connections: 5,
            matching: MatchingConfig::default(),
        };
        let app = router(AppState {
            pool: pool.clone(),
            config: Arc::new(config),
        });
        Self { app, pool }
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let content_type = header("content-type");
        let request_id = header("x-request-id");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        Reply {
            status,
            content_type,
            request_id,
            body,
        }
    }

    fn request(method: Method, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(
            Self::request(Method::GET, path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, path: &str, body: Value) -> Reply {
        self.post_with(path, body, &[]).await
    }

    async fn post_with(&self, path: &str, body: Value, headers: &[(&str, &str)]) -> Reply {
        let mut builder =
            Self::request(Method::POST, path).header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        self.send(builder.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn post_empty(&self, path: &str) -> Reply {
        self.send(
            Self::request(Method::POST, path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn delete(&self, path: &str) -> Reply {
        self.send(
            Self::request(Method::DELETE, path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Registers a customer and returns its id.
    async fn customer(&self, name: &str, name_kana: &str, fee_deducted: bool) -> String {
        let reply = self
            .post(
                "/v1/customers",
                json!({ "name": name, "name_kana": name_kana, "fee_deducted": fee_deducted }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        id_of(&reply.body)
    }

    /// Registers an invoice and returns its id.
    async fn invoice(
        &self,
        customer_id: &str,
        number: &str,
        amount: i64,
        due_date: &str,
    ) -> String {
        let reply = self
            .post(
                "/v1/invoices",
                json!({ "customer_id": customer_id, "invoice_number": number, "amount": amount, "due_date": due_date }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        id_of(&reply.body)
    }

    /// Imports payments dated 2026-11-10 under the given idempotency key.
    async fn import(&self, key: &str, lines: &[(&str, i64)]) -> Reply {
        let payments: Vec<Value> = lines
            .iter()
            .map(|(payer, amount)| json!({ "paid_on": "2026-11-10", "amount": amount, "payer_name": payer }))
            .collect();
        self.post_with(
            "/v1/payment-batches",
            json!({ "payments": payments }),
            &[("Idempotency-Key", key)],
        )
        .await
    }

    async fn invoice_status(&self, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM invoices WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn id_of(body: &Value) -> String {
    body["id"].as_str().expect("an id").to_owned()
}

/// The id of the single match of a payment that is not cancelled.
fn active_match_id(detail: &Value) -> String {
    let active: Vec<&Value> = detail["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["state"] != "cancelled")
        .collect();
    assert_eq!(active.len(), 1, "{detail}");
    id_of(active[0])
}

fn assert_problem(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.content_type, "application/problem+json");
    assert_eq!(reply.body["status"], status.as_u16());
    assert_eq!(reply.body["code"], code, "{}", reply.body);
    assert_eq!(reply.body["type"], "about:blank");
}

// ---------------------------------------------------------------------------------------------------------
// Health, authentication, error format
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn health_endpoints_need_no_token(pool: PgPool) {
    let api = Api::new(pool);
    for path in ["/healthz", "/readyz"] {
        let reply = api
            .send(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.body["status"], "ok");
        assert!(!reply.request_id.is_empty());
    }
}

#[sqlx::test]
async fn api_requires_the_bearer_token(pool: PgPool) {
    let api = Api::new(pool);
    let without = api
        .send(
            Request::builder()
                .uri("/v1/payments")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_problem(&without, StatusCode::UNAUTHORIZED, "unauthorized");

    for value in ["Bearer wrong-token-0123456789", "Basic dGVzdA==", TOKEN] {
        let request = Request::builder()
            .uri("/v1/payments")
            .header(header::AUTHORIZATION, value)
            .body(Body::empty())
            .unwrap();
        assert_problem(
            &api.send(request).await,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        );
    }

    assert_eq!(api.get("/v1/payments").await.status, StatusCode::OK);
}

#[sqlx::test]
async fn request_id_is_echoed_or_generated(pool: PgPool) {
    let api = Api::new(pool);
    let request = Request::builder()
        .uri("/healthz")
        .header("x-request-id", "abc-123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(api.send(request).await.request_id, "abc-123");

    // An id that could not be logged safely is replaced.
    let request = Request::builder()
        .uri("/healthz")
        .header("x-request-id", "a b\"c")
        .body(Body::empty())
        .unwrap();
    let generated = api.send(request).await.request_id;
    assert_ne!(generated, "a b\"c");
    assert_eq!(generated.len(), 36);
}

#[sqlx::test]
async fn malformed_requests_are_problems(pool: PgPool) {
    let api = Api::new(pool);

    let not_json = Api::request(Method::POST, "/v1/customers")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    assert_problem(
        &api.send(not_json).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );

    let no_content_type = Api::request(Method::POST, "/v1/customers")
        .body(Body::from("{}"))
        .unwrap();
    assert_problem(
        &api.send(no_content_type).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    );

    // Well-formed JSON of the wrong shape.
    let unknown_field = api
        .post(
            "/v1/customers",
            json!({ "name": "A", "name_kana": "エー", "nmae": "typo" }),
        )
        .await;
    assert_problem(
        &unknown_field,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
    let wrong_type = api
        .post("/v1/invoices", json!({ "customer_id": "x", "invoice_number": "1", "amount": "1", "due_date": "2026-11-30" }))
        .await;
    assert_problem(
        &wrong_type,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );

    assert_problem(
        &api.get("/v1/payments/not-a-uuid").await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_problem(
        &api.get("/v1/nothing-here").await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_problem(
        &api.get("/v1/payments/00000000-0000-7000-8000-000000000000")
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_problem(
        &api.get("/v1/payments?limit=0").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    assert_problem(
        &api.get("/v1/payments?limit=abc").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
    assert_problem(
        &api.get("/v1/payments?status=done").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
}

#[sqlx::test]
async fn oversized_body_is_rejected(pool: PgPool) {
    let api = Api::new(pool);
    let payer = "ア".repeat(400_000); // 1.2 MB of UTF-8
    let reply = api.import("big", &[(&payer, 1_000)]).await;
    assert_problem(&reply, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
}

// ---------------------------------------------------------------------------------------------------------
// Customers and invoices
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn customer_is_registered_with_its_kana_name_as_the_first_alias(pool: PgPool) {
    let api = Api::new(pool);
    let created = api
        .post(
            "/v1/customers",
            json!({ "name": " 株式会社山田商事 ", "name_kana": "カブシキガイシャ ヤマダショウジ" }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["name"], "株式会社山田商事");
    assert_eq!(created.body["fee_deducted"], false);
    assert_eq!(created.body["aliases"][0]["normalized"], "ヤマダシヨウジ");
    assert_eq!(created.body["aliases"][0]["source"], "kana");

    let id = id_of(&created.body);
    let alias = api
        .post(
            &format!("/v1/customers/{id}/aliases"),
            json!({ "alias": "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ" }),
        )
        .await;
    assert_eq!(alias.status, StatusCode::CREATED, "{}", alias.body);
    assert_eq!(alias.body["normalized"], "ヤマダオオサカシテン");
    assert_eq!(alias.body["source"], "manual");

    let fetched = api.get(&format!("/v1/customers/{id}")).await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.body["aliases"].as_array().unwrap().len(), 2);
}

#[sqlx::test]
async fn a_normalized_name_identifies_one_customer_only(pool: PgPool) {
    let api = Api::new(pool);
    let first = api
        .customer("株式会社山田商事", "カブシキガイシャ ヤマダショウジ", false)
        .await;
    let other = api
        .customer(
            "有限会社佐藤工業",
            "ユウゲンガイシャ サトウコウギョウ",
            false,
        )
        .await;

    // Another spelling of the same name.
    let duplicate = api
        .post(
            "/v1/customers",
            json!({ "name": "山田商事", "name_kana": "ﾔﾏﾀﾞｼﾖｳｼﾞ(ｶ" }),
        )
        .await;
    assert_problem(&duplicate, StatusCode::CONFLICT, "conflict");
    assert_eq!(
        api.count("customers").await,
        2,
        "the failed registration left no customer behind"
    );

    let taken = api
        .post(
            &format!("/v1/customers/{other}/aliases"),
            json!({ "alias": "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ" }),
        )
        .await;
    assert_problem(&taken, StatusCode::CONFLICT, "conflict");
    let same_owner = api
        .post(
            &format!("/v1/customers/{first}/aliases"),
            json!({ "alias": "ﾔﾏﾀﾞｼﾖｳｼﾞ" }),
        )
        .await;
    assert_problem(&same_owner, StatusCode::CONFLICT, "conflict");
}

#[sqlx::test]
async fn customer_validation(pool: PgPool) {
    let api = Api::new(pool);
    let reply = api
        .post(
            "/v1/customers",
            json!({ "name": "  ", "name_kana": "ア".repeat(201) }),
        )
        .await;
    assert_problem(
        &reply,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    assert_eq!(
        reply.body["errors"],
        json!([{ "field": "name", "code": "required" }, { "field": "name_kana", "code": "too_long" }])
    );

    let reply = api
        .post(
            "/v1/customers",
            json!({ "name": "株式会社", "name_kana": "カブシキガイシャ" }),
        )
        .await;
    assert_eq!(
        reply.body["errors"],
        json!([{ "field": "name_kana", "code": "not_normalizable" }])
    );

    let missing = "/v1/customers/00000000-0000-7000-8000-000000000000";
    assert_problem(&api.get(missing).await, StatusCode::NOT_FOUND, "not_found");
    let reply = api
        .post(&format!("{missing}/aliases"), json!({ "alias": "ヤマダ" }))
        .await;
    assert_problem(&reply, StatusCode::NOT_FOUND, "not_found");
}

#[sqlx::test]
async fn invoice_registration_and_validation(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let created = api
        .post(
            "/v1/invoices",
            json!({ "customer_id": customer, "invoice_number": "INV-001", "amount": 110_000, "due_date": "2026-11-30" }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["status"], "open");
    assert_eq!(created.body["amount"], 110_000);
    assert_eq!(created.body["due_date"], "2026-11-30");

    let duplicate = api
        .post(
            "/v1/invoices",
            json!({ "customer_id": customer, "invoice_number": "INV-001", "amount": 1, "due_date": "2026-11-30" }),
        )
        .await;
    assert_problem(&duplicate, StatusCode::CONFLICT, "conflict");

    let unknown_customer = api
        .post(
            "/v1/invoices",
            json!({
                "customer_id": "00000000-0000-7000-8000-000000000000",
                "invoice_number": "INV-002", "amount": 1, "due_date": "2026-11-30",
            }),
        )
        .await;
    assert_problem(
        &unknown_customer,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    assert_eq!(
        unknown_customer.body["errors"],
        json!([{ "field": "customer_id", "code": "not_found" }])
    );

    for amount in [0_i64, -1, 1_000_000_000_000] {
        let reply = api
            .post(
                "/v1/invoices",
                json!({ "customer_id": customer, "invoice_number": " ", "amount": amount, "due_date": "2026-11-30" }),
            )
            .await;
        assert_eq!(
            reply.body["errors"],
            json!([
                { "field": "invoice_number", "code": "required" },
                { "field": "amount", "code": "out_of_range" },
            ]),
            "amount {amount}"
        );
    }

    // An amount with a fraction is not an amount of yen.
    let fraction = api
        .post(
            "/v1/invoices",
            json!({ "customer_id": customer, "invoice_number": "INV-003", "amount": 100.5, "due_date": "2026-11-30" }),
        )
        .await;
    assert_problem(
        &fraction,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
}

#[sqlx::test]
async fn invoices_are_listed_in_pages_and_filtered(pool: PgPool) {
    let api = Api::new(pool);
    let yamada = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let sato = api
        .customer("有限会社佐藤工業", "サトウコウギョウ", false)
        .await;
    let mut ids = Vec::new();
    for n in 1..=5 {
        ids.push(
            api.invoice(&yamada, &format!("Y-{n}"), 10_000 * n, "2026-11-30")
                .await,
        );
    }
    let sato_invoice = api.invoice(&sato, "S-1", 77_000, "2026-11-30").await;

    let first = api
        .get(&format!("/v1/invoices?customer_id={yamada}&limit=2"))
        .await;
    assert_eq!(first.status, StatusCode::OK);
    let listed: Vec<String> = first.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(id_of)
        .collect();
    assert_eq!(listed, ids[..2]);
    let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
    assert_eq!(cursor, ids[1]);

    let second = api
        .get(&format!(
            "/v1/invoices?customer_id={yamada}&limit=2&cursor={cursor}"
        ))
        .await;
    let third_cursor = second.body["next_cursor"].as_str().unwrap().to_owned();
    let last = api
        .get(&format!(
            "/v1/invoices?customer_id={yamada}&limit=2&cursor={third_cursor}"
        ))
        .await;
    assert_eq!(
        last.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect::<Vec<_>>(),
        ids[4..]
    );
    assert_eq!(last.body["next_cursor"], Value::Null);

    // Clearing one invoice moves it from one status filter to the other.
    let imported = api.import("list", &[("ｻﾄｳｺｳｷﾞﾖｳ", 77_000)]).await;
    assert_eq!(imported.body["summary"]["matched"], 1);
    let cleared = api.get("/v1/invoices?status=cleared").await;
    assert_eq!(
        cleared.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect::<Vec<_>>(),
        [sato_invoice]
    );
    assert_eq!(
        api.get("/v1/invoices?status=open").await.body["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

// ---------------------------------------------------------------------------------------------------------
// Import and the automatic rules
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn r1_half_width_payer_name_is_matched_to_the_customer(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "カブシキガイシャ ヤマダショウジ", false)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;
    api.invoice(&customer, "INV-002", 55_000, "2026-12-31")
        .await;

    let imported = api.import("r1", &[("ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000)]).await;
    assert_eq!(imported.status, StatusCode::CREATED, "{}", imported.body);
    assert_eq!(imported.body["replayed"], false);
    assert_eq!(
        imported.body["summary"],
        json!({ "total": 1, "matched": 1, "suggested": 0, "unmatched": 0 })
    );

    let payment = &imported.body["payments"][0];
    assert_eq!(payment["status"], "matched");
    assert_eq!(payment["line_no"], 1);
    assert_eq!(payment["payer_name"], "ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ");
    assert_eq!(payment["payer_name_normalized"], "ヤマダシヨウジ");
    assert_eq!(payment["decision"]["outcome"], "auto_confirmed");
    assert_eq!(payment["decision"]["rule"], "R1");

    // The payment explains itself: which rule, which invoice, how much.
    let detail = api.get(&format!("/v1/payments/{}", id_of(payment))).await;
    assert_eq!(detail.status, StatusCode::OK);
    let matched = &detail.body["matches"][0];
    assert_eq!(matched["rule"], "R1");
    assert_eq!(matched["state"], "confirmed");
    assert_eq!(matched["fee_amount"], 0);
    assert!(matched["confirmed_at"].is_string());
    assert_eq!(
        matched["allocations"],
        json!([{ "invoice_id": invoice, "invoice_number": "INV-001", "invoice_amount": 110_000, "amount": 110_000 }])
    );
    assert_eq!(api.invoice_status(&invoice).await, "cleared");
}

#[sqlx::test]
async fn r2_one_payment_clears_the_only_combination_of_invoices(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let a = api
        .invoice(&customer, "INV-001", 110_000, "2026-09-30")
        .await;
    let b = api
        .invoice(&customer, "INV-002", 55_000, "2026-10-31")
        .await;
    let c = api
        .invoice(&customer, "INV-003", 33_000, "2026-11-30")
        .await;

    let imported = api.import("r2", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 165_000)]).await;
    let payment = &imported.body["payments"][0];
    assert_eq!(payment["status"], "matched");
    assert_eq!(payment["decision"]["rule"], "R2");
    assert_eq!(payment["decision"]["invoice_ids"], json!([a, b]));

    assert_eq!(api.invoice_status(&a).await, "cleared");
    assert_eq!(api.invoice_status(&b).await, "cleared");
    assert_eq!(api.invoice_status(&c).await, "open");
}

#[sqlx::test]
async fn same_amount_invoices_are_suggested_and_confirmed_by_a_person(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let newer = api
        .invoice(&customer, "INV-002", 55_000, "2026-11-30")
        .await;
    let older = api
        .invoice(&customer, "INV-001", 55_000, "2026-10-31")
        .await;

    let imported = api.import("same", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000)]).await;
    let payment = &imported.body["payments"][0];
    assert_eq!(payment["status"], "suggested");
    assert_eq!(
        payment["decision"]["reason"],
        json!({ "kind": "same_amount_invoices", "count": 2 })
    );
    assert_eq!(payment["decision"]["invoice_ids"], json!([older]));
    assert_eq!(
        api.invoice_status(&older).await,
        "open",
        "a suggestion clears nothing"
    );

    let payment_id = id_of(payment);
    let detail = api.get(&format!("/v1/payments/{payment_id}")).await;
    let match_id = active_match_id(&detail.body);
    assert_eq!(detail.body["matches"][0]["state"], "suggested");

    let confirmed = api
        .post_empty(&format!("/v1/matches/{match_id}/confirm"))
        .await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.body);
    assert_eq!(confirmed.body["status"], "matched");
    assert_eq!(confirmed.body["matches"][0]["state"], "confirmed");
    assert_eq!(api.invoice_status(&older).await, "cleared");
    assert_eq!(api.invoice_status(&newer).await, "open");

    let again = api
        .post_empty(&format!("/v1/matches/{match_id}/confirm"))
        .await;
    assert_problem(&again, StatusCode::CONFLICT, "conflict");
}

#[sqlx::test]
async fn r3_payment_short_by_the_transfer_fee_is_suggested_with_the_fee(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", true)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    let imported = api.import("r3", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 109_560)]).await;
    let payment = &imported.body["payments"][0];
    assert_eq!(payment["status"], "suggested");
    assert_eq!(payment["decision"]["rule"], "R3");
    assert_eq!(payment["decision"]["fee_amount"], 440);

    let detail = api.get(&format!("/v1/payments/{}", id_of(payment))).await;
    let confirmed = api
        .post_empty(&format!(
            "/v1/matches/{}/confirm",
            active_match_id(&detail.body)
        ))
        .await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.body);
    let matched = &confirmed.body["matches"][0];
    assert_eq!(matched["state"], "confirmed");
    assert_eq!(matched["fee_amount"], 440);
    assert_eq!(matched["allocations"][0]["amount"], 109_560);
    assert_eq!(matched["allocations"][0]["invoice_amount"], 110_000);
    assert_eq!(api.invoice_status(&invoice).await, "cleared");
}

#[sqlx::test]
async fn a_customer_that_does_not_deduct_fees_gets_no_r3(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    let imported = api.import("no-r3", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 109_560)]).await;
    let payment = &imported.body["payments"][0];
    assert_eq!(payment["status"], "unmatched");
    assert_eq!(
        payment["decision"],
        json!({ "outcome": "unmatched", "reason": "no_candidate", "subset_search_skipped": false })
    );
    assert_eq!(api.count("matches").await, 0);
}

#[sqlx::test]
async fn lines_of_a_batch_see_the_invoices_cleared_by_earlier_lines(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    // The same payment twice (a double transfer): the invoice can be cleared only once.
    let imported = api
        .import("twice", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000)])
        .await;
    assert_eq!(
        imported.body["summary"],
        json!({ "total": 2, "matched": 1, "suggested": 0, "unmatched": 1 })
    );
    assert_eq!(imported.body["payments"][0]["status"], "matched");
    assert_eq!(imported.body["payments"][1]["status"], "unmatched");
    assert_eq!(imported.body["payments"][1]["line_no"], 2);
}

#[sqlx::test]
async fn import_validation(pool: PgPool) {
    let api = Api::new(pool);

    let no_key = api
        .post("/v1/payment-batches", json!({ "payments": [] }))
        .await;
    assert_problem(&no_key, StatusCode::BAD_REQUEST, "bad_request");

    let bad_key = api
        .post_with(
            "/v1/payment-batches",
            json!({ "payments": [] }),
            &[("Idempotency-Key", "a b")],
        )
        .await;
    assert_problem(
        &bad_key,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );

    let empty = api.import("empty", &[]).await;
    assert_eq!(
        empty.body["errors"],
        json!([{ "field": "payments", "code": "out_of_range" }])
    );

    let too_many: Vec<(&str, i64)> = vec![("ﾔﾏﾀﾞ", 1); 1_001];
    assert_eq!(
        api.import("many", &too_many).await.body["errors"],
        json!([{ "field": "payments", "code": "out_of_range" }])
    );

    let long_name = "ア".repeat(201);
    let bad_lines = api
        .import(
            "bad",
            &[
                ("ﾔﾏﾀﾞ", 1_000),
                ("ﾔﾏﾀﾞ", 0),
                (&long_name, 1_000),
                ("ﾔﾏ\u{7}ﾀﾞ", 1_000),
            ],
        )
        .await;
    assert_problem(
        &bad_lines,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    assert_eq!(
        bad_lines.body["errors"],
        json!([
            { "field": "payments[1].amount", "code": "out_of_range" },
            { "field": "payments[2].payer_name", "code": "too_long" },
            { "field": "payments[3].payer_name", "code": "invalid_characters" },
        ])
    );
    assert_eq!(
        api.count("payment_batches").await,
        0,
        "a rejected request records nothing"
    );
}

// ---------------------------------------------------------------------------------------------------------
// Idempotency and atomicity
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn repeating_an_import_writes_nothing(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    let lines = [("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000), ("ﾌﾒｲ", 5_000)];
    let first = api.import("statement-2026-11-10", &lines).await;
    assert_eq!(first.status, StatusCode::CREATED);

    let second = api.import("statement-2026-11-10", &lines).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(second.body["replayed"], true);
    assert_eq!(second.body["batch_id"], first.body["batch_id"]);
    assert_eq!(second.body["payments"], first.body["payments"]);
    assert_eq!(api.count("payments").await, 2);
    assert_eq!(api.count("payment_batches").await, 1);
    assert_eq!(api.count("matches").await, 1);

    // The same payments laid out differently are the same request.
    let reordered_keys = Api::request(Method::POST, "/v1/payment-batches")
        .header(header::CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", "statement-2026-11-10")
        .body(Body::from(
            r#"{ "payments": [
                 { "payer_name": "ﾔﾏﾀﾞｼﾖｳｼﾞ", "amount": 110000, "paid_on": "2026-11-10" },
                 { "payer_name": "ﾌﾒｲ",       "amount": 5000,   "paid_on": "2026-11-10" } ] }"#,
        ))
        .unwrap();
    assert_eq!(api.send(reordered_keys).await.status, StatusCode::OK);

    // The same key for other content is refused, and nothing changes.
    let different = api
        .import("statement-2026-11-10", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_001)])
        .await;
    assert_problem(&different, StatusCode::CONFLICT, "conflict");
    assert_eq!(api.count("payments").await, 2);

    // The same content under a new key is refused as well: a statement sent twice must not be matched
    // against whatever is open by then, which here is next month's invoice of the same amount.
    let next_month = api
        .invoice(&customer, "INV-002", 110_000, "2026-12-31")
        .await;
    let new_key = api.import("statement-2026-11-11", &lines).await;
    assert_problem(&new_key, StatusCode::CONFLICT, "conflict");
    let first_batch = first.body["batch_id"].as_str().unwrap();
    assert!(
        new_key.body["detail"]
            .as_str()
            .unwrap()
            .contains(first_batch),
        "{}",
        new_key.body
    );
    assert_eq!(api.count("payments").await, 2);
    assert_eq!(api.count("payment_batches").await, 1);
    assert_eq!(api.invoice_status(&next_month).await, "open");

    // The refusal did not consume the key, and a caller who means it can say so.
    let insisted = api
        .post_with(
            "/v1/payment-batches",
            json!({
                "allow_duplicate": true,
                "payments": [
                    { "paid_on": "2026-11-10", "amount": 110_000, "payer_name": "ﾔﾏﾀﾞｼﾖｳｼﾞ" },
                    { "paid_on": "2026-11-10", "amount": 5_000, "payer_name": "ﾌﾒｲ" },
                ],
            }),
            &[("Idempotency-Key", "statement-2026-11-11")],
        )
        .await;
    assert_eq!(insisted.status, StatusCode::CREATED, "{}", insisted.body);
    assert_eq!(insisted.body["payments"][0]["status"], "matched");
    assert_eq!(api.count("payments").await, 4);
    assert_eq!(api.invoice_status(&next_month).await, "cleared");
}

#[sqlx::test]
async fn a_request_that_does_not_get_the_matching_lock_in_time_is_turned_away(
    options: PgPoolOptions,
    connect: PgConnectOptions,
) {
    // As in `main`, with a shorter wait.
    let pool = options
        .connect_with(connect.options([("lock_timeout", "200ms")]))
        .await
        .unwrap();
    let api = Api::new(pool);

    // Another request is in the middle of matching.
    let mut holder = api.pool.begin().await.unwrap();
    payment_matching_api::repo::payments::lock_matching(&mut holder)
        .await
        .unwrap();

    let reply = api.import("waits", &[("ﾌﾒｲ", 5_000)]).await;
    assert_problem(&reply, StatusCode::SERVICE_UNAVAILABLE, "busy");
    assert_eq!(api.count("payment_batches").await, 0);

    // Nothing was written, so the same request goes through once the lock is free.
    holder.rollback().await.unwrap();
    let retry = api.import("waits", &[("ﾌﾒｲ", 5_000)]).await;
    assert_eq!(retry.status, StatusCode::CREATED, "{}", retry.body);
}

#[sqlx::test]
async fn concurrent_imports_with_one_key_create_one_batch(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    let lines = [("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000)];
    let (a, b) = tokio::join!(
        api.import("same-key", &lines),
        api.import("same-key", &lines)
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CREATED]);
    assert_eq!(a.body["batch_id"], b.body["batch_id"]);
    assert_eq!(api.count("payments").await, 1);
}

#[sqlx::test]
async fn a_failed_import_leaves_nothing_behind(pool: PgPool) {
    // A constraint that exists only in this test's database makes the match of the second line fail in the
    // middle of the transaction, after the payments have been inserted and the first line has been matched.
    sqlx::query("ALTER TABLE match_allocations ADD CONSTRAINT test_reject CHECK (amount <> 666)")
        .execute(&pool)
        .await
        .unwrap();
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;
    api.invoice(&customer, "INV-002", 666, "2026-11-30").await;

    let reply = api
        .import("fails", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 666)])
        .await;
    assert_problem(&reply, StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    assert_eq!(reply.body["request_id"], reply.request_id.as_str());
    assert!(
        !reply.body.to_string().contains("test_reject"),
        "the cause stays in the log: {}",
        reply.body
    );

    for table in [
        "payment_batches",
        "payments",
        "matches",
        "match_allocations",
    ] {
        assert_eq!(api.count(table).await, 0, "{table}");
    }
    assert_eq!(api.invoice_status(&invoice).await, "open");

    // The key was not consumed by the failure.
    let retry = api.import("fails", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000)]).await;
    assert_eq!(retry.status, StatusCode::CREATED);
}

// ---------------------------------------------------------------------------------------------------------
// Confirming, manual matching, cancelling
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn an_invoice_is_cleared_once_when_two_matches_are_confirmed_at_the_same_time(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 88_000, "2026-11-30")
        .await;

    // Two payments from unknown payers, both suggested (R4) for the same invoice.
    let imported = api
        .import("race", &[("ﾌﾒｲ ｲﾁ", 88_000), ("ﾌﾒｲ ﾆ", 88_000)])
        .await;
    assert_eq!(imported.body["summary"]["suggested"], 2);
    let mut match_ids = Vec::new();
    for payment in imported.body["payments"].as_array().unwrap() {
        assert_eq!(payment["decision"]["rule"], "R4");
        let detail = api.get(&format!("/v1/payments/{}", id_of(payment))).await;
        match_ids.push(active_match_id(&detail.body));
    }

    let first = format!("/v1/matches/{}/confirm", match_ids[0]);
    let second = format!("/v1/matches/{}/confirm", match_ids[1]);
    let (a, b) = tokio::join!(api.post_empty(&first), api.post_empty(&second));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{} / {}",
        a.body,
        b.body
    );

    assert_eq!(api.invoice_status(&invoice).await, "cleared");
    let confirmed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM matches WHERE state = 'confirmed'")
            .fetch_one(&api.pool)
            .await
            .unwrap();
    assert_eq!(confirmed, 1);
    let matched = api.get("/v1/payments?status=matched").await;
    assert_eq!(matched.body["items"].as_array().unwrap().len(), 1);
    // The loser's suggestion went away with the invoice: its payment is unmatched, for a person to match to
    // something else.
    for (status, expected) in [("suggested", 0), ("unmatched", 1)] {
        let listed = api.get(&format!("/v1/payments?status={status}")).await;
        assert_eq!(
            listed.body["items"].as_array().unwrap().len(),
            expected,
            "{status}"
        );
    }
}

#[sqlx::test]
async fn clearing_an_invoice_cancels_the_suggestions_of_other_payments_that_name_it(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 88_000, "2026-11-30")
        .await;
    let other_invoice = api
        .invoice(&customer, "INV-002", 77_000, "2026-11-30")
        .await;

    // Two payments from unknown payers are suggested (R4), one for each invoice.
    let unknown = api
        .import("day-1", &[("ﾌﾒｲ ｲﾁ", 88_000), ("ﾌﾒｲ ﾆ", 77_000)])
        .await;
    assert_eq!(unknown.body["summary"]["suggested"], 2);
    let stale_payment = format!("/v1/payments/{}", id_of(&unknown.body["payments"][0]));
    let kept_payment = format!("/v1/payments/{}", id_of(&unknown.body["payments"][1]));
    let stale_match = active_match_id(&api.get(&stale_payment).await.body);

    let open = api.get(&format!("/v1/invoices/{invoice}")).await;
    assert_eq!(open.status, StatusCode::OK);
    assert_eq!(open.body["status"], "open");
    assert_eq!(open.body["cleared_by"], Value::Null);

    // The customer's own payment of the first invoice arrives and is confirmed without a person.
    let known = api.import("day-2", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 88_000)]).await;
    let payer = &known.body["payments"][0];
    assert_eq!(payer["status"], "matched");

    // The suggestion that named the invoice is cancelled and its payment is unmatched; the match stays as
    // history and can no longer be confirmed.
    let stale = api.get(&stale_payment).await.body;
    assert_eq!(stale["status"], "unmatched");
    assert_eq!(stale["matches"][0]["id"], stale_match.as_str());
    assert_eq!(stale["matches"][0]["state"], "cancelled");
    assert_eq!(stale["matches"][0]["cancel_reason"], "invoice_cleared");
    assert_problem(
        &api.post_empty(&format!("/v1/matches/{stale_match}/confirm"))
            .await,
        StatusCode::CONFLICT,
        "conflict",
    );

    // The suggestion for the other invoice is untouched.
    let kept = api.get(&kept_payment).await.body;
    assert_eq!(kept["status"], "suggested");
    assert_eq!(api.invoice_status(&other_invoice).await, "open");

    // The invoice names the payment that cleared it.
    let cleared = api.get(&format!("/v1/invoices/{invoice}")).await.body;
    assert_eq!(cleared["status"], "cleared");
    assert_eq!(cleared["invoice_number"], "INV-001");
    assert_eq!(cleared["cleared_by"]["payment_id"], payer["id"]);
    assert_eq!(cleared["cleared_by"]["rule"], "R1");
    assert_eq!(cleared["cleared_by"]["amount"], 88_000);
    assert_eq!(cleared["cleared_by"]["fee_amount"], 0);
    assert!(cleared["cleared_by"]["confirmed_at"].is_string());

    // Cancelling the match gives the invoice back.
    let cancel = format!(
        "/v1/matches/{}/cancel",
        cleared["cleared_by"]["match_id"].as_str().unwrap()
    );
    assert_eq!(api.post_empty(&cancel).await.status, StatusCode::OK);
    let reopened = api.get(&format!("/v1/invoices/{invoice}")).await.body;
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["cleared_by"], Value::Null);

    assert_problem(
        &api.get("/v1/invoices/019a0000-0000-7000-8000-000000000000")
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
}

#[sqlx::test]
async fn confirming_with_register_alias_teaches_the_payer_name(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 88_000, "2026-10-31")
        .await;
    api.invoice(&customer, "INV-002", 99_000, "2026-11-30")
        .await;

    // The statement carries the branch, which the customer master does not know.
    let first = api.import("day-1", &[("ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ ｵｵｻｶ", 88_000)]).await;
    let payment = &first.body["payments"][0];
    assert_eq!(payment["status"], "suggested");
    assert_eq!(payment["decision"]["reason"]["kind"], "payer_unknown");

    let detail = api.get(&format!("/v1/payments/{}", id_of(payment))).await;
    let confirm = format!("/v1/matches/{}/confirm", active_match_id(&detail.body));
    let confirmed = api.post(&confirm, json!({ "register_alias": true })).await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.body);

    let aliases = api.get(&format!("/v1/customers/{customer}")).await.body["aliases"].clone();
    assert_eq!(aliases[1]["normalized"], "ヤマダシヨウジオオサカ");
    assert_eq!(aliases[1]["source"], "learned");

    // The next payment under that name is recognised and matched without a person.
    let second = api.import("day-2", &[("ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ ｵｵｻｶ", 99_000)]).await;
    assert_eq!(second.body["payments"][0]["status"], "matched");
    assert_eq!(second.body["payments"][0]["decision"]["rule"], "R1");
}

#[sqlx::test]
async fn confirm_rejects_a_malformed_body(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 88_000, "2026-10-31")
        .await;
    let imported = api.import("c", &[("ﾌﾒｲ", 88_000)]).await;
    let detail = api
        .get(&format!(
            "/v1/payments/{}",
            id_of(&imported.body["payments"][0])
        ))
        .await;
    let confirm = format!("/v1/matches/{}/confirm", active_match_id(&detail.body));

    assert_problem(
        &api.post(&confirm, json!({ "register": true })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
    );
    let broken = Api::request(Method::POST, &confirm)
        .body(Body::from("{"))
        .unwrap();
    assert_problem(
        &api.send(broken).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert_problem(
        &api.post_empty("/v1/matches/00000000-0000-7000-8000-000000000000/confirm")
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
}

#[sqlx::test]
async fn cancelling_a_confirmed_match_reopens_the_invoice_and_keeps_the_history(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let wrong = api
        .invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;

    let imported = api.import("cancel", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000)]).await;
    let payment_id = id_of(&imported.body["payments"][0]);
    let detail = api.get(&format!("/v1/payments/{payment_id}")).await;
    let match_id = active_match_id(&detail.body);

    let cancelled = api
        .post_empty(&format!("/v1/matches/{match_id}/cancel"))
        .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);
    assert_eq!(cancelled.body["status"], "unmatched");
    assert_eq!(cancelled.body["matches"][0]["state"], "cancelled");
    assert_eq!(cancelled.body["matches"][0]["cancel_reason"], "reversed");
    assert!(cancelled.body["matches"][0]["cancelled_at"].is_string());
    assert_eq!(
        cancelled.body["matches"][0]["allocations"][0]["invoice_id"],
        wrong.as_str()
    );
    assert_eq!(api.invoice_status(&wrong).await, "open");

    let again = api
        .post_empty(&format!("/v1/matches/{match_id}/cancel"))
        .await;
    assert_problem(&again, StatusCode::CONFLICT, "conflict");

    // The payment really belonged to another invoice of the same amount, registered late.
    let right = api
        .invoice(&customer, "INV-000", 110_000, "2026-10-31")
        .await;
    let manual = api
        .post(
            &format!("/v1/payments/{payment_id}/matches"),
            json!({ "invoice_ids": [right] }),
        )
        .await;
    assert_eq!(manual.status, StatusCode::CREATED, "{}", manual.body);
    assert_eq!(manual.body["status"], "matched");
    let history = manual.body["matches"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        (&history[0]["state"], &history[0]["rule"]),
        (&json!("cancelled"), &json!("R1"))
    );
    assert_eq!(
        (&history[1]["state"], &history[1]["rule"]),
        (&json!("confirmed"), &json!("manual"))
    );
    assert_eq!(api.invoice_status(&right).await, "cleared");
    assert_eq!(api.invoice_status(&wrong).await, "open");
}

#[sqlx::test]
async fn dismissing_a_suggestion_leaves_the_payment_unmatched(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let invoice = api
        .invoice(&customer, "INV-001", 88_000, "2026-11-30")
        .await;
    let imported = api.import("dismiss", &[("ﾌﾒｲ", 88_000)]).await;
    let payment_id = id_of(&imported.body["payments"][0]);
    let match_id = active_match_id(&api.get(&format!("/v1/payments/{payment_id}")).await.body);

    let dismissed = api
        .post_empty(&format!("/v1/matches/{match_id}/cancel"))
        .await;
    assert_eq!(dismissed.status, StatusCode::OK);
    assert_eq!(dismissed.body["status"], "unmatched");
    assert_eq!(dismissed.body["matches"][0]["cancel_reason"], "dismissed");
    assert_eq!(api.invoice_status(&invoice).await, "open");
    assert_problem(
        &api.post_empty(&format!("/v1/matches/{match_id}/confirm"))
            .await,
        StatusCode::CONFLICT,
        "conflict",
    );
}

#[sqlx::test]
async fn manual_match_with_a_fee_replaces_the_suggestion(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let a = api
        .invoice(&customer, "INV-001", 110_000, "2026-10-31")
        .await;
    let b = api
        .invoice(&customer, "INV-002", 55_000, "2026-11-30")
        .await;
    let decoy = api
        .invoice(&customer, "INV-003", 164_340, "2026-12-31")
        .await;

    // R1 confirms the decoy automatically; the person knows better, cancels and matches by hand with a fee
    // the customer was not expected to deduct.
    let imported = api
        .import("manual", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 164_340), ("ﾌﾒｲ", 164_340)])
        .await;
    let auto_payment = id_of(&imported.body["payments"][0]);
    let auto_match = active_match_id(&api.get(&format!("/v1/payments/{auto_payment}")).await.body);
    assert_eq!(
        api.post_empty(&format!("/v1/matches/{auto_match}/cancel"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(api.invoice_status(&decoy).await, "open");

    let manual = api
        .post(
            &format!("/v1/payments/{auto_payment}/matches"),
            json!({ "invoice_ids": [b, a], "fee_amount": 660 }),
        )
        .await;
    assert_eq!(manual.status, StatusCode::CREATED, "{}", manual.body);
    let created = manual.body["matches"].as_array().unwrap().last().unwrap();
    assert_eq!(created["rule"], "manual");
    assert_eq!(created["fee_amount"], 660);
    // Oldest first, whatever the order of the request; the fee comes off the oldest.
    assert_eq!(
        created["allocations"],
        json!([
            { "invoice_id": a, "invoice_number": "INV-001", "invoice_amount": 110_000, "amount": 109_340 },
            { "invoice_id": b, "invoice_number": "INV-002", "invoice_amount": 55_000, "amount": 55_000 },
        ])
    );

    // The second payment had no suggestion (the decoy was taken when it was imported); match it to the decoy.
    let other_payment = id_of(&imported.body["payments"][1]);
    assert_eq!(imported.body["payments"][1]["status"], "unmatched");
    let other = api
        .post(
            &format!("/v1/payments/{other_payment}/matches"),
            json!({ "invoice_ids": [decoy] }),
        )
        .await;
    assert_eq!(other.status, StatusCode::CREATED, "{}", other.body);
    for invoice in [&a, &b, &decoy] {
        assert_eq!(api.invoice_status(invoice).await, "cleared");
    }
}

#[sqlx::test]
async fn manual_match_cancels_an_open_suggestion(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let older = api
        .invoice(&customer, "INV-001", 55_000, "2026-10-31")
        .await;
    let newer = api
        .invoice(&customer, "INV-002", 55_000, "2026-11-30")
        .await;

    let imported = api.import("override", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000)]).await;
    let payment_id = id_of(&imported.body["payments"][0]);
    assert_eq!(
        imported.body["payments"][0]["decision"]["invoice_ids"],
        json!([older])
    );

    // The person picks the other invoice instead of the suggested one.
    let manual = api
        .post(
            &format!("/v1/payments/{payment_id}/matches"),
            json!({ "invoice_ids": [newer] }),
        )
        .await;
    assert_eq!(manual.status, StatusCode::CREATED, "{}", manual.body);
    let history = manual.body["matches"].as_array().unwrap();
    assert_eq!(history[0]["state"], "cancelled");
    assert_eq!(history[0]["cancel_reason"], "replaced");
    assert_eq!(history[1]["state"], "confirmed");
    assert_eq!(history[1]["cancel_reason"], Value::Null);
    assert_eq!(api.invoice_status(&older).await, "open");
    assert_eq!(api.invoice_status(&newer).await, "cleared");
}

#[sqlx::test]
async fn manual_match_validation(pool: PgPool) {
    let api = Api::new(pool);
    let yamada = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let sato = api
        .customer("有限会社佐藤工業", "サトウコウギョウ", false)
        .await;
    let y1 = api.invoice(&yamada, "Y-1", 60_000, "2026-10-31").await;
    let y2 = api.invoice(&yamada, "Y-2", 40_000, "2026-11-30").await;
    let s1 = api.invoice(&sato, "S-1", 40_000, "2026-11-30").await;
    let s2 = api.invoice(&sato, "S-2", 45_000, "2026-11-30").await;
    let tiny = api.invoice(&yamada, "Y-3", 300, "2026-09-30").await;

    let imported = api
        .import("v", &[("ﾌﾒｲ", 100_000), ("ｻﾄｳｺｳｷﾞﾖｳ", 40_000)])
        .await;
    let payment = id_of(&imported.body["payments"][0]);
    let path = format!("/v1/payments/{payment}/matches");
    let errors = |field: &str, code: &str| json!([{ "field": field, "code": code }]);

    let cases = [
        (
            json!({ "invoice_ids": [] }),
            errors("invoice_ids", "out_of_range"),
        ),
        (
            json!({ "invoice_ids": [y1, y1] }),
            errors("invoice_ids", "duplicate"),
        ),
        (
            json!({ "invoice_ids": [y1], "fee_amount": -1 }),
            errors("fee_amount", "out_of_range"),
        ),
        (
            json!({ "invoice_ids": [y1, "00000000-0000-7000-8000-000000000000"] }),
            errors("invoice_ids", "not_found"),
        ),
        (
            json!({ "invoice_ids": [y1, s2] }),
            errors("invoice_ids", "different_customers"),
        ),
        (
            json!({ "invoice_ids": [y1] }),
            errors("invoice_ids", "amount_mismatch"),
        ),
        (
            json!({ "invoice_ids": [y1, y2], "fee_amount": 440 }),
            errors("invoice_ids", "amount_mismatch"),
        ),
        // The fee comes off the oldest invoice, which is too small for it.
        (
            json!({ "invoice_ids": [tiny, y1, y2], "fee_amount": 300 }),
            errors("fee_amount", "out_of_range"),
        ),
    ];
    for (body, expected) in cases {
        let reply = api.post(&path, body.clone()).await;
        assert_problem(
            &reply,
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        );
        assert_eq!(reply.body["errors"], expected, "{body}");
    }
    assert_eq!(
        api.count("matches").await,
        1,
        "only the automatic match of the second line exists"
    );

    // s1 was cleared by the second line of the import.
    assert_eq!(api.invoice_status(&s1).await, "cleared");
    let other = api.import("v2", &[("ﾌﾒｲ", 40_000)]).await;
    let other_payment = id_of(&other.body["payments"][0]);
    let cleared = api
        .post(
            &format!("/v1/payments/{other_payment}/matches"),
            json!({ "invoice_ids": [s1] }),
        )
        .await;
    assert_problem(&cleared, StatusCode::CONFLICT, "conflict");

    let ok = api.post(&path, json!({ "invoice_ids": [y1, y2] })).await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    let already = api.post(&path, json!({ "invoice_ids": [tiny] })).await;
    assert_problem(&already, StatusCode::CONFLICT, "conflict");

    let missing = api
        .post(
            "/v1/payments/00000000-0000-7000-8000-000000000000/matches",
            json!({ "invoice_ids": [tiny] }),
        )
        .await;
    assert_problem(&missing, StatusCode::NOT_FOUND, "not_found");
}

#[sqlx::test]
async fn learning_an_alias_that_belongs_to_another_customer_is_refused(pool: PgPool) {
    let api = Api::new(pool);
    let yamada = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let sato = api
        .customer("有限会社佐藤工業", "サトウコウギョウ", false)
        .await;
    api.invoice(&yamada, "Y-1", 60_000, "2026-10-31").await;
    let s1 = api.invoice(&sato, "S-1", 70_000, "2026-11-30").await;

    // Yamada pays an invoice of Sato's (a group company, say). Matching it is fine; teaching the system that
    // Yamada's name means Sato is not.
    let imported = api
        .import("alias", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 70_000), ("", 60_000)])
        .await;
    let payment = id_of(&imported.body["payments"][0]);
    assert_eq!(imported.body["payments"][0]["status"], "unmatched");
    let path = format!("/v1/payments/{payment}/matches");

    let refused = api
        .post(
            &path,
            json!({ "invoice_ids": [s1], "register_alias": true }),
        )
        .await;
    assert_problem(&refused, StatusCode::CONFLICT, "conflict");
    assert_eq!(
        api.invoice_status(&s1).await,
        "open",
        "the refused request matched nothing"
    );

    let accepted = api.post(&path, json!({ "invoice_ids": [s1] })).await;
    assert_eq!(accepted.status, StatusCode::CREATED, "{}", accepted.body);

    // A payment without a payer name has nothing to learn from.
    let nameless = &imported.body["payments"][1];
    assert_eq!(nameless["status"], "suggested");
    assert_eq!(nameless["payer_name_normalized"], "");
    let match_id = active_match_id(
        &api.get(&format!("/v1/payments/{}", id_of(nameless)))
            .await
            .body,
    );
    let reply = api
        .post(
            &format!("/v1/matches/{match_id}/confirm"),
            json!({ "register_alias": true }),
        )
        .await;
    assert_problem(
        &reply,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
    assert_eq!(
        reply.body["errors"],
        json!([{ "field": "register_alias", "code": "payer_name_not_normalizable" }])
    );
}

// ---------------------------------------------------------------------------------------------------------
// Deciding again
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn unmatched_payments_are_decided_again(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;

    // Nothing to match against yet: the branch name is unknown, and no invoice has these amounts.
    let imported = api
        .import(
            "day-1",
            &[("ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ", 99_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 77_000), ("ﾌﾒｲ", 5_000)],
        )
        .await;
    assert_eq!(imported.body["summary"]["unmatched"], 3);
    let ids: Vec<String> = imported.body["payments"]
        .as_array()
        .unwrap()
        .iter()
        .map(id_of)
        .collect();
    assert_eq!(
        imported.body["payments"][0]["decided_at"],
        imported.body["payments"][0]["created_at"]
    );

    // Deciding again changes nothing while nothing else has changed. It goes through the unmatched payments
    // a page at a time.
    let first_page = api.post_empty("/v1/payments/rematch?limit=2").await;
    assert_eq!(first_page.status, StatusCode::OK, "{}", first_page.body);
    assert_eq!(
        first_page.body["summary"],
        json!({ "total": 2, "matched": 0, "suggested": 0, "unmatched": 2 })
    );
    assert_eq!(first_page.body["items"], json!([]));
    assert_eq!(first_page.body["next_cursor"], ids[1].as_str());
    let second_page = api
        .post_empty(&format!("/v1/payments/rematch?limit=2&cursor={}", ids[1]))
        .await;
    assert_eq!(second_page.body["summary"]["total"], 1);
    assert_eq!(second_page.body["next_cursor"], Value::Null);

    // The branch name becomes an alias and the invoices arrive: one of 99,000 and two of 77,000.
    let alias = api
        .post(
            &format!("/v1/customers/{customer}/aliases"),
            json!({ "alias": "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ" }),
        )
        .await;
    assert_eq!(alias.status, StatusCode::CREATED);
    let invoice = api
        .invoice(&customer, "INV-001", 99_000, "2026-11-30")
        .await;
    let older = api
        .invoice(&customer, "INV-002", 77_000, "2026-10-31")
        .await;
    api.invoice(&customer, "INV-003", 77_000, "2026-11-30")
        .await;

    let again = api.post_empty("/v1/payments/rematch").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(
        again.body["summary"],
        json!({ "total": 3, "matched": 1, "suggested": 1, "unmatched": 1 })
    );
    assert_eq!(again.body["next_cursor"], Value::Null);
    let changed = again.body["items"].as_array().unwrap();
    assert_eq!(changed.len(), 2);

    // The first payment is recognised by its new alias and confirmed without a person (R1).
    assert_eq!(changed[0]["id"], ids[0].as_str());
    assert_eq!(changed[0]["status"], "matched");
    assert_eq!(changed[0]["decision"]["outcome"], "auto_confirmed");
    assert_eq!(changed[0]["decision"]["rule"], "R1");
    assert_ne!(changed[0]["decided_at"], changed[0]["created_at"]);
    assert_eq!(api.invoice_status(&invoice).await, "cleared");
    let detail = api.get(&format!("/v1/payments/{}", ids[0])).await.body;
    assert_eq!(detail["matches"][0]["state"], "confirmed");
    assert_eq!(detail["matches"][0]["rule"], "R1");

    // The second has two invoices of its amount now: a suggestion, the older one.
    assert_eq!(changed[1]["id"], ids[1].as_str());
    assert_eq!(changed[1]["status"], "suggested");
    assert_eq!(
        changed[1]["decision"]["reason"]["kind"],
        "same_amount_invoices"
    );
    assert_eq!(changed[1]["decision"]["invoice_ids"], json!([older]));

    // The third is still unmatched, and the only one left to look at.
    let third = api.get(&format!("/v1/payments/{}", ids[2])).await.body;
    assert_eq!(third["status"], "unmatched");
    assert_eq!(third["decision"]["reason"], "payer_unknown");
    let once_more = api.post_empty("/v1/payments/rematch").await;
    assert_eq!(
        once_more.body["summary"],
        json!({ "total": 1, "matched": 0, "suggested": 0, "unmatched": 1 })
    );
    assert_eq!(api.count("matches").await, 2);

    assert_problem(
        &api.post_empty("/v1/payments/rematch?limit=0").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
    );
}

#[sqlx::test]
async fn a_payment_that_lost_its_suggestion_is_matched_to_what_is_left(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let october = api
        .invoice(&customer, "INV-010", 55_000, "2026-10-31")
        .await;
    let november = api
        .invoice(&customer, "INV-011", 55_000, "2026-11-30")
        .await;

    // Two payments of the amount two invoices have: each is suggested the older invoice.
    let imported = api
        .import("both", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000)])
        .await;
    assert_eq!(imported.body["summary"]["suggested"], 2);
    let first = format!("/v1/payments/{}", id_of(&imported.body["payments"][0]));
    let second = format!("/v1/payments/{}", id_of(&imported.body["payments"][1]));

    // A person confirms the first. The second loses its suggestion with the invoice.
    let confirm = format!(
        "/v1/matches/{}/confirm",
        active_match_id(&api.get(&first).await.body)
    );
    assert_eq!(api.post_empty(&confirm).await.status, StatusCode::OK);
    assert_eq!(api.invoice_status(&october).await, "cleared");
    let lost = api.get(&second).await.body;
    assert_eq!(lost["status"], "unmatched");
    assert_eq!(lost["matches"][0]["cancel_reason"], "invoice_cleared");

    // Decided again, it has one invoice of its amount left: confirmed without a person.
    let again = api.post_empty("/v1/payments/rematch").await;
    assert_eq!(
        again.body["summary"],
        json!({ "total": 1, "matched": 1, "suggested": 0, "unmatched": 0 })
    );
    assert_eq!(api.invoice_status(&november).await, "cleared");
    let settled = api.get(&second).await.body;
    assert_eq!(settled["status"], "matched");
    assert_eq!(settled["decision"]["outcome"], "auto_confirmed");
    let history = settled["matches"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["state"], "cancelled");
    assert_eq!(history[1]["state"], "confirmed");
    assert_eq!(
        history[1]["allocations"][0]["invoice_id"],
        november.as_str()
    );
}

#[sqlx::test]
async fn a_line_that_clears_an_invoice_takes_the_suggestion_of_an_earlier_line(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    for (number, amount, due) in [
        ("INV-001", 55_000, "2026-10-31"),
        ("INV-002", 55_000, "2026-11-30"),
        ("INV-003", 33_000, "2026-11-30"),
    ] {
        api.invoice(&customer, number, amount, due).await;
    }

    // Line 1 has two invoices of its amount: a suggestion. Line 2 pays all three invoices, the only
    // combination with that sum: confirmed, and the invoice suggested to line 1 is gone.
    let imported = api
        .import("one-batch", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 143_000)])
        .await;
    assert_eq!(imported.status, StatusCode::CREATED, "{}", imported.body);
    assert_eq!(
        imported.body["summary"],
        json!({ "total": 2, "matched": 1, "suggested": 0, "unmatched": 1 })
    );
    let first = &imported.body["payments"][0];
    assert_eq!(first["decision"]["outcome"], "suggested");
    assert_eq!(first["status"], "unmatched");
    let detail = api
        .get(&format!("/v1/payments/{}", id_of(first)))
        .await
        .body;
    assert_eq!(detail["matches"][0]["state"], "cancelled");
    assert_eq!(detail["matches"][0]["cancel_reason"], "invoice_cleared");

    let second = &imported.body["payments"][1];
    assert_eq!(second["status"], "matched");
    assert_eq!(second["decision"]["rule"], "R2");
    assert_eq!(
        api.get("/v1/invoices?status=open").await.body["items"],
        json!([])
    );
}

#[sqlx::test]
async fn payments_are_listed_by_status(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-11-30")
        .await;
    api.invoice(&customer, "INV-002", 88_000, "2026-11-30")
        .await;

    let imported = api
        .import("list", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000), ("ﾌﾒｲ", 88_000), ("ﾌﾒｲ", 1)])
        .await;
    let ids: Vec<String> = imported.body["payments"]
        .as_array()
        .unwrap()
        .iter()
        .map(id_of)
        .collect();

    let all = api.get("/v1/payments").await;
    assert_eq!(
        all.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect::<Vec<_>>(),
        ids,
        "in line order"
    );
    for (status, expected) in [
        ("matched", &ids[0]),
        ("suggested", &ids[1]),
        ("unmatched", &ids[2]),
    ] {
        let listed = api.get(&format!("/v1/payments?status={status}")).await;
        assert_eq!(
            listed.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(id_of)
                .collect::<Vec<_>>(),
            vec![expected.clone()]
        );
    }

    let page = api.get("/v1/payments?limit=2").await;
    assert_eq!(page.body["next_cursor"], ids[1].as_str());
    let rest = api
        .get(&format!("/v1/payments?limit=2&cursor={}", ids[1]))
        .await;
    assert_eq!(
        rest.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect::<Vec<_>>(),
        [ids[2].clone()]
    );
}

// ---------------------------------------------------------------------------------------------------------
// Corrections: removing an alias, voiding an invoice
// ---------------------------------------------------------------------------------------------------------

#[sqlx::test]
async fn a_wrong_alias_is_removed_and_no_longer_identifies_the_customer(pool: PgPool) {
    let api = Api::new(pool);
    let yamada = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let suzuki = api
        .customer("鈴木工業株式会社", "スズキコウギョウ", false)
        .await;
    api.invoice(&yamada, "INV-001", 110_000, "2026-10-31").await;
    api.invoice(&yamada, "INV-002", 110_000, "2026-11-30").await;

    // The branch name is registered for the wrong customer, and a payment is confirmed through it.
    let alias = api
        .post(
            &format!("/v1/customers/{yamada}/aliases"),
            json!({ "alias": "ｽｽﾞｷ ｵｵｻｶｼﾃﾝ" }),
        )
        .await;
    assert_eq!(alias.status, StatusCode::CREATED, "{}", alias.body);
    let alias_id = id_of(&alias.body);
    let before = api.import("before", &[("ｽｽﾞｷ ｵｵｻｶｼﾃﾝ", 110_000)]).await;
    assert_eq!(before.body["payments"][0]["status"], "suggested");
    assert_eq!(
        before.body["payments"][0]["decision"]["reason"]["kind"],
        "same_amount_invoices"
    );

    let alias_path = format!("/v1/customers/{yamada}/aliases/{alias_id}");
    // Not under another customer, and not the customer's own kana name.
    assert_problem(
        &api.delete(&format!("/v1/customers/{suzuki}/aliases/{alias_id}"))
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    let own = api.get(&format!("/v1/customers/{yamada}")).await.body;
    assert_eq!(own["aliases"][0]["source"], "kana");
    assert_problem(
        &api.delete(&format!(
            "/v1/customers/{yamada}/aliases/{}",
            id_of(&own["aliases"][0])
        ))
        .await,
        StatusCode::CONFLICT,
        "conflict",
    );

    let removed = api.delete(&alias_path).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.body);
    assert_eq!(removed.body, Value::Null);
    let after = api.get(&format!("/v1/customers/{yamada}")).await.body;
    assert_eq!(after["aliases"].as_array().unwrap().len(), 1);
    assert_problem(
        &api.delete(&alias_path).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );

    // What was matched through the alias stays; a person decides about it.
    let earlier = api
        .get(&format!(
            "/v1/payments/{}",
            id_of(&before.body["payments"][0])
        ))
        .await
        .body;
    assert_eq!(earlier["status"], "suggested");

    // The next payment under the name is not taken for this customer's.
    let next = api.import("after", &[("ｽｽﾞｷ ｵｵｻｶｼﾃﾝ", 55_000)]).await;
    assert_eq!(next.body["payments"][0]["status"], "unmatched");
    assert_eq!(
        next.body["payments"][0]["decision"]["reason"],
        "payer_unknown"
    );

    // And the name is free for the customer it belongs to.
    let right = api
        .post(
            &format!("/v1/customers/{suzuki}/aliases"),
            json!({ "alias": "ｽｽﾞｷ ｵｵｻｶｼﾃﾝ" }),
        )
        .await;
    assert_eq!(right.status, StatusCode::CREATED, "{}", right.body);
}

#[sqlx::test]
async fn a_void_invoice_takes_no_part_in_matching(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    let wrong = api
        .invoice(&customer, "INV-001", 55_000, "2026-10-31")
        .await;
    let right = api
        .invoice(&customer, "INV-002", 55_000, "2026-11-30")
        .await;
    let paid = api
        .invoice(&customer, "INV-003", 33_000, "2026-11-30")
        .await;

    // Two invoices of 55,000: a suggestion, the older one. 33,000 is confirmed.
    let imported = api
        .import("void", &[("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000), ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 33_000)])
        .await;
    let payment = format!("/v1/payments/{}", id_of(&imported.body["payments"][0]));
    assert_eq!(imported.body["payments"][0]["status"], "suggested");
    assert_eq!(
        imported.body["payments"][0]["decision"]["invoice_ids"],
        json!([wrong])
    );

    // A cleared invoice has a payment matched to it: not voided.
    assert_problem(
        &api.post_empty(&format!("/v1/invoices/{paid}/void")).await,
        StatusCode::CONFLICT,
        "conflict",
    );
    assert_eq!(api.invoice_status(&paid).await, "cleared");

    // The older invoice was a mistake. Voiding it takes the suggestion with it.
    let voided = api.post_empty(&format!("/v1/invoices/{wrong}/void")).await;
    assert_eq!(voided.status, StatusCode::OK, "{}", voided.body);
    assert_eq!(voided.body["id"], wrong.as_str());
    assert_eq!(voided.body["status"], "void");
    let lost = api.get(&payment).await.body;
    assert_eq!(lost["status"], "unmatched");
    assert_eq!(lost["matches"][0]["state"], "cancelled");
    assert_eq!(lost["matches"][0]["cancel_reason"], "invoice_voided");

    assert_problem(
        &api.post_empty(&format!("/v1/invoices/{wrong}/void")).await,
        StatusCode::CONFLICT,
        "conflict",
    );
    assert_problem(
        &api.post_empty("/v1/invoices/019a0000-0000-7000-8000-000000000000/void")
            .await,
        StatusCode::NOT_FOUND,
        "not_found",
    );

    // It is listed as void, has no match, and cannot be matched by hand.
    let listed = api.get("/v1/invoices?status=void").await.body;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["id"], wrong.as_str());
    let detail = api.get(&format!("/v1/invoices/{wrong}")).await.body;
    assert_eq!(detail["status"], "void");
    assert_eq!(detail["cleared_by"], Value::Null);
    assert_problem(
        &api.post(
            &format!("{payment}/matches"),
            json!({ "invoice_ids": [wrong] }),
        )
        .await,
        StatusCode::CONFLICT,
        "conflict",
    );

    // Decided again, the payment has one invoice of its amount left.
    let again = api.post_empty("/v1/payments/rematch").await;
    assert_eq!(
        again.body["summary"],
        json!({ "total": 1, "matched": 1, "suggested": 0, "unmatched": 0 })
    );
    assert_eq!(api.invoice_status(&right).await, "cleared");
    assert_eq!(api.invoice_status(&wrong).await, "void");

    // The number of the void invoice can be registered again, once.
    let again = api
        .invoice(&customer, "INV-001", 66_000, "2026-10-31")
        .await;
    assert_ne!(again, wrong);
    let twice = api
        .post(
            "/v1/invoices",
            json!({ "customer_id": customer, "invoice_number": "INV-001", "amount": 66_000, "due_date": "2026-10-31" }),
        )
        .await;
    assert_problem(&twice, StatusCode::CONFLICT, "conflict");
}

// ---------------------------------------------------------------------------------------------------------
// Consistency of payments, matches and invoices
// ---------------------------------------------------------------------------------------------------------

/// What must hold between `payments`, `matches`, `match_allocations` and `invoices` after every request,
/// whatever the requests were. Each query selects the rows that break one rule.
const CONSISTENCY_RULES: [(&str, &str); 7] = [
    (
        "I1: a payment has at most one match that is not cancelled",
        "SELECT payment_id::text FROM matches WHERE state <> 'cancelled' GROUP BY payment_id HAVING count(*) > 1",
    ),
    (
        "the status of a payment is that of its match: confirmed -> matched, suggested -> suggested, \
         none -> unmatched",
        "SELECT p.id::text || ' is ' || p.status
           FROM payments p
          WHERE p.status <> COALESCE(
                    (SELECT min(CASE m.state WHEN 'confirmed' THEN 'matched' ELSE 'suggested' END)
                       FROM matches m
                      WHERE m.payment_id = p.id AND m.state <> 'cancelled'),
                    'unmatched')",
    ),
    (
        "I2: a cleared invoice is in exactly one confirmed match, an open or void invoice in none",
        "SELECT i.id::text || ' is ' || i.status || ' with ' || count(m.id) || ' confirmed matches'
           FROM invoices i
           LEFT JOIN match_allocations a ON a.invoice_id = i.id
           LEFT JOIN matches m ON m.id = a.match_id AND m.state = 'confirmed'
          GROUP BY i.id
         HAVING count(m.id) <> CASE i.status WHEN 'cleared' THEN 1 ELSE 0 END",
    ),
    (
        "I3: a match that is not cancelled allocates the whole payment, and with the fee the whole of its \
         invoices",
        "SELECT m.id::text || ' allocates ' || COALESCE(sum(a.amount), 0) || ' of payment ' || p.amount
                || ', fee ' || m.fee_amount || ', invoices ' || COALESCE(sum(i.amount), 0)
           FROM matches m
           JOIN payments p ON p.id = m.payment_id
           LEFT JOIN match_allocations a ON a.match_id = m.id
           LEFT JOIN invoices i ON i.id = a.invoice_id
          WHERE m.state <> 'cancelled'
          GROUP BY m.id, p.amount
         HAVING COALESCE(sum(a.amount), 0) <> p.amount
             OR COALESCE(sum(a.amount), 0) + m.fee_amount <> COALESCE(sum(i.amount), 0)",
    ),
    (
        "I5: a suggestion names open invoices only",
        "SELECT m.id::text || ' names ' || i.id::text
           FROM matches m
           JOIN match_allocations a ON a.match_id = m.id
           JOIN invoices i ON i.id = a.invoice_id
          WHERE m.state = 'suggested' AND i.status <> 'open'",
    ),
    (
        "the invoices of a match belong to one customer",
        "SELECT a.match_id::text
           FROM match_allocations a
           JOIN invoices i ON i.id = a.invoice_id
          GROUP BY a.match_id
         HAVING count(DISTINCT i.customer_id) > 1",
    ),
    (
        "the timestamps and the reason of a match follow its state",
        "SELECT id::text || ' is ' || state
           FROM matches
          WHERE (state = 'confirmed') AND confirmed_at IS NULL
             OR (state = 'suggested') AND confirmed_at IS NOT NULL
             OR (state = 'cancelled') <> (cancelled_at IS NOT NULL)
             OR (state = 'cancelled') <> (cancel_reason IS NOT NULL)",
    ),
];

/// The rules that are broken, each with the rows that break it. Empty when the data is consistent.
async fn inconsistencies(conn: &mut sqlx::PgConnection) -> Vec<String> {
    let mut found = Vec::new();
    for (rule, sql) in CONSISTENCY_RULES {
        let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut *conn).await.unwrap();
        if !rows.is_empty() {
            found.push(format!("{rule}: {}", rows.join(", ")));
        }
    }
    found
}

/// A small deterministic generator, so a failing run can be repeated from its seed.
struct Dice(u64);

impl Dice {
    fn roll(&mut self, sides: usize) -> usize {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        usize::try_from(z % u64::try_from(sides).unwrap()).unwrap()
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.roll(from.len())]
    }
}

/// Few amounts and few payers, so that payments compete for the same invoices: several invoices of one
/// amount, payments short by a fee, payments that pay two invoices at once.
const AMOUNTS: [i64; 3] = [11_000, 22_000, 33_000];
const PAYERS: [&str; 5] = ["ﾔﾏﾀﾞｼﾖｳｼﾞ", "ｽｽﾞｷｺｳｷﾞﾖｳ", "ﾀﾅｶｻﾝｷﾞﾖｳ", "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ", "ﾌﾒｲ"];

/// Runs `steps` requests chosen by the seed and checks the rules after each of them. Returns how many
/// requests of each kind succeeded, and the states that the rules got to see (`table.column=value`).
async fn run_random_requests(
    api: &Api,
    seed: u64,
    steps: usize,
) -> ([usize; 9], std::collections::BTreeSet<String>) {
    let mut dice = Dice(seed);
    let mut customers = Vec::new();
    for (name, kana, fee_deducted) in [
        ("株式会社山田商事", "ヤマダショウジ", false),
        ("鈴木工業株式会社", "スズキコウギョウ", true),
        ("田中産業株式会社", "タナカサンギョウ", true),
    ] {
        customers.push(api.customer(name, kana, fee_deducted).await);
    }
    let mut succeeded = [0; 9];
    let mut seen = std::collections::BTreeSet::new();
    let mut log: Vec<String> = Vec::new();

    for step in 0..steps {
        let kind = dice.pick(&[0, 0, 0, 0, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 8]);
        let (what, ok) = match kind {
            // Import one to four lines.
            0 => {
                let lines: Vec<(&str, i64)> = (0..=dice.roll(4))
                    .map(|_| {
                        let amount = match dice.roll(4) {
                            0 => dice.pick(&AMOUNTS) - 440,
                            1 => dice.pick(&AMOUNTS) + dice.pick(&AMOUNTS),
                            _ => dice.pick(&AMOUNTS),
                        };
                        (dice.pick(&PAYERS), amount)
                    })
                    .collect();
                let reply = api
                    .import(&format!("seed-{seed}-step-{step}"), &lines)
                    .await;
                // The same lines under a new key are refused as a duplicate; nothing else may fail.
                assert!(
                    reply.status == StatusCode::CREATED || reply.status == StatusCode::CONFLICT,
                    "{}",
                    reply.body
                );
                (
                    format!("import {lines:?}"),
                    reply.status == StatusCode::CREATED,
                )
            }
            // Register an invoice.
            1 => {
                let customer = dice.pick(&[0, 1, 2]);
                let amount = dice.pick(&AMOUNTS);
                let due = dice.pick(&["2026-10-31", "2026-11-30", "2026-12-31"]);
                api.invoice(&customers[customer], &format!("INV-{step:04}"), amount, due)
                    .await;
                (format!("invoice of {amount} for customer {customer}"), true)
            }
            // Confirm a suggestion; every other time, learn the payer name with it.
            2 => {
                let suggested: Vec<String> = sqlx::query_scalar(
                    "SELECT id::text FROM matches WHERE state = 'suggested' ORDER BY id",
                )
                .fetch_all(&api.pool)
                .await
                .unwrap();
                if suggested.is_empty() {
                    ("confirm (nothing suggested)".to_owned(), false)
                } else {
                    let id = &suggested[dice.roll(suggested.len())];
                    let path = format!("/v1/matches/{id}/confirm");
                    let reply = if dice.roll(2) == 0 {
                        api.post(&path, json!({ "register_alias": true })).await
                    } else {
                        api.post_empty(&path).await
                    };
                    // Learning a name that already belongs to another customer is refused.
                    assert!(
                        reply.status == StatusCode::OK || reply.status == StatusCode::CONFLICT,
                        "{}",
                        reply.body
                    );
                    (format!("confirm {id}"), reply.status == StatusCode::OK)
                }
            }
            // Cancel a match: every other time a suggestion (there are fewer of them), otherwise any.
            3 => {
                let wanted = if dice.roll(2) == 0 {
                    "suggested"
                } else {
                    "cancelled"
                };
                let active: Vec<String> = sqlx::query_scalar(
                    "SELECT id::text FROM matches
                      WHERE state <> 'cancelled' AND (state = $1 OR $1 = 'cancelled')
                      ORDER BY id",
                )
                .bind(wanted)
                .fetch_all(&api.pool)
                .await
                .unwrap();
                if active.is_empty() {
                    ("cancel (no match)".to_owned(), false)
                } else {
                    let id = &active[dice.roll(active.len())];
                    let reply = api.post_empty(&format!("/v1/matches/{id}/cancel")).await;
                    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
                    (format!("cancel {id}"), true)
                }
            }
            // Match by hand a payment that is not matched, to an open invoice that explains it with or
            // without a fee. One time in four the invoice is any open one, which is usually refused.
            4 => {
                let careless = dice.roll(4) == 0;
                // Every other time a payment that has a suggestion, which the match replaces.
                let wanted = if dice.roll(2) == 0 {
                    "suggested"
                } else {
                    "any"
                };
                let pairs: Vec<(String, String, i64)> = sqlx::query_as(
                    "SELECT p.id::text, i.id::text, i.amount - p.amount
                       FROM payments p
                       JOIN invoices i ON i.status = 'open' AND ($1 OR i.amount - p.amount IN (0, 440))
                      WHERE p.status <> 'matched' AND (p.status = $2 OR $2 = 'any')
                      ORDER BY p.id, i.id",
                )
                .bind(careless)
                .bind(wanted)
                .fetch_all(&api.pool)
                .await
                .unwrap();
                if pairs.is_empty() {
                    ("manual match (nothing fits)".to_owned(), false)
                } else {
                    let (payment, invoice, fee) = &pairs[dice.roll(pairs.len())];
                    let reply = api
                        .post(
                            &format!("/v1/payments/{payment}/matches"),
                            json!({ "invoice_ids": [invoice], "fee_amount": (*fee).max(0) }),
                        )
                        .await;
                    assert!(
                        reply.status == StatusCode::CREATED
                            || reply.status == StatusCode::UNPROCESSABLE_ENTITY,
                        "{}",
                        reply.body
                    );
                    assert!(
                        careless || reply.status == StatusCode::CREATED,
                        "{}",
                        reply.body
                    );
                    (
                        format!("manual match of {payment} to {invoice}, fee {fee}"),
                        reply.status == StatusCode::CREATED,
                    )
                }
            }
            // Decide the unmatched payments again, a few at a time.
            5 => {
                let limit = 1 + dice.roll(5);
                let reply = api
                    .post_empty(&format!("/v1/payments/rematch?limit={limit}"))
                    .await;
                assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
                let changed = reply.body["items"].as_array().unwrap().len();
                (
                    format!("rematch of {limit}, {changed} changed"),
                    changed > 0,
                )
            }
            // Void an open invoice.
            7 => {
                let open: Vec<String> = sqlx::query_scalar(
                    "SELECT id::text FROM invoices WHERE status = 'open' ORDER BY id",
                )
                .fetch_all(&api.pool)
                .await
                .unwrap();
                if open.is_empty() {
                    ("void (no open invoice)".to_owned(), false)
                } else {
                    let id = &open[dice.roll(open.len())];
                    let reply = api.post_empty(&format!("/v1/invoices/{id}/void")).await;
                    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
                    (format!("void {id}"), true)
                }
            }
            // Remove an alias that is not a customer's own kana name.
            8 => {
                let aliases: Vec<(String, String)> = sqlx::query_as(
                    "SELECT customer_id::text, id::text FROM customer_aliases WHERE source <> 'kana'
                      ORDER BY id",
                )
                .fetch_all(&api.pool)
                .await
                .unwrap();
                if aliases.is_empty() {
                    ("remove alias (none)".to_owned(), false)
                } else {
                    let (customer, alias) = &aliases[dice.roll(aliases.len())];
                    let reply = api
                        .delete(&format!("/v1/customers/{customer}/aliases/{alias}"))
                        .await;
                    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.body);
                    (format!("remove alias {alias}"), true)
                }
            }
            // Teach the branch name to a customer. Refused once another customer has it.
            _ => {
                let customer = dice.pick(&[0, 1, 2]);
                let reply = api
                    .post(
                        &format!("/v1/customers/{}/aliases", customers[customer]),
                        json!({ "alias": "ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ" }),
                    )
                    .await;
                assert!(
                    reply.status == StatusCode::CREATED
                        || reply.status == StatusCode::OK
                        || reply.status == StatusCode::CONFLICT,
                    "{}",
                    reply.body
                );
                (
                    format!("alias for customer {customer}"),
                    reply.status != StatusCode::CONFLICT,
                )
            }
        };
        succeeded[kind] += usize::from(ok);
        log.push(format!("{step}: {what}"));

        let mut conn = api.pool.acquire().await.unwrap();
        let broken = inconsistencies(&mut conn).await;
        assert!(
            broken.is_empty(),
            "seed {seed}: inconsistent after step {step}\n{}\nrequests so far:\n{}",
            broken.join("\n"),
            log.join("\n")
        );
        let states: Vec<String> = sqlx::query_scalar(
            "SELECT 'payments.status=' || status FROM payments
             UNION SELECT 'matches.state=' || state FROM matches
             UNION SELECT 'matches.cancel_reason=' || cancel_reason FROM matches WHERE state = 'cancelled'
             UNION SELECT 'invoices.status=' || status FROM invoices",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        seen.extend(states);
    }
    (succeeded, seen)
}

#[sqlx::test]
async fn any_sequence_of_requests_leaves_payments_matches_and_invoices_consistent(pool: PgPool) {
    let api = Api::new(pool);
    let (succeeded, seen) = run_random_requests(&api, 20_261_118, 400).await;

    // The run is worth something only if every kind of request did change the data.
    for (kind, count) in [
        "import",
        "invoice",
        "confirm",
        "cancel",
        "manual match",
        "rematch",
        "alias",
        "void",
        "remove alias",
    ]
    .iter()
    .zip(succeeded)
    {
        assert!(count > 0, "no {kind} succeeded: {succeeded:?}");
    }
    // And only if the rules got to see every state.
    for state in [
        "payments.status=unmatched",
        "payments.status=suggested",
        "payments.status=matched",
        "matches.state=suggested",
        "matches.state=confirmed",
        "matches.state=cancelled",
        "matches.cancel_reason=dismissed",
        "matches.cancel_reason=reversed",
        "matches.cancel_reason=replaced",
        "matches.cancel_reason=invoice_cleared",
        "matches.cancel_reason=invoice_voided",
        "invoices.status=open",
        "invoices.status=cleared",
        "invoices.status=void",
    ] {
        assert!(seen.contains(state), "never seen: {state}");
    }
}

#[sqlx::test]
async fn other_sequences_of_requests_are_consistent_too(pool: PgPool) {
    let api = Api::new(pool);
    // A second seed, in its own database. Change the number to try other sequences.
    run_random_requests(&api, 7, 400).await;
}

#[sqlx::test]
async fn the_consistency_rules_notice_damaged_data(pool: PgPool) {
    let api = Api::new(pool);
    let customer = api
        .customer("株式会社山田商事", "ヤマダショウジ", false)
        .await;
    api.invoice(&customer, "INV-001", 110_000, "2026-10-31")
        .await;
    api.invoice(&customer, "INV-002", 55_000, "2026-10-31")
        .await;
    api.invoice(&customer, "INV-003", 55_000, "2026-11-30")
        .await;
    // One confirmed match, one suggestion, and a cancelled suggestion with an unmatched payment.
    let imported = api
        .import(
            "damage",
            &[
                ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 110_000),
                ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000),
                ("ﾔﾏﾀﾞｼﾖｳｼﾞ", 55_000),
            ],
        )
        .await;
    assert_eq!(
        imported.body["summary"],
        json!({ "total": 3, "matched": 1, "suggested": 2, "unmatched": 0 })
    );
    let last = api
        .get(&format!(
            "/v1/payments/{}",
            id_of(&imported.body["payments"][2])
        ))
        .await;
    let dismissed = api
        .post_empty(&format!(
            "/v1/matches/{}/cancel",
            active_match_id(&last.body)
        ))
        .await;
    assert_eq!(dismissed.status, StatusCode::OK);

    let mut conn = api.pool.acquire().await.unwrap();
    assert_eq!(inconsistencies(&mut conn).await, Vec::<String>::new());

    // Each change is one a bug could make; each is rolled back before the next.
    let damage = [
        (
            "UPDATE payments SET status = 'unmatched' WHERE status = 'matched'",
            "the status of a payment",
        ),
        (
            "UPDATE payments SET status = 'suggested' WHERE status = 'unmatched'",
            "the status of a payment",
        ),
        (
            "UPDATE matches SET state = 'confirmed', confirmed_at = now() WHERE state = 'suggested'",
            "the status of a payment",
        ),
        (
            "UPDATE invoices SET status = 'open' WHERE status = 'cleared'",
            "I2",
        ),
        (
            "UPDATE invoices SET status = 'cleared' WHERE invoice_number = 'INV-003'",
            "I2",
        ),
        (
            "UPDATE invoices SET status = 'cleared' WHERE invoice_number = 'INV-002'",
            "I5",
        ),
        (
            "UPDATE matches SET fee_amount = fee_amount + 1 WHERE state = 'confirmed'",
            "I3",
        ),
        (
            "UPDATE match_allocations a SET amount = a.amount - 1 FROM matches m
              WHERE m.id = a.match_id AND m.state = 'suggested'",
            "I3",
        ),
        (
            "DELETE FROM match_allocations a USING matches m WHERE m.id = a.match_id AND m.state = 'confirmed'",
            "I3",
        ),
        (
            "UPDATE matches SET cancelled_at = now() WHERE state = 'confirmed'",
            "the timestamps",
        ),
        (
            "UPDATE matches SET cancel_reason = NULL WHERE state = 'cancelled'",
            "the timestamps",
        ),
        (
            "UPDATE matches SET confirmed_at = NULL WHERE state = 'confirmed'",
            "the timestamps",
        ),
    ];
    for (sql, rule) in damage {
        let mut tx = api.pool.begin().await.unwrap();
        let changed = sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert!(changed > 0, "changed nothing: {sql}");
        let broken = inconsistencies(&mut tx).await;
        assert!(
            broken.iter().any(|found| found.starts_with(rule)),
            "{sql}\nwas expected to break \"{rule}\", but the rules found: {broken:?}"
        );
        tx.rollback().await.unwrap();
    }
}
