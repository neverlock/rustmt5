//! Port of pymt5 example 03: stream ticks for a few symbols.
//!
//!   LOGIN=... PASSWORD=... SERVER=wss://... cargo run --example realtime_ticks -- EURUSD GBPUSD

use std::time::Duration;

use rustmt5::{ClientConfig, Mt5Client};

#[tokio::main]
async fn main() -> rustmt5::Result<()> {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let names = if names.is_empty() { vec!["EURUSD".to_string()] } else { names };
    let login: u64 = std::env::var("LOGIN").expect("LOGIN").parse().expect("numeric LOGIN");
    let client = Mt5Client::new(ClientConfig::new(std::env::var("SERVER").expect("SERVER")));
    client.connect().await?;
    client.login(login, &std::env::var("PASSWORD").expect("PASSWORD")).await?;
    client.load_symbols(true).await?;

    client.on_tick_event(|t| println!("{:<10} bid={:<10} ask={:<10} vol={}", t.symbol, t.bid, t.ask, t.volume));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut sub = client.subscribe_ticks_managed(&client.subscribe_symbols(&refs).await?).await?;
    tokio::time::sleep(Duration::from_secs(15)).await;
    sub.unsubscribe().await?;
    client.close().await;
    Ok(())
}
