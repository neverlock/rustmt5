//! End-to-end trading check on a **demo** account (refuses to run on anything else).
//!
//! Opens a 0.01 lot market position, modifies its SL/TP, closes it, then places a far-away pending
//! order, modifies and cancels it. Everything is cleaned up at the end.
//!
//!   DEMO_LOGIN=... DEMO_PASSWORD=... DEMO_SERVER=wss://host/terminal \
//!       cargo run --example demo_trade_check -- [--prefix STD] [--symbols EURUSDs,XAUUSDs]
//!
//! * `--prefix STD` reads `DEMO_STD_LOGIN` / `DEMO_STD_PASSWORD` / `DEMO_STD_SERVER` (server falls back to `DEMO_SERVER`).
//! * `--symbols A,B` runs the whole check for each symbol (default: `$SYMBOL`, else `EURUSDc`).
//!
//! (Without a prefix it uses DEMO_*, falling back to LOGIN/PASSWORD/SERVER - the account must still be a demo.)

use std::time::Duration;

use rustmt5::{ClientConfig, CloseOptions, MarketOrderOptions, ModifyOrderOptions, Mt5Client, PendingOrderOptions, Result};

fn env2(a: &str, b: &str) -> String {
    std::env::var(a).or_else(|_| std::env::var(b)).unwrap_or_else(|_| panic!("set {a} (or {b})"))
}

async fn wait_for<F, Fut>(what: &str, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..15 {
        if f().await { return true; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    eprintln!("timed out waiting for: {what}");
    false
}

fn strict(a: &str) -> String { std::env::var(a).unwrap_or_else(|_| panic!("set {a}")) }

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

async fn run_symbol(client: &Mt5Client, symbol: &str) -> Result<()> {
    let symbol = symbol.to_string();
    let id = client.get_symbol_id(&symbol).expect("symbol not found");
    client.subscribe_ticks(&[id]).await?;
    let tick = loop {
        if let Some(t) = client.symbol_info_tick(&symbol) { break t; }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    let (bid, ask) = (tick.get_f64("bid"), tick.get_f64("ask"));
    println!("{symbol}: bid={bid} ask={ask}");

    // --- market position ---
    let opened = client.buy_market(&symbol, 0.01, &MarketOrderOptions::default()).await?;
    println!("buy_market  -> {opened}");
    if opened.success {
        let appeared = wait_for("position to appear", || async {
            client.get_positions().await.map(|p| p.iter().any(|x| x.get_str("trade_symbol") == symbol)).unwrap_or(false)
        })
        .await;
        if appeared {
            let pos = client.get_positions().await?.into_iter().find(|p| p.get_str("trade_symbol") == symbol).unwrap();
            let pid = pos.get_u64("position_id");
            println!("position {pid}: open={} profit={}", pos.get_f64("price_open"), pos.get_f64("profit"));
            let m = client.modify_position_sltp(pid, Some(bid * 0.98), Some(ask * 1.02)).await?;
            println!("modify SLTP -> {m}");
            let c = client.close_position(pid, None, &CloseOptions::default()).await?;
            println!("close       -> {c}");
        }
    }

    // --- pending order ---
    let far = ((bid * 0.95) * 1e5).round() / 1e5;
    let placed = client.buy_limit(&symbol, 0.01, far, &PendingOrderOptions::default()).await?;
    println!("buy_limit   -> {placed}");
    if placed.success {
        let m = client.modify_pending_order(placed.order, &ModifyOrderOptions { price: Some(far * 0.999), ..Default::default() }).await?;
        println!("modify      -> {m}");
        let x = client.cancel_pending_order(placed.order).await?;
        println!("cancel      -> {x}");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "rustmt5=warn".into())).init();
    let prefix = arg("--prefix").map(|p| format!("DEMO_{p}_"));
    let (server, login, password) = match &prefix {
        Some(p) => (env2(&format!("{p}SERVER"), "DEMO_SERVER"), strict(&format!("{p}LOGIN")), strict(&format!("{p}PASSWORD"))),
        None => (env2("DEMO_SERVER", "SERVER"), env2("DEMO_LOGIN", "LOGIN"), env2("DEMO_PASSWORD", "PASSWORD")),
    };
    let client = Mt5Client::new(ClientConfig::new(server));
    client.connect().await?;
    client.login(login.parse().expect("numeric login"), &password).await?;

    let acct = client.get_account().await?;
    if !acct.get_bool("is_demo") || acct.get_bool("is_real") {
        eprintln!("REFUSING to trade: this is not a demo account");
        client.close().await;
        std::process::exit(2);
    }
    println!("demo account: {} {} balance={}", acct.get_str("server"), acct.get_str("currency"), acct.get_f64("balance"));

    client.load_symbols(true).await?;
    let symbols: Vec<String> = arg("--symbols")
        .or_else(|| std::env::var("SYMBOL").ok())
        .unwrap_or_else(|| "EURUSDc".into())
        .split(',')
        .map(str::to_string)
        .collect();
    for symbol in &symbols {
        println!("=== {symbol}");
        run_symbol(&client, symbol).await?;
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    let end = client.get_positions_and_orders().await?;
    println!("final: positions={} orders={}", end.positions.len(), end.orders.len());
    client.close().await;
    Ok(())
}
