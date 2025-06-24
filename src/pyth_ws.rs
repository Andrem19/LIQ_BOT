// ─────────────────────────── src/pyth_ws.rs ───────────────────────────
use anyhow::{anyhow, Context, Result};
use bytes::BytesMut;
use futures::StreamExt;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{
    select,
    sync::watch,
    time::{sleep, Instant},
};

const HERMES: &str = "https://hermes.pyth.network";
const SSE_IDLE_TIMEOUT: u64 = 60;        // сек до reconnect
const RETRY_TIMEOUT:     u64 = 15;        // сек если hermes недоступен

/// Стартует поток цен для 1-го `price_feed_id`.
///
/// * `feed_id` — строка вида `"FsSMV...eHjF"` (смотрите в Pyth-доке
///   или получайте через `/v2/price_feeds?query=SOL/USD`).
///
/// Возвращает `watch::Receiver<Option<f64>>`.
/// * `Some(price)` — свежая цена.
/// * `None`        — поток мёртв, используйте fallback HTTP/Whirlpool.
pub async fn subscribe(
    feed_id: String,
) -> Result<watch::Receiver<Option<f64>>> {
    // канал, через который отправляем цену наружу
    let (tx, rx) = watch::channel::<Option<f64>>(None);

    // держим feed_id и tx в Arc, чтобы передать в spawn-задачу
    let feed_id = Arc::new(feed_id);
    let tx_arc  = Arc::new(tx);

    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);

        loop {
            // -----------------------------------------------------------------
            // 1. Подключаемся к Hermes SSE
            // -----------------------------------------------------------------
            let url = format!(
                "{HERMES}/v2/updates/price/stream?parsed=true&ids[]={}",
                feed_id
            );
            match reqwest::Client::new().get(&url).send().await {
                Ok(mut resp) if resp.status().is_success() => {
                    backoff = Duration::from_secs(1); // сброс back-off
                    let mut stream = resp.bytes_stream();
                    let mut buf    = BytesMut::new();
                    let mut last   = Instant::now();

                    loop {
                        select! {
                            maybe = stream.next() => match maybe {
                                Some(Ok(chunk)) => {
                                    last = Instant::now();
                                    buf.extend_from_slice(&chunk);

                                    while let Some(pos) = find_double_nl(&buf) {
                                        let block = buf.split_to(pos + 2);
                                        if let Some(js) = extract_data(&block) {
                                            if let Some(p) = parse_price(&js) {
                                                // отправляем наружу; игнорируем, если rx уже дропнут
                                                let _ = tx_arc.send_replace(Some(p));
                                            }
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    eprintln!("pyth_ws: stream read err: {e:?}");
                                    break;          // переподключаемся
                                }
                                None => break,       // сервер закрыл соединение
                            },

                            _ = sleep(Duration::from_secs(SSE_IDLE_TIMEOUT)) => {
                                if last.elapsed().as_secs() >= SSE_IDLE_TIMEOUT {
                                    eprintln!("pyth_ws: idle > {SSE_IDLE_TIMEOUT}s, reconnect");
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(resp) => {
                    eprintln!("pyth_ws: HTTP {}", resp.status());
                }
                Err(e) => {
                    eprintln!("pyth_ws: connect err: {e:?}");
                }
            }

            // поток умер — даём знать потребителю
            let _ = tx_arc.send_replace(None);

            eprintln!("pyth_ws: retry in {backoff:?}");
            sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });

    Ok(rx)
}

// ──────────────── helpers ────────────────
fn find_double_nl(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

fn extract_data(block: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(block).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("data:").map(|s| s.trim().to_string()))
}

fn parse_price(js: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(js).ok()?;
    let arr = v.get("parsed")?.as_array()?;
    let first = arr.first()?;
    let price_obj = first.get("price")?;
    let mant = price_obj.get("price")?.as_str()?.parse::<f64>().ok()?;
    let expo = price_obj.get("expo")?.as_i64()?;
    Some(mant * 10f64.powi(expo as i32))
}
