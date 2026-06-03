//! Binary entry point. All work happens in `qpedia_api::AppBuilder`.
//! The `qpedia-pvt-api` binary calls the same `AppBuilder` chain plus
//! its own `.with_routes()` / `.with_state_extension()` calls. See
//! `OPEN-CORE.md` and `crates/qpedia-api/src/lib.rs`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    qpedia_api::AppBuilder::from_env().await?.serve().await
}
