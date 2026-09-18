//! Library surface exists for the binary and the integration tests, not as a stable API.

pub mod alert;
pub mod cache;
pub mod eval;
pub mod ingest;
pub mod jev;
pub mod judge;
pub mod metrics;
pub mod pipeline;
pub mod route;
pub mod serve;
pub mod template;

pub const NAME: &str = env!("CARGO_PKG_NAME");

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .expect("http client")
}
