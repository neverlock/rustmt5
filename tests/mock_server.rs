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
use rustmt5::{ClientConfig, CloseOptions, Error, ModifyOrderOptions, Mt5Client, PendingOrderOptions, Record, TradeRequest};
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
    /// 0 = build-6182 style (4-byte ack + cmd 19 push), 1 = legacy immediate full response, 2 = ack only (no push)
    trade_mode: u8,
    /// every cmd=12 payload received
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// cmd=19 push as observed on build 6182: u32 token + request(236) + 12 filler + result(128).
fn trade_push(action_id: u32, retcode: u32, symbol: &str, volume: u64) -> Vec<u8> {
    let mut b = 0x014f930du32.to_le_bytes().to_vec();
    b.extend(SeriesCodec::serialize(&[
        SerField::u32(action_id), SerField::u32(1), SerField::fixed_string(symbol, 64), SerField::u64(volume), SerField::u32(5), SerField::u64(0),
        SerField::u32(0), SerField::u32(0), SerField::u32(0), SerField::u32(0), SerField::u32(0), SerField::f64(0.0), SerField::f64(0.0),
        SerField::f64(0.0), SerField::f64(0.0), SerField::u32(0), SerField::f64(0.0), SerField::f64(0.0), SerField::fixed_string("", 64),
        SerField::u64(0), SerField::u64(0), SerField::u32(0),
    ]).unwrap());
    b.extend(SeriesCodec::serialize(&[
        SerField::u32(retcode), SerField::new(PropType::I64, 555i64, None), SerField::new(PropType::I64, 556i64, None),
        SerField::new(PropType::I64, volume as i64, None), SerField::f64(1.15366), SerField::u32(9), SerField::f64(1.15364),
        SerField::f64(1.15366), SerField::f64(0.0), SerField::fixed_string(if retcode == 10009 { "done" } else { "rejected" }, 64),
    ]).unwrap());
    b
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
            18 => {
                // real Web Terminal format: [u32 count][u32 symbol_id]*  (name-based requests are rejected)
                let n = u32::from_le_bytes(payload[0..4].try_into().unwrap_or([0; 4]));
                if payload.len() != 4 + 4 * n as usize || n == 0 {
                    ws.send(Message::Binary(frame(&cipher, 18, 1, &[]).into())).await.unwrap();
                } else {
                    let id = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                    let mut body = 1u32.to_le_bytes().to_vec();
                    body.extend(record_bytes(
                        FULL_SYMBOL_SCHEMA,
                        FULL_SYMBOL_FIELD_NAMES,
                        &[("trade_symbol", Value::Str("EURUSD".into())), ("symbol_id", Value::UInt(id as u64)), ("digits", Value::UInt(5)),
                          ("point", Value::Float(0.00001)), ("contract_size", Value::Float(100000.0)), ("currency_profit", Value::Str("USD".into())),
                          ("trade", Value::Bytes(record_bytes(
                              ACCOUNT_WEB_TRADE_SETTINGS_SCHEMA,
                              ACCOUNT_WEB_TRADE_SETTINGS_FIELD_NAMES,
                              &[("trade_mode", Value::Int(4)), ("trade_exemode", Value::UInt(2)), ("trade_fill_flags", Value::UInt(3)),
                                ("volume_min", Value::UInt(1_000_000)), ("volume_step", Value::UInt(1_000_000)),
                                ("volume_max", Value::UInt(100_000_000_000))],
                          )))],
                    ));
                    ws.send(Message::Binary(frame(&cipher, 18, 0, &body).into())).await.unwrap();
                }
            }
            4 => {
                let pos = record_bytes(POSITION_SCHEMA, POSITION_FIELD_NAMES, &[
                    ("position_id", Value::Int(900)), ("trade_symbol", Value::Str("EURUSD".into())), ("trade_action", Value::UInt(0)),
                    ("price_open", Value::Float(1.1)), ("price_close", Value::Float(1.2)), ("sl", Value::Float(1.0)), ("tp", Value::Float(1.3)),
                    ("trade_volume", Value::UInt(1_000_000)), ("digits", Value::UInt(5)),
                ]);
                let ord = record_bytes(ORDER_SCHEMA, ORDER_FIELD_NAMES, &[
                    ("trade_order", Value::Int(901)), ("trade_symbol", Value::Str("EURUSD".into())), ("order_type", Value::UInt(2)),
                    ("volume_initial", Value::Int(2_000_000)), ("volume_current", Value::Int(2_000_000)), ("price_order", Value::Float(1.05)),
                    ("type_filling", Value::UInt(2)), ("digits", Value::UInt(5)),
                ]);
                let mut body = 1u32.to_le_bytes().to_vec();
                body.extend(pos);
                body.extend(1u32.to_le_bytes());
                body.extend(ord);
                body.extend([0u8; 12]); // trailer seen on build 6182
                ws.send(Message::Binary(frame(&cipher, 4, 0, &body).into())).await.unwrap();
            }
            12 => {
                assert_eq!(payload.len(), 248, "trade request must use the 248-byte Web Terminal layout");
                let action_id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                assert_ne!(action_id, 0, "client must assign a unique action_id");
                let type_filling = u32::from_le_bytes(payload[96..100].try_into().unwrap());
                let volume = u64::from_le_bytes(payload[72..80].try_into().unwrap());
                b.requests.lock().unwrap().push(payload.to_vec());
                let action = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                let flags = u32::from_le_bytes(payload[104..108].try_into().unwrap());
                let order = u64::from_le_bytes(payload[84..92].try_into().unwrap());
                let position = u64::from_le_bytes(payload[228..236].try_into().unwrap());
                // what a market-execution server accepts (behaviour observed on build 6182)
                let valid = match action {
                    3 => (flags == 2 && type_filling <= 1) || (flags == 3 && position != 0),
                    5 => type_filling == 2 && flags == 2,
                    6 => position != 0 && volume != 0,
                    7 | 8 => order != 0 && volume != 0,
                    10 => position != 0 && payload[236..244] != [0; 8],
                    _ => false,
                };
                let retcode = if type_filling == 7 { 10030 } else if valid { 10009 } else { 10013 };
                match b.trade_mode {
                    1 => {
                        let body = SeriesCodec::serialize(&[
                            SerField::u32(10009), SerField::u64(555), SerField::u64(556), SerField::u64(1_000_000), SerField::f64(1.15366),
                            SerField::f64(1.15364), SerField::f64(1.15366), SerField::fixed_string("done", 64), SerField::u32(9),
                        ])
                        .unwrap();
                        ws.send(Message::Binary(frame(&cipher, 12, 0, &body).into())).await.unwrap();
                    }
                    mode => {
                        ws.send(Message::Binary(frame(&cipher, 12, 0, &[0, 0, 0, 0]).into())).await.unwrap();
                        if mode == 0 {
                            ws.send(Message::Binary(frame(&cipher, 19, 0, &trade_push(action_id, retcode, "EURUSD", volume)).into())).await.unwrap();
                        }
                    }
                }
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
    assert_eq!((summary.leverage, summary.positions_count, summary.orders_count), (100, 1, 1));

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

    let full = c.get_full_symbol_info("EURUSD").await.unwrap().expect("full symbol info via id request");
    assert_eq!((full.get_i64("digits"), full.get_f64("contract_size"), full.get_str("currency_profit")), (5, 100000.0, "USD"));
    assert!(c.get_full_symbol_info("UNKNOWN").await.unwrap().is_none());
    assert_eq!(c.symbol_info("EURUSD").await.unwrap().unwrap().get_str("currency_profit"), "USD");

    // used margin is computed locally from the live positions (0.01 lot of a 100000 contract at 1:100 = 10)
    let am = c.account_margin().await.unwrap();
    assert!((am.get_f64("margin") - 10.0).abs() < 1e-9, "{am:?}");
    assert_eq!((am.get_i64("positions"), am.get_i64("unpriced"), am.get_str("margin_source")), (1, 0, "computed"));
    assert!((am.get_f64("margin_free") - (am.get_f64("equity") - 10.0)).abs() < 1e-9);
    // order_check: server volume limits are wire-encoded (0.01 lot = 1_000_000); user volumes are lots
    let req = |vol: f64| Record::new().with("action", 1i64).with("symbol", "EURUSD").with("volume", vol).with("type", 0i64).with("price", 1.1f64);
    let ok = c.order_check(&req(0.01)).await.unwrap();
    assert_eq!(ok.get_i64("retcode"), 0, "{ok:?}");
    assert!((ok.get_f64("margin") - 20.0).abs() < 1e-9, "open position (10) + this order (10): {ok:?}");
    assert!(ok.get_bool("margin_known"));
    assert_eq!(c.order_check(&req(0.37)).await.unwrap().get_i64("retcode"), 0);
    let low = c.order_check(&req(0.001)).await.unwrap();
    assert_eq!((low.get_i64("retcode"), low.get_str("comment").contains("below")), (10014, true));
    assert_eq!(c.order_check(&req(0.015)).await.unwrap().get_i64("retcode"), 10014);
    assert_eq!(c.order_check(&req(2000.0)).await.unwrap().get_i64("retcode"), 10014, "above symbol maximum");
    assert_eq!(c.order_check(&req(50.0)).await.unwrap().get_i64("retcode"), 10019, "not enough money");

    let pos = c.get_positions_and_orders().await.unwrap();
    assert_eq!((pos.positions.len(), pos.orders.len()), (1, 1), "the 12-byte trailer must not disturb parsing");
    assert_eq!(pos.positions[0].get_i64("position_id"), 900);
    assert_eq!(pos.orders[0].get_i64("trade_order"), 901);

    let r = c.buy_market("EURUSD", 0.01, &Default::default()).await.unwrap();
    assert!(r.success);
    assert_eq!((r.retcode, r.deal, r.order, r.volume), (10009, 555, 556, 1_000_000));
    assert_ne!(r.request_id, 0);
    assert_eq!(r.comment, "done");
    // the broker rejects the order asynchronously: success must be false (previously reported as success)
    let rejected = c.trade_request(&TradeRequest { trade_action: 1, symbol: "EURUSD".into(), volume: 1_000_000, type_filling: 7, ..Default::default() }).await.unwrap();
    assert_eq!((rejected.retcode, rejected.success, rejected.comment.as_str()), (10030, false, "rejected"));
    assert_eq!(rejected.description, "Invalid order filling type");
    // typed push event delivers the same result
    let seen_ev: Arc<Mutex<Vec<i64>>> = Arc::default();
    let s3 = seen_ev.clone();
    c.on_trade_result_event(move |e| s3.lock().unwrap().push(e.retcode));
    c.buy_market("EURUSD", 0.01, &Default::default()).await.unwrap();
    for _ in 0..50 { if !seen_ev.lock().unwrap().is_empty() { break; } tokio::time::sleep(Duration::from_millis(20)).await; }
    assert_eq!(seen_ev.lock().unwrap().first(), Some(&10009));
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

#[tokio::test]
async fn trade_result_legacy_immediate_and_timeout() {
    // legacy servers answer cmd=12 with the complete result
    let c = Mt5Client::new(cfg(start(Behavior { trade_mode: 1, ..Default::default() }).await));
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    c.load_symbols(true).await.unwrap();
    let r = c.buy_market("EURUSD", 0.01, &Default::default()).await.unwrap();
    assert_eq!((r.success, r.retcode, r.order), (true, 10009, 556));
    c.close().await;
    // ack but never a result push -> Timeout error (order may be live; caller must check)
    let c = Mt5Client::new(ClientConfig { timeout: 0.4, ..cfg(start(Behavior { trade_mode: 2, ..Default::default() }).await) });
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    c.load_symbols(true).await.unwrap();
    match c.buy_market("EURUSD", 0.01, &Default::default()).await {
        Err(Error::Timeout(m)) => assert!(m.contains("no final trade result push"), "{m}"),
        other => panic!("expected timeout, got {other:?}"),
    }
    c.ping().await.unwrap();
    c.close().await;
}

fn field_u32(req: &[u8], off: usize) -> u32 { u32::from_le_bytes(req[off..off + 4].try_into().unwrap()) }
fn field_u64(req: &[u8], off: usize) -> u64 { u64::from_le_bytes(req[off..off + 8].try_into().unwrap()) }
fn field_f64(req: &[u8], off: usize) -> f64 { f64::from_le_bytes(req[off..off + 8].try_into().unwrap()) }

#[tokio::test]
async fn order_helpers_build_web_terminal_requests() {
    let b = Behavior::default();
    let c = Mt5Client::new(cfg(start(b.clone()).await));
    c.connect().await.unwrap();
    c.login(1, "pw").await.unwrap();
    c.load_symbols(true).await.unwrap();

    // market buy in market-execution mode: action 3, type_flags 2, IOC (fill flags 3), price 0
    let r = c.buy_market("EURUSD", 0.01, &Default::default()).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u32(&req, 96), field_u32(&req, 104), field_u64(&req, 72), field_u32(&req, 92)), (3, 1, 2, 1_000_000, 0));
    assert_eq!(field_f64(&req, 112), 0.0);

    // pending order: RETURN filling by default, type_flags 2
    let r = c.buy_limit("EURUSD", 0.02, 1.0123456, &PendingOrderOptions::default()).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u32(&req, 96), field_u32(&req, 104), field_u32(&req, 92)), (5, 2, 2, 2));
    assert_eq!(field_f64(&req, 112), 1.01235, "price normalised to 5 digits");

    // close: looks the position up (sell side of a buy), type_flags 3, ticket + current price
    let r = c.close_position(900, None, &CloseOptions::default()).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u32(&req, 92), field_u32(&req, 104), field_u64(&req, 228), field_u64(&req, 72)), (3, 1, 3, 900, 1_000_000));
    assert_eq!(field_f64(&req, 112), 1.2, "current price of the position");
    assert!(matches!(c.close_position(12345, None, &Default::default()).await, Err(Error::Validation(_))));

    // cancel: resends symbol, volume, type and price of the existing order
    let r = c.cancel_pending_order(901).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u64(&req, 84), field_u64(&req, 72), field_u32(&req, 92)), (8, 901, 2_000_000, 2));
    assert_eq!(field_f64(&req, 112), 1.05);

    // modify: only the price changes, the rest comes from the existing order
    let r = c.modify_pending_order(901, &ModifyOrderOptions { price: Some(1.04), ..Default::default() }).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u64(&req, 72), field_f64(&req, 112)), (7, 2_000_000, 1.04));

    // SLTP: keeps the other level
    let r = c.modify_position_sltp(900, Some(1.05), None).await.unwrap();
    assert!(r.success, "{r:?}");
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u64(&req, 228), field_f64(&req, 128), field_f64(&req, 136)), (6, 900, 1.05, 1.3));

    // MetaTrader5-style dicts are translated to the server's representation
    let deal = Record::new().with("action", 1i64).with("symbol", "EURUSD").with("volume", 0.01f64).with("type", 0i64);
    assert!(c.order_send(&deal).await.unwrap().success);
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u32(&req, 104)), (3, 2), "DEAL on a market-execution symbol -> action 3");
    assert!(c.order_send(&Record::new().with("action", 1i64).with("symbol", "EURUSD").with("position", 900i64).with("type", 1i64)).await.unwrap().success);
    let req = b.requests.lock().unwrap().last().unwrap().clone();
    assert_eq!((field_u32(&req, 4), field_u32(&req, 104), field_u64(&req, 228)), (3, 3, 900), "DEAL with a position closes it");
    assert!(c.order_send(&Record::new().with("action", 8i64).with("order", 901i64)).await.unwrap().success);
    assert_eq!(field_u32(b.requests.lock().unwrap().last().unwrap(), 4), 8);
    assert!(c.order_send(&Record::new().with("action", 6i64).with("position", 900i64).with("tp", 1.4f64)).await.unwrap().success);
    assert!(matches!(c.order_send(&Record::new().with("action", 99i64)).await, Err(Error::Validation(_))));

    // the old pymt5 style (DEAL action on a market-execution symbol) is rejected by this server model
    let bad = c.trade_request(&TradeRequest { trade_action: 1, symbol: "EURUSD".into(), volume: 1_000_000, type_flags: 2, ..Default::default() }).await.unwrap();
    assert_eq!((bad.retcode, bad.success), (10013, false));
    c.close().await;
}

/// Reference `LinkConnector`: a plain tungstenite socket driven through the channel interface.
struct ChannelConnector;

impl rustmt5::LinkConnector for ChannelConnector {
    fn connect<'a>(&'a self, uri: &'a str) -> rustmt5::LinkFuture<'a> {
        Box::pin(async move {
            let (ws, _) = tokio_tungstenite::connect_async(uri).await.map_err(|e| e.to_string())?;
            let (mut sink, mut stream) = ws.split();
            let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            let (in_tx, in_rx) = tokio::sync::mpsc::channel::<rustmt5::LinkEvent>(64);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        outgoing = out_rx.recv() => match outgoing {
                            Some(bytes) => { if sink.send(Message::Binary(bytes.into())).await.is_err() { break; } }
                            None => { let _ = sink.close().await; return; }
                        },
                        incoming = stream.next() => match incoming {
                            Some(Ok(Message::Binary(b))) => { if in_tx.send(rustmt5::LinkEvent::Binary(b.to_vec())).await.is_err() { return; } }
                            Some(Ok(Message::Close(_))) | None => { let _ = in_tx.send(rustmt5::LinkEvent::Closed("closed".into())).await; return; }
                            Some(Err(e)) => { let _ = in_tx.send(rustmt5::LinkEvent::Closed(e.to_string())).await; return; }
                            Some(Ok(_)) => {}
                        },
                    }
                }
            });
            Ok(rustmt5::Link { tx: out_tx, rx: in_rx })
        })
    }
}

#[tokio::test]
async fn external_link_connector_handshake_login_ping() {
    let b = Behavior::default();
    let uri = start(b.clone()).await;
    let c = Mt5Client::new(ClientConfig { link_connector: Some(Arc::new(ChannelConnector)), ..cfg(uri) });
    c.connect().await.unwrap();
    assert_eq!(c.server_build(), 6182);
    c.login(12345678, "secret").await.unwrap();
    c.ping().await.unwrap();
    c.close().await;
    assert_eq!(b.logins.load(Ordering::SeqCst), 1);
}
