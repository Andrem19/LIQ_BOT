// src/main.rs

pub mod telegram;
pub mod params;
pub mod get_info;
pub mod exchange;
pub mod types;
pub mod swap;
pub mod net;

use anyhow::Result;
use params::POOLS;
use get_info::fetch_pool_position_info;
use swap::execute_swap;
use types::PoolPositionInfo;

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Получаем информацию по позиции
    let pool_cfg = &POOLS[0];
    // let _info: PoolPositionInfo = fetch_pool_position_info(pool_cfg).await?;

    // 2) Совершаем своп: продаём 50 USDC (mint_B) → покупаем WSOL (mint_A)
    let result = swap::execute_swap(pool_cfg, pool_cfg.mint_B, pool_cfg.mint_A, 10.0).await?;
    println!(
        "После свопа — USDC: {:.6}, WSOL: {:.9}",
        result.balance_sell,
        result.balance_buy
    );

    Ok(())
}
