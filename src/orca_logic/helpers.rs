use crate::types::{RangeAlloc, PriceBound, BoundType};
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::time::{Duration, Instant};
use crate::params::WSOL as SOL_MINT;
use reqwest::Client;

pub fn calc_bound_prices_struct(base_price: f64, pct_list: &[f64]) -> Vec<PriceBound> {
    assert!(pct_list.len() == 4, "Нужно ровно 4 процента: [верх_внутр, низ_внутр, верх_экстр, низ_экстр]");

    // Верхняя внутренняя граница (немного выше рынка)
    let upper_inner = base_price * (1.0 + pct_list[0]);
    // Нижняя внутренняя граница (немного ниже рынка)
    let lower_inner = base_price * (1.0 - pct_list[1]);
    // Верхняя экстремальная (ещё выше)
    let upper_outer = upper_inner * (1.0 + pct_list[2]);
    // Нижняя экстремальная (ещё ниже)
    let lower_outer = lower_inner * (1.0 - pct_list[3]);

    vec![
        PriceBound { bound_type: BoundType::UpperOuter, value: upper_outer },
        PriceBound { bound_type: BoundType::UpperInner, value: upper_inner },
        PriceBound { bound_type: BoundType::LowerInner, value: lower_inner },
        PriceBound { bound_type: BoundType::LowerOuter, value: lower_outer },
    ]
}



pub fn calc_range_allocation_struct(
    price: f64,
    bounds: &[PriceBound],
    weights: &[f64; 3],
    total_usdc: f64,
) -> Vec<RangeAlloc> {
    // Достаем нужные границы по типу:
    let get = |t: BoundType| bounds.iter().find(|b| b.bound_type == t).unwrap().value;

    let upper_outer = get(BoundType::UpperOuter);
    let upper_inner = get(BoundType::UpperInner);
    let lower_inner = get(BoundType::LowerInner);
    let lower_outer = get(BoundType::LowerOuter);

    // Верхний диапазон — между upper_outer и upper_inner
    let upper_weight = total_usdc * weights[0] / 100.0;
    let upper_sol = upper_weight / price;
    let upper = RangeAlloc {
        range_idx: 0,
        range_type: "upper",
        usdc_amount: 0.0,
        sol_amount: upper_sol,
        usdc_equivalent: upper_sol * price,
        upper_price: upper_outer,
        lower_price: upper_inner,
    };

     // 2) Центральный диапазон — по формуле CLMM
    //    доля в USDC и SOL зависит от положения price в [lower_inner, upper_inner]
    let center_weight = total_usdc * weights[1] / 100.0;
    let sqrt_p = price.sqrt();
    let sqrt_l = lower_inner.sqrt();
    let sqrt_u = upper_inner.sqrt();
    let span = sqrt_u - sqrt_l;

    // значение в USDC-части (token0)
    let usdc_val = center_weight * (sqrt_u - sqrt_p) / span;
    // значение в SOL-части в терминах USDC
    let sol_val_usd = center_weight * (sqrt_p - sqrt_l) / span;
    // переводим USDC-эквивалент SOL в количество SOL
    let center_sol = sol_val_usd / price;

    let center = RangeAlloc {
        range_idx: 1,
        range_type: "center",
        usdc_amount: center_weight-usdc_val,
        sol_amount: center_sol,
        usdc_equivalent: center_weight, // всегда равно сумме частей
        upper_price: upper_inner,
        lower_price: lower_inner,
    };


    // Нижний диапазон — между lower_inner и lower_outer
    let lower_weight = total_usdc * weights[2] / 100.0;
    let lower = RangeAlloc {
        range_idx: 2,
        range_type: "lower",
        usdc_amount: lower_weight,
        sol_amount: 0.0,
        usdc_equivalent: lower_weight,
        upper_price: lower_inner,
        lower_price: lower_outer,
    };

    vec![upper, center, lower]
}

/// Кэшируем цену на 30 сек., чтобы не ддосить внешние сервисы
static PRICE_CACHE: Lazy<tokio::sync::RwLock<(f64, Instant)>> =
    Lazy::new(|| tokio::sync::RwLock::new((0.0, Instant::now() - Duration::from_secs(60))));

/// Возвращает актуальную цену 1 SOL в USD (сначала Jupiter, резерв — CoinGecko)
///
/// *Цена кэшируется на 30 секунд.*
pub async fn get_sol_price_usd() -> Result<f64> {
    // -------- 0. быстрый взгляд в кэш -----------------------------------
    {
        let rd = PRICE_CACHE.read().await;
        if rd.1.elapsed() < Duration::from_secs(30) && rd.0 > 0.0 {
            return Ok(rd.0);
        }
    }

    let client = http_client(); // ваша вспомогательная функция (reqwest::Client)

    // -------- 1. пробуем Jupiter lite-api --------------------------------
    let url = format!(
        "https://lite-api.jup.ag/price/v2?ids={SOL_MINT}"
    );
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            let body = resp.text().await?;
            let v: Value = serde_json::from_str(&body)
                .map_err(|e| anyhow!("Jupiter price JSON parse error: {e}  body={body}"))?;
            if let Some(p_str) = v["data"][SOL_MINT]["price"].as_str() {
                if let Ok(price) = p_str.parse::<f64>() {
                    cache_price(price).await;
                    return Ok(price);
                }
            }
        }
    }

    // -------- 2. fallback → CoinGecko ------------------------------------
    let cg_url = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";
    let resp = client.get(cg_url).send().await?;
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| anyhow!("CoinGecko JSON parse error: {e}  body={body}"))?;
    if let Some(price) = v["solana"]["usd"].as_f64() {
        cache_price(price).await;
        return Ok(price);
    }

    Err(anyhow!("cannot fetch SOL/USD price from Jupiter or CoinGecko"))
}

/// сохраняем значение в кэше
async fn cache_price(p: f64) {
    let mut wr = PRICE_CACHE.write().await;
    *wr = (p, Instant::now());
}

pub fn http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client")
}