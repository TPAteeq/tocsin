use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tocsin::jev::{DEFAULT_POLICY, Jev};
use tocsin::judge::{Judge, Verdict};
use tokio::net::TcpListener;

async fn mock(respond: fn(usize) -> Response) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/v1/systemone",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let counter = counter.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer test-key");
                assert_eq!(body["state"]["log"]["template"], "disk <*> failed");
                assert!(body["state"]["policy"]["page"].is_array());
                assert_eq!(body["questions"]["pageable"]["type"], "noul");
                respond(counter.fetch_add(1, Ordering::SeqCst))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, hits)
}

fn answers(pageable: f64) -> Response {
    Json(json!({
        "model": "jev-test",
        "answers": {
            "pageable": {"type": "noul", "noul": pageable},
            "detail": {"type": "noul", "noul": 0.1},
            "severity": {"type": "score", "score": 2.5, "confidence": 0.8, "legend": {}, "probabilities": {}},
            "area": {"type": "choice", "choice": "storage", "confidence": 0.9, "probabilities": {}}
        },
        "usage": {"input_tokens": 321, "output_tokens": 12}
    }))
    .into_response()
}

fn jev(endpoint: &str, policy: &str) -> Jev {
    Jev::new("test-key".into(), "jev-latest".into(), endpoint, policy)
}

#[tokio::test]
async fn retries_rate_limits_and_parses_answers() {
    let (url, hits) = mock(|hit| match hit {
        0 => (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "slow down",
        )
            .into_response(),
        1 => (
            StatusCode::from_u16(529).unwrap(),
            [("retry-after", "NaN")],
            "overloaded",
        )
            .into_response(),
        _ => answers(0.9),
    })
    .await;
    let judged = jev(&url, DEFAULT_POLICY)
        .judge("disk <*> failed")
        .await
        .unwrap();

    let expected = Verdict {
        pageable: 0.9,
        detail: 0.1,
        severity: 2.5,
        area: "storage".into(),
    };
    assert_eq!(judged.verdict, expected);
    assert_eq!(
        (judged.input_tokens, judged.model.as_str()),
        (321, "jev-test")
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn does_not_retry_invalid_requests() {
    let (url, hits) =
        mock(|_| (StatusCode::UNPROCESSABLE_ENTITY, "bad question").into_response()).await;
    let err = jev(&url, DEFAULT_POLICY)
        .judge("disk <*> failed")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("422"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejects_out_of_range_verdicts() {
    let (url, _) = mock(|_| answers(1e100)).await;
    let err = jev(&url, DEFAULT_POLICY)
        .judge("disk <*> failed")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("out-of-range"));
}

#[test]
fn policy_and_endpoint_are_part_of_the_cache_fingerprint() {
    let default = jev("http://localhost", DEFAULT_POLICY).fingerprint();
    assert_ne!(
        default,
        jev("http://localhost", "Page only when checkout is down.").fingerprint()
    );
    assert_ne!(
        default,
        jev("http://staging.local", DEFAULT_POLICY).fingerprint()
    );
}

#[tokio::test]
#[ignore = "calls the live TypeSafe API; needs TYPESAFE_API_KEY"]
async fn live_jev_separates_an_outage_from_a_health_check() {
    let key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY");
    let jev = Jev::new(
        key,
        "jev-latest".into(),
        tocsin::jev::DEFAULT_ENDPOINT,
        DEFAULT_POLICY,
    );
    let outage = jev
        .judge("kernel: Out of Memory: Killed process <NUM> (postgres)")
        .await
        .unwrap();
    let health = jev.judge("GET /healthz <2xx> <NUM>ms").await.unwrap();
    assert!(outage.verdict.attention() > 0.5, "{:?}", outage.verdict);
    assert!(health.verdict.attention() < 0.2, "{:?}", health.verdict);
}
