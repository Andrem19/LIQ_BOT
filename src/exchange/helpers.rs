use chrono::Local;
use reqwest::Client;
use serde_json::Value;
use anyhow::{Context, Result};
use ta::indicators::AverageTrueRange;
use ta::{DataItem, Next};

/// Представление одной 5-минутной свечи
#[derive(Debug, Clone, Copy)]
pub struct Candle {
    pub timestamp: i64,
    pub open:      f64,
    pub high:      f64,
    pub low:       f64,
    pub close:     f64,
    pub volume:    f64,
}

/// Запрос последних `limit` баров для символа `symbol` с интервалом `interval` минут.
pub async fn get_kline(
    symbol: &str,
    limit: usize,
    interval: u32,
) -> Result<Vec<Candle>> {
    let endpoint = format!(
        "/fapi/v1/klines?symbol={}&interval={}m&limit={}",
        symbol, interval, limit
    );
    let url = format!("https://fapi.binance.com{}", endpoint);
    let client = Client::new();

    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Не удалось выполнить запрос klines для {}", symbol))?;
    let raw: Vec<Vec<Value>> = resp
        .json()
        .await
        .with_context(|| format!("Не удалось распарсить JSON klines для {}", symbol))?;

    let mut candles = Vec::with_capacity(raw.len());
    for item in raw {
        if item.len() < 6 { continue; }
        let ts = item[0].as_i64().unwrap_or(0);
        let parse_f64 = |v: &Value| {
            v.as_str()
             .and_then(|s| s.parse::<f64>().ok())
             .unwrap_or(0.0)
        };
        let open   = parse_f64(&item[1]);
        let high   = parse_f64(&item[2]);
        let low    = parse_f64(&item[3]);
        let close  = parse_f64(&item[4]);
        let volume = parse_f64(&item[5]);
        candles.push(Candle { timestamp: ts, open, high, low, close, volume });
    }
    Ok(candles)
}

pub fn percentage_change(base: f64, other: f64) -> f64 {
    if base == 0.0 {
        panic!("Невозможно вычислить изменение: базовое значение (base) равно нулю");
    }
    (other - base) / base
}

/// Агрегирует массивы OHLCV в более крупный таймфрейм.
/// `timeframe` — сколько входных баров в одном выходном (например, 12 × 5m = 1h).
/// Если `ln == 0`, длина = ceil(n / timeframe) (неполные группы тоже включаем).
pub fn convert_timeframe(
    opens:   &[f64],
    highs:   &[f64],
    lows:    &[f64],
    closes:  &[f64],
    volumes: &[f64],
    timeframe: usize,
    ln: usize,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let n = opens.len();
    let length = if ln == 0 {
        (n + timeframe - 1) / timeframe
    } else {
        ln
    };
    let mut new_o = vec![0.0; length];
    let mut new_h = vec![f64::MIN; length];
    let mut new_l = vec![f64::MAX; length];
    let mut new_c = vec![0.0; length];
    let mut new_v = vec![0.0; length];

    for i in 0..length {
        let end = n.saturating_sub(i * timeframe);
        let start = end.saturating_sub(timeframe);
        let max_h = highs[start..end].iter().cloned().fold(f64::MIN, f64::max);
        let min_l = lows[start..end].iter().cloned().fold(f64::MAX, f64::min);
        let sum_v = volumes[start..end].iter().sum();
        let dst = length - 1 - i;
        new_o[dst] = opens[start];
        new_h[dst] = max_h;
        new_l[dst] = min_l;
        new_c[dst] = closes[end - 1];
        new_v[dst] = sum_v;
    }
    (new_o, new_h, new_l, new_c, new_v)
}

/// Основная функция: возвращает вектор ATR по часовому таймфрейму.
/// - `symbol` — торговая пара (e.g. "SOLUSDT")  
/// - `five_min_limit` — сколько 5-минутных баров скачать (мы возьмём 400 по умолчанию)  
/// - `timeframe` — сколько 5-минуток в одном баре (12 = 1h)  
/// - `atr_period` — период ATR (14 по умолчанию)
pub fn get_atr(
    o1h: &Vec<f64>,
    h1h: &Vec<f64>,
    l1h: &Vec<f64>,
    c1h: &Vec<f64>,
    v1h: &Vec<f64>,
    atr_period: usize,
) -> Result<Vec<f64>> {
    let len = o1h.len();
    if len == 0
        || h1h.len() != len
        || l1h.len() != len
        || c1h.len() != len
        || v1h.len() != len
    {
        return Err(anyhow::anyhow!(
            "Входные векторы должны быть непустыми и одинаковой длины"
        ));
    }

    let mut atr = AverageTrueRange::new(atr_period)
        .expect("Период ATR должен быть больше 0");

    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let di = DataItem::builder()
            .open(o1h[i])
            .high(h1h[i])
            .low(l1h[i])
            .close(c1h[i])
            .volume(v1h[i])
            .build()
            .unwrap();
        result.push(atr.next(&di));
    }

    Ok(result)
}