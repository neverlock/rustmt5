//! Live connectivity check. Reads LOGIN, PASSWORD, SERVER from the environment
//! (the password is never printed). Read-only: no orders are placed.
//!
//!   LOGIN=123 PASSWORD=... SERVER=wss://host/terminal cargo run --example live_check

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustmt5::{ClientConfig, Mt5Client};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing env var {name}"))
}

#[tokio::main]
async fn main() -> rustmt5::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "rustmt5=info".into())).init();
    let (login, password, server) = (env("LOGIN").parse::<u64>().expect("LOGIN must be numeric"), env("PASSWORD"), env("SERVER"));
    println!("server = {server}  login = {login}");

    let client = Mt5Client::new(ClientConfig { timeout: 20.0, ..ClientConfig::new(server) });
    client.connect().await?;
    println!("[1] websocket + bootstrap OK  (server_build = {})", client.server_build());

    let (_token, session) = client.login(login, &password).await?;
    println!("[2] login OK  session = {session}");

    let acct = client.get_account().await?;
    println!(
        "[3] account: name={:?} company={:?} server={:?} currency={} leverage=1:{} balance={:.2} equity={:.2} real={} demo={} trade_allowed={}",
        acct.get_str("name"), acct.get_str("company"), acct.get_str("server"), acct.get_str("currency"), acct.get_i64("leverage"),
        acct.get_f64("balance"), acct.get_f64("equity"), acct.get_bool("is_real"), acct.get_bool("is_demo"), acct.get_bool("trade_allowed"),
    );

    let symbols = client.load_symbols(true).await?;
    println!("[4] symbols loaded: {}", symbols.len());
    let groups = client.get_symbol_groups().await?;
    println!("[5] symbol groups: {} e.g. {:?}", groups.len(), groups.iter().take(5).collect::<Vec<_>>());

    let pos = client.get_positions_and_orders().await?;
    println!("[6] open positions = {}, pending orders = {}", pos.positions.len(), pos.orders.len());
    let hist = client.get_trade_history(0, 0).await?;
    println!("[7] history deals = {}, history orders = {}", hist.deals.len(), hist.orders.len());

    let sym = ["EURUSD", "EURUSDm", "EURUSD.", "XAUUSD", "BTCUSD"].into_iter().find(|s| symbols.contains_key(*s)).map(str::to_string).or_else(|| symbols.keys().next().cloned());
    if let Some(sym) = sym {
        println!("[8] using symbol {sym}");
        if let Some(info) = client.get_full_symbol_info(&sym).await? {
            println!("    full info: digits={} contract_size={} volume_min={} volume_step={} currency_profit={}",
                info.get_i64("digits"), info.get_f64("contract_size"), info.get_f64("volume_min"), info.get_f64("volume_step"), info.get_str("currency_profit"));
        } else { println!("    full info: none"); }

        let now = SystemTime::now();
        let end = now.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;
        let bars = client.get_rates(&sym, 60, end - 3600 * 24 * 5, end).await?;
        println!("[9] H1 bars (5d): {} {}", bars.len(), bars.last().map(|b| format!("last: t={} o={} h={} l={} c={}", b.get_i64("time"), b.get_f64("open"), b.get_f64("high"), b.get_f64("low"), b.get_f64("close"))).unwrap_or_default());

        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        client.on_tick(move |ticks| { n2.fetch_add(ticks.len(), Ordering::SeqCst); });
        let id = client.get_symbol_id(&sym).unwrap();
        client.subscribe_ticks(&[id]).await?;
        tokio::time::sleep(Duration::from_secs(8)).await;
        println!("[10] ticks received in 8s: {}  cached={:?}", n.load(Ordering::SeqCst), client.symbol_info_tick(&sym).map(|t| (t.get_f64("bid"), t.get_f64("ask"))));
        client.unsubscribe_ticks(&[id]).await?;
    }

    let h = client.health_check().await;
    println!("[11] health: state={:?} ping={:?}ms uptime={:.1}s", h.state, h.ping_latency_ms.map(|v| v.round()), h.uptime_seconds);
    client.close().await;
    println!("[12] closed cleanly");
    Ok(())
}
