//! Port of pymt5 example 04: place a pending order far from market, modify, cancel it.
//! **Places real orders on the account in the environment** - use a demo account.
//!
//!   LOGIN=... PASSWORD=... SERVER=wss://... SYMBOL=EURUSD cargo run --example trading

use rustmt5::{ClientConfig, ModifyOrderOptions, Mt5Client, PendingOrderOptions};

#[tokio::main]
async fn main() -> rustmt5::Result<()> {
    let login: u64 = std::env::var("LOGIN").expect("LOGIN").parse().expect("numeric LOGIN");
    let client = Mt5Client::new(ClientConfig::new(std::env::var("SERVER").expect("SERVER")));
    client.connect().await?;
    client.login(login, &std::env::var("PASSWORD").expect("PASSWORD")).await?;
    client.load_symbols(true).await?;

    let symbol = std::env::var("SYMBOL").unwrap_or_else(|_| "EURUSD".into());
    let rates = client.copy_rates_from_pos(&symbol, 1, 0, 1).await?;
    let price = rates.last().map(|b| b.get_f64("close")).unwrap_or(1.0);

    // a buy limit 5% below the market will not fill
    let far = (price * 0.95 * 1e5).round() / 1e5;
    let placed = client.buy_limit(&symbol, 0.01, far, &PendingOrderOptions { comment: "rustmt5 demo".into(), ..Default::default() }).await?;
    println!("place: {placed}");
    if placed.success && placed.order != 0 {
        let modified = client.modify_pending_order(placed.order, &ModifyOrderOptions { price: Some(far * 0.999), ..Default::default() }).await?;
        println!("modify: {modified}");
        println!("cancel: {}", client.cancel_pending_order(placed.order).await?);
    }
    client.close().await;
    Ok(())
}
