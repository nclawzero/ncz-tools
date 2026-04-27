//! Live smoke for Phase 1 — fetches `/api/config`, lists model keys,
//! sets one, prints the resolved current key. Run against a live
//! zeroclaw daemon:
//!
//! ```sh
//! ~/.cargo/bin/cargo run --release --example smoke_models -- http://127.0.0.1:42617
//! ```
//!
//! Not in CI — depends on a running daemon. Used during the Phase 1
//! ship to verify the wiring round-trips end-to-end. Safe to delete
//! once a richer integration test framework lands.

use anyhow::Result;
use zterm::cli::client::ZeroclawClient;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:42617".to_string());
    let token = std::env::var("ZEROCLAW_TOKEN").unwrap_or_default();

    let client = ZeroclawClient::new(url.clone(), token);
    println!("→ refresh_models against {url}");
    let list = client.refresh_models().await?;
    println!("  got {} entries:", list.len());
    for m in &list {
        println!("    - {:<10}  ({} → {})", m.key, m.provider, m.model);
    }
    println!(
        "→ default current_model_key = {}",
        client.current_model_key()
    );

    if let Some(first) = list.first() {
        client.set_current_model(&first.key)?;
        println!("→ set_current_model({}) ok", first.key);
        println!("→ current_model_key now = {}", client.current_model_key());
    }

    let bad = client.set_current_model("definitely-not-a-key");
    println!("→ set_current_model(bogus) -> {:?}", bad.is_err());

    Ok(())
}
