// // src/main.rs

// pub mod telegram;
// pub mod params;
// pub mod get_info;
// pub mod exchange;
// pub mod types;
// pub mod swap;
// pub mod net;
// pub mod open_position;
// pub mod compare;

// use anyhow::Result;
// use params::POOLS;
// use get_info::fetch_pool_position_info;
// use swap::execute_swap;
// use types::PoolPositionInfo;

// #[tokio::main]
// async fn main() -> Result<()> {
//     // 1) Получаем информацию по позиции
//     let pool_cfg = &POOLS[0];
//     // compare::compare_pools().await?;
//     // if pool_cfg.position_address.is_some() {
//     //     let _info: PoolPositionInfo = fetch_pool_position_info(pool_cfg).await?;
//     // } else {
//     //     println!("⚠️  Позиция ещё не открыта — пропускаем получение информации.");
//     // }

//     // 2) Совершаем своп: продаём 50 USDC (mint_B) → покупаем WSOL (mint_A)
//     // let result = swap::execute_swap(pool_cfg, pool_cfg.mint_B, pool_cfg.mint_A, 10.0).await?;
//     // println!(
//     //     "После свопа — USDC: {:.6}, WSOL: {:.9}",
//     //     result.balance_sell,
//     //     result.balance_buy
//     // );

//     Ok(())
// }
// src/main.rs

pub mod telegram;
pub mod params;
pub mod get_info;
pub mod exchange;
pub mod types;
pub mod swap;
pub mod net;
pub mod open_position;

use anyhow::Result;
use std::time::Duration;
use tokio::time::sleep;
use crate::exchange::hyperliquid::hl::HL;
use dotenv::dotenv;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    // 1. Инициализация HL-клиента из переменных окружения.
    // Перед запуском убедитесь, что в окружении установлены:
    //   HLSECRET_1 — ваш приватный ключ в hex
    //   (опционально) HL_SUBACCOUNT — адрес саб-аккаунта в hex
    let mut hl = HL::new_from_env().await?;

    // Торгуемая пара
    let symbol = "SOLUSDT";

    // 2. Узнаём текущую цену
    let price = hl.get_last_price(symbol).await?;
    println!("Current price for {}: {}", symbol, price);

    // 3. Открываем длинную позицию на 100 USDT
    //    reduce_only = false, amount_coins не используется в режиме открытия
    let (open_cloid, entry_price) = hl
        .open_market_order(symbol, "Buy", 40.0, false, 0.0)
        .await?;
    println!("Opened long position: cloid = {}, entry_price = {}", open_cloid, entry_price);
    println!("Wait 10 sec");
    sleep(Duration::from_secs(10)).await;
    // 4. Берём размер позиции из API, чтобы выставить стоп-лосс
    let position = hl
        .get_position(symbol)
        .await?
        .expect("Position not found after opening");
    println!("Position details: {:?}", position);

    // 5. Ставим стоп-лосс 1% от цены входа
    hl.open_sl(symbol, "Buy", position.size, position.entry_px, 0.01)
        .await?;
    println!("Stop-loss set at 1%");

    // 6. Ждём одну минуту
    sleep(Duration::from_secs(60)).await;

    // 7. Узнаём текущий нереализованный PnL
    let position_after = hl
        .get_position(symbol)
        .await?
        .expect("Position disappeared");
    println!(
        "Unrealized PnL after 1 minute: {}",
        position_after.unrealized_pnl
    );

    // 8. Закрываем позицию по маркету (reduce_only = true, передаём размер)
    let (close_cloid, close_price) = hl
        .open_market_order(symbol, "Sell", 0.0, true, position_after.size)
        .await?;
    println!(
        "Closed position: cloid = {}, avg_exit_price = {}",
        close_cloid, close_price
    );

    Ok(())
}
