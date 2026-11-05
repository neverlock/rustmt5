//! End-to-end tests against an in-process mock MT5 Web Terminal server that speaks the real
//! framing + AES session-key handshake.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustmt5::crypto::{initial_cipher, AesCipher};
use rustmt5::protocol::{build_command, pack_outer, unpack_outer, Field, PropType, SerField, SeriesCodec, Value};
use rustmt5::schemas::*;
use rustmt5::{ClientConfig, Error, Mt5Client, Record};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const SESSION_KEY: [u8; 32] = [0x42; 32];

fn zero_for(f: &Field) -> SerField {
    match f.ty {
        PropType::F32 | PropType::F64 => SerField::new(f.ty, 0.0f64, f.len),
        PropType::String | PropType::FixedString => SerField::new(f.ty, "", f.len),
        PropType::Bytes => SerField::new(f.ty, Vec::<u8>::new(), f.len),
        _ => SerField::new(f.ty, 0i64, f.len),
    }
}

fn record_bytes(schema: &[Field], names: &[&str], over: &[(&str, Value)]) -> Vec<u8> {
    let fields: Vec<SerField> = schema
        .iter()
        .zip(names)
        .map(|(f, n)| match over.iter().find(|(k, _)| k == n) { Some((_, v)) => SerField::new(f.ty, v.clone(), f.len), None => zero_for(f) })
        .collect();
    SeriesCodec::serialize(&fields).unwrap()
}

fn frame(cipher: &AesCipher, cmd: u16, code: u8, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![0xAA, 0xBB];
    inner.extend(cmd.to_le_bytes());
    inner.push(code);
    inner.extend(body);
    pack_outer(&cipher.encrypt(&inner))
}

#[derive(Clone, Default)]
struct Behavior {
    reject_login: bool,
    /// close the socket right after the first successful login
    drop_after_login_once: bool,
    ignore_cmd: Option<u16>,
    logins: Arc<AtomicUsize>,
    subscribes: Arc<Mutex<Vec<Vec<u32>>>>,
    connections: Arc<AtomicUsize>,
}

fn account_body() -> Vec<u8> {
    let mut b = record_bytes(
        ACCOUNT_WEB_MAIN_SCHEMA,
        ACCOUNT_WEB_MAIN_FIELD_NAMES,
        &[
            ("account_type", Value::Int(1)),
            ("balance", Value::Float(1000.5)),
            ("credit", Value::Float(10.0)),
            ("acc_profit", Value::Float(5.0)),
            ("account_currency", Value::Str("USD".into())),
            ("margin_leverage", Value::Int(100)),
            ("account_name", Value::Str("Tester".into())),
            ("server_name", Value::Str("Mock-Server".into())),
            ("server_build", Value::Int(6182)),
            ("company", Value::Str("MockCo".into())),
            ("rights", Value::Int(0)),
        ],
    );
    // one trade-settings record (real servers always send >= 1; note pymt5 does not advance past an empty count)
    b.extend(1u32.to_le_bytes());
    b.extend(record_bytes(
        ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA,
        ACCOUNT_WEB_TRADE_SETTINGS_FIELD_NAMES,
        &[("symbol_path", Value::Str("*".into())), ("volume_min", Value::UInt(1_000_000)), ("trade_mode", Value::Int(4))],
    ));
    b.extend(0i32.to_le_bytes()); // trade_flags
    b.extend(2i32.to_le_bytes()); // symbols_count
    b.extend(0u64.to_le_bytes()); // leverage_flags
    b.extend(0i32.to_le_bytes()); // leverage rules
    b.extend(0u32.to_le_bytes()); // commissions
    b
}

fn symbols_body() -> Vec<u8> {
    let rec = |name: &str, id: u32, digits: u32| {
        SeriesCodec::serialize(&[
            SerField::fixed_string(name, 64), SerField::fixed_string("desc", 128), SerField::u32(digits), SerField::u32(id),
            SerField::fixed_string("Forex\\Major", 256), SerField::u32(0), SerField::fixed_string("", 64), SerField::u16(0),
        ])
        .unwrap()
    };
    let mut raw = 2u32.to_le_bytes().to_vec();
    raw.extend(rec("EURUSD", 1, 5));
    raw.extend(rec("USDJPY", 2, 3));
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&raw).unwrap();
    let mut body = (raw.len() as u32).to_le_bytes().to_vec();
    body.extend(enc.finish().unwrap());
    body
}

fn tick_bytes(id: u32, bid: f64, ask: f64) -> Vec<u8> {
    SeriesCodec::serialize(&[
        SerField::u32(id), SerField::i32(1_700_000_000), SerField::u32(7), SerField::f64(bid), SerField::f64(ask), SerField::f64(0.0),
        SerField::new(PropType::I64, 3i64, None), SerField::u32(100), SerField::u16(0),
    ])
    .unwrap()
}

async fn serve_connection(stream: tokio::net::TcpStream, b: Behavior) {
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let conn_no = b.connections.fetch_add(1, Ordering::SeqCst);
    let mut cipher = initial_cipher();
    while let Some(Ok(msg)) = ws.next().await {
        let Message::Binary(data) = msg else { continue };
        let (_, _, enc) = unpack_outer(&data).unwrap();
        let inner = cipher.decrypt(enc).unwrap();
        let cmd = u16::from_le_bytes([inner[2], inner[3]]);
        let payload = &inner[4..];
        if Some(cmd) == b.ignore_cmd { continue; }
        match cmd {
            0 => {
                let mut body = 6182u16.to_le_bytes().to_vec();
                body.extend([9u8; 64]);
                body.extend(SESSION_KEY);
                ws.send(Message::Binary(frame(&cipher, 0, 0, &body).into())).await.unwrap();
                cipher = AesCipher::new(&SESSION_KEY).unwrap(); // switch to session key
            }
            28 => {
                assert_eq!(payload.len(), 912, "login payload must match the browser capture size");
                if b.reject_login {
                    ws.send(Message::Binary(frame(&cipher, 28, 3, &[]).into())).await.unwrap();
                    continue;
                }
                b.logins.fetch_add(1, Ordering::SeqCst);
                let mut body = vec![0x11u8; 160];
                body.extend(777u64.to_le_bytes());
                ws.send(Message::Binary(frame(&cipher, 28, 0, &body).into())).await.unwrap();
                if b.drop_after_login_once && conn_no == 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = ws.close(None).await;
                    return;
                }
            }
            51 | 2 => ws.send(Message::Binary(frame(&cipher, cmd, 0, &[]).into())).await.unwrap(),
            3 => ws.send(Message::Binary(frame(&cipher, 3, 0, &account_body()).into())).await.unwrap(),
            34 => ws.send(Message::Binary(frame(&cipher, 34, 0, &symbols_body()).into())).await.unwrap(),
            7 => {
                let n = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                let ids: Vec<u32> = (0..n).map(|i| u32::from_le_bytes(payload[4 + i * 4..8 + i * 4].try_into().unwrap())).collect();
                b.subscribes.lock().unwrap().push(ids);
                ws.send(Message::Binary(frame(&cipher, 7, 0, &[]).into())).await.unwrap();
                // scaled ints (digits=5); the 2nd push has bid=0 meaning "unchanged" -> carried over from the cache
                ws.send(Message::Binary(frame(&cipher, 8, 0, &tick_bytes(1, 115364.0, 115366.0)).into())).await.unwrap();
                ws.send(Message::Binary(frame(&cipher, 8, 0, &tick_bytes(1, 0.0, 115370.0)).into())).await.unwrap();
            }
            4 => ws.send(Message::Binary(frame(&cipher, 4, 0, &[0, 0, 0, 0, 0, 0, 0, 0]).into())).await.unwrap(),
            12 => {
                let body = SeriesCodec::serialize(&[
                    SerField::u32(10009), SerField::u64(555), SerField::u64(556), SerField::u64(1_000_000), SerField::f64(1.15366),
                    SerField::f64(1.15364), SerField::f64(1.15366), SerField::fixed_string("done", 64), SerField::u32(9),
                ])
                .unwrap();
                ws.send(Message::Binary(frame(&cipher, 12, 0, &body).into())).await.unwrap();
            }
            _ => ws.send(Message::Binary(frame(&cipher, cmd, 0, &[]).into())).await.unwrap(),
        }
    }
}

async fn start(b: Behavior) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else { return };
            tokio::spawn(serve_connection(s, b.clone()));
        }
    });
    format!("ws://{addr}/terminal")
}

fn cfg(uri: String) -> ClientConfig { ClientConfig { timeout: 5.0, heartbeat_interval: 3600.0, ..ClientConfig::new(uri) } }

#[tokio::test]
async fn handshake_login_ping_logout() {
    let b = Behavior::default();
    let c = Mt5Client::new(cfg(start(b.clone()).await));
    c.connect().await.unwrap();
    assert_eq!(c.server_build(), 6182);
    assert!(!c.is_connected());
    let (token, session) = c.login(12345678, "secret").await.unwrap();
    assert_eq!(session, 777);
    assert_eq!(token, "11".repeat(160));
    assert!(c.is_connected());
    c.ping().await.unwrap();
    let h = c.health_check().await;
    assert!(h.ping_latency_ms.is_some());
    c.close().await;
    assert!(!c.is_connected());
    assert_eq!(b.logins.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn login_rejection_is_authentication_error() {
    let c = Mt5Client::new(cfg(start(Behavior { reject_login: true, ..Default::default() }).await));
    c.connect().await.unwrap();
    match c.login(1, "bad").await {
        Err(Error::Authentication(m)) => assert!(m.contains("code=3"), "{m}"),
        other => panic!("expected auth error, got {other:?}"),
    }
    assert!(!c.is_connected());
    c.close().await;
}

#[tokio::test]
async fn account_symbols_ticks_and_trade() {
    let b = Behavior::default();
    let c = Mt5Client::new(cfg(start(b.clone()).await));
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();

    let acct = c.get_account().await.unwrap();
    assert_eq!(acct.get_f64("balance"), 1000.5);
    assert_eq!(acct.get_str("currency"), "USD");
    assert_eq!(acct.get_i64("leverage"), 100);
    assert_eq!(acct.get_str("server"), "Mock-Server");
    assert!(acct.get_bool("is_demo") && !acct.get_bool("is_real") && acct.get_bool("trade_allowed"));
    assert_eq!(acct.get_f64("equity"), 1000.5 + 10.0 + 5.0);
    assert_eq!(acct.get_i64("symbols_count"), 2);
    let ts = acct.get_records("trade_settings");
    assert_eq!(ts.len(), 1);
    assert_eq!((ts[0].get_str("symbol_path"), ts[0].get_i64("trade_mode"), ts[0].get_list("swap_rates").len()), ("*", 4, 7));
    assert_eq!(c.version().await.unwrap(), Some((500, 6182, String::new())));
    assert_eq!(c.terminal_info().await.unwrap().get_str("company"), "MockCo");
    let summary = c.get_account_summary().await.unwrap();
    assert_eq!((summary.leverage, summary.positions_count), (100, 0));

    let symbols = c.load_symbols(true).await.unwrap();
    assert_eq!(symbols.len(), 2);
    assert_eq!(c.get_symbol_id("USDJPY"), Some(2));
    assert!(matches!(c.subscribe_symbols(&["NOPE"]).await, Err(Error::SymbolNotFound(_))));
    assert_eq!(c.symbols_get(Some("EUR*"), true).await.unwrap().len(), 1);

    let seen: Arc<Mutex<Vec<Record>>> = Arc::default();
    let s2 = seen.clone();
    c.on_tick(move |ticks| s2.lock().unwrap().extend_from_slice(ticks));
    let ids = c.subscribe_symbols(&["EURUSD"]).await.unwrap();
    assert_eq!(ids, vec![1]);
    for _ in 0..50 {
        if seen.lock().unwrap().len() >= 2 { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ticks = seen.lock().unwrap().clone();
    assert_eq!(ticks.len(), 2);
    assert_eq!(ticks[0].get_str("symbol"), "EURUSD");
    assert!((ticks[0].get_f64("bid") - 1.15364).abs() < 1e-9, "scaled price normalised");
    assert!((ticks[1].get_f64("bid") - 1.15364).abs() < 1e-9, "unchanged bid carried forward");
    assert!((ticks[1].get_f64("ask") - 1.15370).abs() < 1e-9);
    let cached = c.symbol_info_tick("EURUSD").unwrap();
    assert!((cached.get_f64("ask") - 1.1537).abs() < 1e-9);
    assert_eq!(c.tick_history_stats().get_i64("total_ticks"), 2);
    assert_eq!(c.copy_ticks_from("EURUSD", 0i64, 10, 3).await.unwrap().len(), 2);

    // subscribing again sends the merged set
    c.subscribe_ticks(&[2]).await.unwrap();
    assert_eq!(b.subscribes.lock().unwrap().last().unwrap(), &vec![1, 2]);
    let mut h = c.subscribe_ticks_managed(&[1]).await.unwrap();
    h.unsubscribe().await.unwrap();
    assert_eq!(b.subscribes.lock().unwrap().last().unwrap(), &vec![2]);

    let pos = c.get_positions_and_orders().await.unwrap();
    assert!(pos.positions.is_empty() && pos.orders.is_empty());

    let r = c.buy_market("EURUSD", 0.01, &Default::default()).await.unwrap();
    assert!(r.success);
    assert_eq!((r.retcode, r.deal, r.order, r.volume, r.request_id), (10009, 555, 556, 1_000_000, 9));
    assert_eq!(r.comment, "done");
    assert!(c.buy_market("EURUSD", 0.0, &Default::default()).await.is_err());
    assert!(c.buy_limit("EURUSD", 0.1, 0.0, &Default::default()).await.is_err());
    c.close().await;
}

#[tokio::test]
async fn command_timeout_and_transport_not_ready() {
    let c = Mt5Client::new(ClientConfig { timeout: 0.3, ..cfg(start(Behavior { ignore_cmd: Some(20), ..Default::default() }).await) });
    assert!(matches!(c.ping().await, Err(Error::Session(_))));
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    assert!(matches!(c.get_spreads(&[]).await, Err(Error::Timeout(_))));
    c.ping().await.unwrap(); // connection still healthy afterwards
    c.close().await;
}

#[tokio::test]
async fn disconnect_callback_and_auto_reconnect_restores_subscriptions() {
    let b = Behavior { drop_after_login_once: true, ..Default::default() };
    let uri = start(b.clone()).await;
    let c = Mt5Client::new(ClientConfig {
        auto_reconnect: true, reconnect_delay: 0.05, max_reconnect_delay: 0.2, max_reconnect_attempts: 5, ..cfg(uri)
    });
    let dropped = Arc::new(AtomicUsize::new(0));
    let d2 = dropped.clone();
    c.on_disconnect(move || { d2.fetch_add(1, Ordering::SeqCst); });
    let ticks = Arc::new(AtomicUsize::new(0));
    let t2 = ticks.clone();
    c.on_tick(move |t| { t2.fetch_add(t.len(), Ordering::SeqCst); });
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    for _ in 0..100 {
        if b.logins.load(Ordering::SeqCst) >= 2 && c.is_connected() { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(b.logins.load(Ordering::SeqCst), 2, "re-logged in with stored credentials");
    assert!(c.is_connected());
    c.ping().await.unwrap();
    // listeners survived the reconnect (improvement over pymt5): ticks still reach on_tick
    c.load_symbols(true).await.unwrap();
    c.subscribe_ticks(&[1]).await.unwrap();
    for _ in 0..50 {
        if ticks.load(Ordering::SeqCst) >= 2 { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ticks.load(Ordering::SeqCst) >= 2);
    c.close().await;
}

#[tokio::test]
async fn panicking_push_callback_does_not_kill_receive_loop() {
    let c = Mt5Client::new(cfg(start(Behavior::default()).await));
    let errors = Arc::new(AtomicUsize::new(0));
    let e2 = errors.clone();
    c.on_callback_error(move |ce| { assert_eq!(ce.command, 8); e2.fetch_add(1, Ordering::SeqCst); });
    c.on_tick(|_| panic!("user callback bug"));
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    c.load_symbols(true).await.unwrap();
    c.subscribe_ticks(&[1]).await.unwrap();
    for _ in 0..50 {
        if errors.load(Ordering::SeqCst) >= 1 { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(errors.load(Ordering::SeqCst) >= 1);
    c.ping().await.unwrap();
    c.close().await;
}

#[test]
fn login_payload_matches_browser_capture_sizes() {
    let c = Mt5Client::new(ClientConfig::default());
    // build_command + AES + outer framing sizes from the Python smoke tests
    let inner = build_command(28, &vec![0u8; 912]);
    assert_eq!(inner.len(), 916);
    let enc = initial_cipher().encrypt(&inner);
    assert_eq!(enc.len(), 928);
    assert_eq!(pack_outer(&enc).len(), 936);
    drop(c);
}
