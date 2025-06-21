mod params;
mod types;
mod orca_logic;
mod telegram_service;
mod wirlpool_services;
mod exchange;
mod orchestrator;

use anyhow::Result;
use dotenv::dotenv;
use std::sync::Arc;
use std::env;
use tokio::sync::mpsc::UnboundedSender;

use std::time::Duration;
use tokio::time::sleep;
use env_logger::Env;
use params::{POOL, TOTAL_USDC_SOLUSDC, USDC_SOLRAY, PCT_LIST_1, PCT_LIST_2, USDC_SOLwhETH, WSOL, RAY, USDC, WETH};
use types::{PoolConfig, PoolRuntimeInfo};
use tokio::{sync::Mutex};

use crate::telegram_service::tl_engine::ServiceCommand;


// #[tokio::main]
// async fn main() -> Result<()> {
//     dotenv().ok();
//     // env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
//     // .init();

//     // 1. start Telegram service
//     let (tx_telegram, _commander) = telegram_service::tl_engine::start();

//     // 2. start Hyperliquid hedge worker
//     // let tx_hl = exchange::hl_engine::start()?;
//     let mut skip_open_first = false;

//     // 3. orchestrator: infinite cycle
//     loop {
//         if let Err(e) = orchestrator::orchestrator_cycle(&tx_telegram, &mut skip_open_first).await {
//             eprintln!("[ERROR] orchestrator cycle failed: {:?}", e);
//             let _ = tx_telegram.send(ServiceCommand::SendMessage(
//                 format!("❌ Bot error: {:?}", e)
//             ));
//         }
//         // small pause before restarting cycle
//         sleep(Duration::from_secs(10)).await;
//     }
// }

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // ─── Telegram
    let (tx_tg, _commander) = telegram_service::tl_engine::start();



    // ─── A)  SOL/USDC пул  – 3 диапазона
    let solusdc_cfg = PoolConfig {
        program: "whirlpool",
        name:    "SOL/USDC",
        pool_address: Box::leak(env::var("SOLUSDC_POOL")?.into_boxed_str()),
        mint_a:  WSOL,  decimal_a: 9,
        mint_b:  USDC,   decimal_b: 6,
        amount:  0.0,   sol_init: 0.0,
        position_1: None, position_2: None, position_3: None,
    };

    // ─── B)  RAY/SOL пул  – 1 диапазон
    let raysol_cfg = PoolConfig {
        program: "whirlpool",
        name:    "RAY/SOL",
        pool_address: Box::leak(env::var("RAYSOL_POOL")?.into_boxed_str()),
        mint_a:  WSOL,  decimal_a: 9,
        mint_b:  RAY,   decimal_b: 6,
        amount:  0.0,   sol_init: 0.0,
        position_1: None, position_2: None, position_3: None,
    };

    // ─── B)  whETH/SOL пул  – 1 диапазон
    let ethsol_cfg = PoolConfig {
        program: "whirlpool",
        name:    "WETH/SOL",
        pool_address: Box::leak(env::var("ETHSOL_POOL")?.into_boxed_str()),
        mint_a:  WSOL,  decimal_a: 9,
        mint_b:  WETH,   decimal_b: 8,
        amount:  0.0,   sol_init: 0.0,
        position_1: None, position_2: None, position_3: None,
    };

    let report_cfgs = vec![ solusdc_cfg.clone(), raysol_cfg.clone(), ethsol_cfg.clone()];

    // tokio::spawn(run_pool_with_restart(
    //     solusdc_cfg.clone(), TOTAL_USDC_SOLUSDC, PCT_LIST_1, true, 
    //     tx_tg.clone()
    // ));

    // tokio::spawn(run_pool_with_restart(
    //     raysol_cfg.clone(), USDC_SOLRAY, [0.0;4], false,
    //     tx_tg.clone()
    // ));

    tokio::spawn(run_pool_with_restart(
        ethsol_cfg.clone(), USDC_SOLwhETH, [0.0;4], false,
        tx_tg.clone()
    ));

    // ─── C)  единый репортёр каждые 5 минут ────────────────────────────
    let tx_tel = tx_tg.clone();
    tokio::spawn(async move {
        use tokio::time;
        let mut itv = time::interval(Duration::from_secs(300));

        loop {
            itv.tick().await;
            let mut msg = String::new();

            for cfg in &report_cfgs {
                match orchestrator::build_pool_report(cfg).await {
                    Ok(part) => msg.push_str(&part),
                    Err(e)   => msg.push_str(&format!("⚠️ report for {} failed: {e}\n", cfg.name)),
                }
            }

            let _ = tx_tel.send(ServiceCommand::SendMessage(msg));
        }
    });

    // ─── держим runtime живым
    futures::future::pending::<()>().await;

    Ok(())
}

async fn run_pool_with_restart(
    cfg:        PoolConfig,
    capital:    f64,
    pct:        [f64; 4],
    three_rng:  bool,
    tx_tg:      UnboundedSender<ServiceCommand>,
) {
    use tokio::time::{sleep, Duration};

    loop {
        let res = orchestrator::orchestrator_pool(
            cfg.clone(), capital, pct, three_rng, tx_tg.clone()
        ).await;

        match res {
            // нормальный выход (позиции закрыли / цена вышла …) —
            // через 10 с попробуем открыть заново
            Ok(()) => sleep(Duration::from_secs(10)).await,

            Err(e) => {
                let txt = format!("{e:#}");
                // «нехватка средств» (выбрасывается внутри open_with_funds…)
                if txt.contains("всё ещё не хватает") ||
                   txt.contains("Не хватает ни B, ни USDC") {
                    let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                        "⛔ {} остановлен: недостаток средств.\n{txt}",
                        cfg.name
                    )));
                    break;                      // больше **не** рестартим
                }

                // любая другая ошибка → сообщаем и пробуем снова
                let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                    "⚠️ {} упал: {txt}\nПерезапуск через 15 с…",
                    cfg.name
                )));
                sleep(Duration::from_secs(15)).await;
            }
        }
    }
}