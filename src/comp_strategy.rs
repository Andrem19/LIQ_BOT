use anyhow::{Context, Result};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};

use crate::exchange::helpers::{get_kline, Candle};   // ← ✅ берём единый Candle

/// ------------------------------------------------------------------
///  Основная функция: возвращает Arc<RwLock<Vec<Candle>>>
/// ------------------------------------------------------------------
pub async fn stream_candles(
    symbol:   &str,   // "SOLUSDT"
    interval: u32,    // в минутах, 1 3 5 …
    limit:    usize,  // сколько баров в истории хотим держать
) -> Result<Arc<RwLock<Vec<Candle>>>> {
    // 1) начальная история REST’ом
    let mut candles = get_kline(symbol, limit, interval).await
        .context("initial REST download")?;
    candles.sort_by_key(|c| c.timestamp);

    // 2) общая точка данных
    let store = Arc::new(RwLock::new(candles));

    // 3) фоновый веб-сокет куда будем дописывать бары
    let sym_lower = symbol.to_ascii_lowercase();
    let ws_url = format!(
        "wss://fstream.binance.com/ws/{}@kline_{}m",
        sym_lower, interval
    );

    let ws_store = Arc::clone(&store);
    tokio::spawn(async move {
        loop {
            match connect_async(&ws_url).await {
                Ok((mut ws, _)) => {
                    while let Some(msg) = ws.next().await {
                        match msg {
                            Ok(Message::Text(txt)) => {
                                if let Err(e) = handle_ws_msg(&txt, &ws_store, limit).await {
                                    eprintln!("WS-parse error: {e:?}");
                                }
                            }
                            Ok(Message::Ping(p))  => { let _ = ws.send(Message::Pong(p)).await; }
                            Ok(Message::Close(_)) => break,       // переподключаемся
                            _ => {}
                        }
                    }
                }
                Err(e) => eprintln!("WS-connect error: {e:?}"),
            }
            sleep(Duration::from_secs(5)).await; // пауза и retry
        }
    });

    Ok(store)   // ← ✅ возвращаем Arc<RwLock<Vec<Candle>>>
}

/// ------------------------------------------------------------------
///  Парсим одно сообщение сокета и обновляем вектор свечей
/// ------------------------------------------------------------------
async fn handle_ws_msg(
    txt: &str,
    store: &Arc<RwLock<Vec<Candle>>>,
    limit: usize,
) -> Result<()> {
    let v: Value = serde_json::from_str(txt)?;
    let k = &v["k"];                       // под-объект kline в сообщении

    // x == true → свеча закрылась; false → всё ещё текущая
    let closed = k["x"].as_bool().unwrap_or(false);
    let ts     = k["t"].as_i64().unwrap_or(0);

    let mut w = store.write().await;

    if let Some(last) = w.last_mut() {
        if last.timestamp == ts {
            // обновляем текущую свечу
            last.high   = last.high.max(k["h"].as_str().unwrap_or("0").parse::<f64>()?);
            last.low    = last.low.min(k["l"].as_str().unwrap_or("0").parse::<f64>()?);
            last.close  = k["c"].as_str().unwrap_or("0").parse::<f64>()?;
            last.volume = k["v"].as_str().unwrap_or("0").parse::<f64>()?;
            return Ok(());
        }
    }

    // если пришла _закрытая_ новая свеча ‒ добавляем её
    if closed {
        let open   = k["o"].as_str().unwrap_or("0").parse::<f64>()?;
        let high   = k["h"].as_str().unwrap_or("0").parse::<f64>()?;
        let low    = k["l"].as_str().unwrap_or("0").parse::<f64>()?;
        let close  = k["c"].as_str().unwrap_or("0").parse::<f64>()?;
        let volume = k["v"].as_str().unwrap_or("0").parse::<f64>()?;
        w.push(Candle { timestamp: ts, open, high, low, close, volume });
        if w.len() > limit {
            w.remove(0); // держим нужную длину
        }
    }

    Ok(())
}
