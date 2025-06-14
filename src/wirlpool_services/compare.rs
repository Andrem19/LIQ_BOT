//! Аналитика любых Orca Whirlpool-пулов — данные считаются on-chain.
//!
//! ⚙️ Требуется RPC endpoint (RPC_URL в `params.rs`), тот же что и в get_info.
//! 📦 Кэш суточных feeGrowth хранится в `./.cache/fees_<pool>.json`.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use crate::{
    wirlpool_services::net::http_client,
    params::{POOL, RPC_URL},
};
use crate::types::PoolConfig;

const Q64: f64 = 1.8446744073709552e19;

/* ─────────────────────── utilities ─────────────────────── */

 async fn price_jup(mint: &str) -> Result<f64> {
        // USDC всегда = 1
        if mint == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" {
            return Ok(1.0);
        }
        let url = format!("https://api-v3.raydium.io/mint/price?mints={mint}");
        let resp = http_client().get(&url).send().await?;
        let v: serde_json::Value = resp.json().await?;
        v["data"][mint]
            .as_str()
            .ok_or_else(|| anyhow!("no price"))?
            .parse::<f64>()
            .map_err(|e| anyhow!("parse price: {}", e))
    }

/* ─────────────── on-chain TVL / fee delta ─────────────── */

#[derive(Clone, Debug)]
struct StatsOnchain {
    tvl: f64,
    fees_24h: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct FeeSnapshot {
    ts: u64,
    fee_a: u128,
    fee_b: u128,
}

fn cache_path(addr: &str) -> PathBuf {
    let mut p = PathBuf::from(".cache");
    fs::create_dir_all(&p).ok();
    p.push(format!("fees_{addr}.json"));
    p
}

fn read_snapshot(addr: &str) -> Option<FeeSnapshot> {
    fs::read_to_string(cache_path(addr)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn save_snapshot(addr: &str, snap: &FeeSnapshot) {
    if let Ok(s) = serde_json::to_string(snap) {
        let _ = fs::write(cache_path(addr), s);
    }
}

async fn stats_onchain(pool: &PoolConfig) -> Result<StatsOnchain> {
    use orca_whirlpools_client::Whirlpool;
    let rpc = RpcClient::new(RPC_URL.to_string());

    let whirl_addr = Pubkey::from_str(pool.pool_address)?;
    let whirl_acc = rpc.get_account(&whirl_addr)?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&whirl_acc.data)?;

    // Балансы в ваксе
    let bal_a = rpc.get_token_account_balance(&whirl.token_vault_a)?.amount.parse::<u128>()?;
    let bal_b = rpc.get_token_account_balance(&whirl.token_vault_b)?.amount.parse::<u128>()?;

    let price_a = price_jup(pool.mint_a).await?;
    let price_b = price_jup(pool.mint_b).await?;

    let tvl = bal_a as f64 / 10f64.powi(pool.decimal_a as i32) * price_a
        + bal_b as f64 / 10f64.powi(pool.decimal_b as i32) * price_b;

    /* ------------ fee delta ------------ */
    let fee_snap = FeeSnapshot {
        ts: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        fee_a: whirl.fee_growth_global_a,
        fee_b: whirl.fee_growth_global_b,
    };
    let fees_24h = if let Some(prev) = read_snapshot(pool.pool_address) {
        if fee_snap.ts > prev.ts + 86_000 {
            // прошло ~24 ч
            let d_a = (fee_snap.fee_a - prev.fee_a) as f64 / Q64 * price_a;
            let d_b = (fee_snap.fee_b - prev.fee_b) as f64 / Q64 * price_b;
            save_snapshot(pool.pool_address, &fee_snap);
            Some(d_a + d_b)
        } else {
            // слишком свежий снапшот
            None
        }
    } else {
        // первого раза достаточно сохранить
        save_snapshot(pool.pool_address, &fee_snap);
        None
    };

    Ok(StatsOnchain { tvl, fees_24h })
}

/* ─────────────────────────── main ───────────────────────── */

pub async fn compare_pools(initial_amount_usdc: f64) -> Result<()> {
    let mut rows = Vec::new();

    match stats_onchain(&POOL).await {
        Ok(s) => rows.push((POOL, s)),
        Err(e) => eprintln!("⚠️  on-chain error {}: {}", POOL.pool_address, e),
    }
    if rows.is_empty() {
        return Err(anyhow!("не удалось получить ни одного пула"));
    }

    /* сортируем: если fees_24h есть → по доходности, иначе в конец */
    rows.sort_by(|a, b| {
        match (a.1.fees_24h, b.1.fees_24h) {
            (Some(fa), Some(fb)) => (fb / b.1.tvl).partial_cmp(&(fa / a.1.tvl)).unwrap(),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
    });

    println!("\n========= Whirlpool SOL/USDC (on-chain) =========");
    println!("{:<4}{:<44}{:>14}{:>14}{:>14}{:>10}",
        "#","Pool address","TVL","Fees 24h","Yield %","Profit");
    for (i,(cfg,s)) in rows.iter().enumerate() {
        let y = s.fees_24h.map(|f| f / s.tvl * 100.0);
        let pr = y.map(|y| initial_amount_usdc * y / 100.0);
        println!(
            "{:<4}{:<44}{:>14.2}{:>14}{:>9}{:>10}",
            i+1,
            cfg.pool_address,
            s.tvl,
            s.fees_24h.map(|v|format!("{:.2}",v)).unwrap_or_else(||"—".into()),
            y.map(|v|format!("{:.3}",v)).unwrap_or_else(||"—".into()),
            pr.map(|v|format!("{:.2}",v)).unwrap_or_else(||"—".into()),
        );
    }
    println!("\n(*) Если `Fees 24h` = \"—\", значит первый запуск: нужно подождать сутки для оценки доходности.");
    Ok(())
}
