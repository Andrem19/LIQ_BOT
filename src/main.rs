mod params;
mod types;
mod orca_logic;
mod telegram_service;
mod wirlpool_services;
mod exchange;
mod orchestrator;
pub mod utils;

use anyhow::Result;
use dotenv::dotenv;
use std::sync::Arc;
use std::env;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

use std::time::Duration;
use tokio::time::sleep;
use std::sync::atomic::AtomicBool;
use params::{TOTAL_USDC_SOLUSDC, USDC_SOLRAY, PCT_LIST_1, USDC_SOLWETH, WSOL, RAY, USDC, WETH};
use types::PoolConfig;


use crate::telegram_service::tl_engine::ServiceCommand;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let close_notify = Arc::new(tokio::sync::Notify::new());

    // ─── Telegram
    let (tx_tg, _commander) = telegram_service::tl_engine::start(close_notify.clone());

    let need_to_open_new_positions = Arc::new(AtomicBool::new(true)); //true - будут открываться новые при запуске; false - не будут

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

    tokio::spawn(run_pool_with_restart(
        solusdc_cfg.clone(), TOTAL_USDC_SOLUSDC, PCT_LIST_1, true, 
        tx_tg.clone(), need_to_open_new_positions.clone(), close_notify.clone()
    ));
    sleep(Duration::from_secs(20)).await;

    tokio::spawn(run_pool_with_restart(
        raysol_cfg.clone(), USDC_SOLRAY, [0.0;4], false,
        tx_tg.clone(), need_to_open_new_positions.clone(), close_notify.clone()
    ));
    sleep(Duration::from_secs(10)).await;

    tokio::spawn(run_pool_with_restart(
        ethsol_cfg.clone(), USDC_SOLWETH, [0.0;4], false,
        tx_tg.clone(), need_to_open_new_positions.clone(), close_notify.clone()
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
    need_new: Arc<AtomicBool>,
    close_ntf:  Arc<Notify>,
) {
    use tokio::time::{sleep, Duration};

    loop {
        let res = orchestrator::orchestrator_pool(
            cfg.clone(), capital, pct, three_rng, tx_tg.clone(), need_new.clone(), close_ntf.clone()
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
                if txt.contains("dns error") || txt.contains("timed out") {
                    let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                        "🌐 {}: RPC недоступен. Повтор через 15 с…", cfg.name
                    )));
                    sleep(Duration::from_secs(15)).await;
                    continue;              // НЕ закрываем позиции
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