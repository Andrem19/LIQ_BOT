use chrono::Local;
use reqwest::Client;
use serde_json::Value;
use anyhow::{Context, Result};
use ta::indicators::AverageTrueRange;
use ta::{DataItem, Next};
use ta::indicators::RelativeStrengthIndex;
use crate::params;

use std::{sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::sleep};

/// --- ПАРАМЕТРЫ -----------------------------------------------------------------
const LOOKBACK_1M: usize = 30;    // сколько 1-минуток держим в расчёте
const ATR_PER: usize     = 14;
const RSI_PER: usize     = 14;

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



/// RSI по тем же исходным векторам, что и ATR
pub fn get_rsi(
    o: &[f64], h: &[f64], l: &[f64], c: &[f64], v: &[f64],
    period: usize,
) -> Result<Vec<f64>> {
    let len = c.len();
    if len == 0 || h.len()!=len || l.len()!=len || o.len()!=len || v.len()!=len {
        anyhow::bail!("Входные векторы должны быть непустыми и одинаковой длины");
    }

    let mut rsi = RelativeStrengthIndex::new(period)
        .expect("Период RSI > 0");

    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let di = DataItem::builder()
            .open(o[i]).high(h[i]).low(l[i]).close(c[i]).volume(v[i])
            .build().unwrap();
        out.push(rsi.next(&di));
    }
    Ok(out)
}


pub async fn run_indicators<F>(
    candles_arc: Arc<RwLock<Vec<Candle>>>,
    tick_every:  Duration,
    mut on_update: F,                         // ваша callback-логика
) -> !
where
    F: FnMut(&[f64], &[f64]) + Send + 'static,
{
    loop {
        //------------------ 1. берём последние 30 баров -------------------------
        let src: Vec<Candle> = {
            let g = candles_arc.read().await;
            g.iter()
                .rev()
                .take(LOOKBACK_1M)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };

        if src.len() == LOOKBACK_1M {
            //---------------- 2. разбиваем на OHLCV-векторы -----------------
            let (opens, highs, lows, closes, volumes) =
                src.iter()
                .map(|c| (c.open, c.high, c.low, c.close, c.volume))
                .unzip5();

            //---------------- 3. конвертируем 1m → 5m -----------------------
            let (o5, h5, l5, c5, v5) = convert_timeframe(&opens, &highs, &lows, &closes, &volumes, 1, 5);

            //---------------- 4. считаем ATR и RSI --------------------------
            if let (Ok(atr), Ok(rsi)) = (get_atr(&o5, &h5, &l5, &c5, &v5, ATR_PER),
                                         get_rsi(&o5, &h5, &l5, &c5, &v5, RSI_PER))
            {
                on_update(&atr, &rsi);        // ← вызываем ваш код
            }
        }

        sleep(tick_every).await;
    }
}

/// ------------------------------------------------------------------------------
/// небольшой «внутренний» макрос – заменяет itertools::unzip_n
/// ------------------------------------------------------------------------------
pub trait Unzip5<A,B,C,D,E> {
    fn unzip5(self) -> (Vec<A>,Vec<B>,Vec<C>,Vec<D>,Vec<E>);
}

impl<I,A,B,C,D,E> Unzip5<A,B,C,D,E> for I
where
    I: Iterator<Item = (A,B,C,D,E)>,
{
    fn unzip5(self) -> (Vec<A>,Vec<B>,Vec<C>,Vec<D>,Vec<E>) {
        let mut v1 = Vec::new();
        let mut v2 = Vec::new();
        let mut v3 = Vec::new();
        let mut v4 = Vec::new();
        let mut v5 = Vec::new();
        for (a,b,c,d,e) in self {
            v1.push(a);
            v2.push(b);
            v3.push(c);
            v4.push(d);
            v5.push(e);
        }
        (v1,v2,v3,v4,v5)
    }
}



pub fn decide(atr: Vec<f64>) -> bool {
    if atr[atr.len()-1] < 0.40 {
        return true
    } else {
        return false
    }
}


/// Ошибки, которые может возвращать функция `range_coefficient`.
#[derive(Debug)]
pub enum RangeError {
    /// Параметр `period` должен быть > 0.
    InvalidPeriod,
    /// Входные срезы имеют разную длину.
    LengthMismatch,
}

/// Режим подсчёта пересечения свечей с диапазоном.
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Полностью лежат внутри [lower, upper].
    Full,
    /// Хоть сколько-нибудь пересекаются с [lower, upper].
    Intersect,
}

pub fn range_coefficient(
    opens: &[f64],
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    period: usize,
    lower: f64,
    upper: f64,
    mode: Mode,
) -> Result<f64, RangeError> {
    // Проверка корректности периода
    if period == 0 {
        return Err(RangeError::InvalidPeriod);
    }

    // Проверка, что все срезы одной длины
    let n = highs.len();
    if opens.len() != n || lows.len() != n || closes.len() != n {
        return Err(RangeError::LengthMismatch);
    }

    // Определяем, сколько свечей реально будем анализировать
    let actual_period = if n >= period { period } else { n };
    let start = n - actual_period;

    // Считаем количество попавших свечей
    let mut count = 0usize;
    for i in start..n {
        let h = highs[i];
        let l = lows[i];

        match mode {
            Mode::Full => {
                if l >= lower && h <= upper {
                    count += 1;
                }
            }
            Mode::Intersect => {
                if h >= lower && l <= upper {
                    count += 1;
                }
            }
        }
    }

    // Возвращаем коэффициент вхождения
    Ok(count as f64 / actual_period as f64)
}


pub fn calculate_price_bounds(current_price: f64) -> (f64, f64) {
    let pct_list = match params::PCT_NUMBER {
        1 => &params::PCT_LIST_1,
        2 => &params::PCT_LIST_2,
        other => panic!("Unsupported PCT_NUMBER: {}", other),
    };

    let sum_up   = pct_list[0] + pct_list[2];
    let sum_down = pct_list[1] + pct_list[3];

    let upper_price = current_price * (1.0 + sum_up);
    let lower_price = current_price * (1.0 - sum_down);

    (upper_price, lower_price)
}