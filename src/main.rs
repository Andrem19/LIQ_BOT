mod params;
mod types;
mod orca_logic;
mod telegram_service;
mod wirlpool_services;
mod exchange;
mod orchestrator;
pub mod utils;
pub mod database;
pub mod pyth_ws;

use anyhow::Result;
use dotenv::dotenv;
use std::sync::Arc;
use anyhow::anyhow;
use std::env;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use std::sync::atomic::Ordering;
use chrono::Utc;


use std::time::Duration;
use tokio::time::sleep;
use std::sync::atomic::AtomicBool;
use params::{AMOUNT, PCT_LIST_1, PCT_LIST_2, WSOL, USDC, POOL_NUMBER};
use types::PoolConfig;
use database::triggers;
use database::triggers::Trigger;
use database::positions;
use database::history;
use database::general_settings;

use crate::{telegram_service::tl_engine::ServiceCommand};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();


    triggers::init().await?;
    positions::init_positions_module().await?;
    history::init_history_module().await?;
    general_settings::init_general_settings_module().await?;
    general_settings::init_settings_from_params().await?;

    let close_notify = Arc::new(tokio::sync::Notify::new());

    // ─── Telegram
    let (tx_tg, _commander) = telegram_service::tl_engine::start(close_notify.clone());

    let need_to_open_new_positions_SOLUSDC = Arc::new(AtomicBool::new(false)); //true - будут открываться новые при запуске; false - не будут

    let flag = need_to_open_new_positions_SOLUSDC.load(Ordering::SeqCst);
    let tr = Trigger {
        name:     "position_start_open".into(),
        state:    !flag,
        position: String::new(),
    };
    triggers::upsert_trigger(&tr).await?;

    let new_positions = Arc::new(AtomicBool::new(true));
    // ─── A)  SOL/USDC пул  – 3 диапазона
    let solusdc_cfg = PoolConfig {
        program: "whirlpool".to_string(),
        name:    "SOL/USDC".to_string(),
        pool_address: env::var("SOLUSDC_POOL")?,
        mint_a:  WSOL.to_string(),  decimal_a: 9,
        mint_b:  USDC.to_string(),   decimal_b: 6,
        amount:  0.0,
        position_1: None, position_2: None, position_3: None,

        date_opened:           Utc::now(),     // или любая другая заглушечная дата
        is_closed:             false,          // позиция ещё не закрыта
        commission_collected_1: 0.0,            // пока комиссий нет
        commission_collected_2: 0.0,
        commission_collected_3: 0.0,
        total_value_open:      0.0,            // либо исходная TVL
        total_value_current:   0.0,            // либо текущее TVL
    };

    let report_cfgs = vec![solusdc_cfg.clone()];

    tokio::spawn(run_pool_with_restart(
        solusdc_cfg.clone(), AMOUNT, PCT_LIST_1, true, 
        tx_tg.clone(), need_to_open_new_positions_SOLUSDC.clone(), close_notify.clone(), 5, Some(0.03), new_positions.clone()
    ));
    sleep(Duration::from_secs(50)).await;

    // ─── C)  единый репортёр каждые 5 минут ────────────────────────────
    let tx_tel = tx_tg.clone();
    tokio::spawn({
        let report_cfgs = report_cfgs.clone();   // если нужно
        let tx_tel      = tx_tel.clone();
        async move {
            use tokio::time::{interval, Duration};
            use std::collections::VecDeque;
    
            let mut itv          = interval(Duration::from_secs(300));
            let mut prev_total   = 0.0;
            let mut last_profits = VecDeque::<f64>::with_capacity(12); // 12×5 мин = час
            let mut first_loop = true;
            let mut counter = 0;

            loop {
                match triggers::get_trigger("position_start_open").await {
                    Ok(Some(tr)) if tr.state => break, // выходим, если триггер включён
                    Ok(_) => {
                        // либо нет записи, либо state=false
                        sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        // не удалось прочитать из БД — логируем, ждём и пробуем снова
                        log::error!("Error reading trigger: {}", e);
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
    
            loop {
                itv.tick().await;
                counter+=1;

                // 1) собираем отчёты
                let mut grand_total = 0.0;
                let mut msg = String::new();

                let is_new_positions = new_positions.load(Ordering::SeqCst);
    
                for cfg in &report_cfgs {
                    match orchestrator::build_pool_report(cfg).await {
                        Ok(rep) => {
                            grand_total += rep.total;
                            msg.push_str(&rep.text);
                        }
                        Err(e) => msg.push_str(
                            &format!("⚠️ report for {} failed: {e}\n", cfg.name)),
                    }
                }
    
                // 2) считаем profit и поддерживаем «кольцевой буфер»
                let mut profit = 0.0;

                
                if is_new_positions {
                    first_loop = true;
                    last_profits.clear();
                }

                new_positions.store(false, Ordering::SeqCst);

                if first_loop {
                    counter = 0;
                    prev_total = grand_total;
                    first_loop = false;
                } else {
                    profit = grand_total - prev_total;
                    prev_total = grand_total;
        
                    if last_profits.len() == 12 { last_profits.pop_front(); }
                    last_profits.push_back(profit);
                }

    
                let sum_last: f64 = last_profits.iter().sum();
                let avg_last: f64 = if !last_profits.is_empty() {
                    sum_last / last_profits.len() as f64
                } else { 0.0 };

                let duration = counter * 5;
    
                // 3) итоговый блок и отправка
                msg.push_str(&format!(
                    "📈 Total: ${:.6}\nΔ5 min: {:+.4}\nΣ1 h (12×5 min): {:+.4} (avg {:+.4}/5 min) dur: {:}",
                    grand_total, profit, sum_last, avg_last, duration
                ));
                let _ = tx_tel.send(ServiceCommand::SendMessage(msg));
            }
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
    min_restart: u64,
    range: Option<f32>,
    new_positions: Arc<AtomicBool>,
) -> Result<()> {
    use tokio::time::{sleep, Duration};
    
    loop {
        if let Some(t) = triggers::get_trigger("auto_trade").await? {
            println!("Got: {:?}", t);
            if t.state == true {
                sleep(Duration::from_secs(15)).await;
                continue;
            }
        }

        new_positions.store(true, Ordering::SeqCst);
        let res = orchestrator::orchestrator_pool(
            cfg.clone(), capital, pct, three_rng, tx_tg.clone(), need_new.clone(), close_ntf.clone(), min_restart, range
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
                    break;                    // больше **не** рестартим
                }
                if txt.contains("dns error") || txt.contains("timed out") {
                    let _ = tx_tg.send(ServiceCommand::SendSignal(format!(
                        "🌐 {}: RPC недоступен. Повтор через 15 с…", cfg.name
                    )));
                    sleep(Duration::from_secs(15)).await;
                    continue;              // НЕ закрываем позиции
                }

                // любая другая ошибка → сообщаем и пробуем снова
                let _ = tx_tg.send(ServiceCommand::SendSignal(format!(
                    "⚠️ {} упал: {txt}\nПерезапуск через 15 с…",
                    cfg.name
                )));
                sleep(Duration::from_secs(15)).await;
            }
        }
    }
    return Ok(());
}