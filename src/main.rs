// ─── Module declarations ───────────────────────────────────────────────────
mod params;
mod types;
mod telegram_service;
mod dex_services;
mod exchange;
mod orchestrator;


pub mod utils;
pub mod database;
pub mod pyth_ws;
pub mod strategies;
pub mod comp_strategy;

// ─── External and standard imports ─────────────────────────────────────────
use std::{
    env,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use sqlx::Error;
use tokio::time::Instant;
use anyhow::{Context, Result};
use chrono::Utc;
use dotenv::dotenv;
use tokio::{
    sync::{mpsc::UnboundedSender, Notify, RwLock},
    time::{sleep, Duration},
};

// ─── Local crate imports ────────────────────────────────────────────────────
use crate::{
    database::{
        general_settings, history, positions, triggers::{self, Trigger}
    }, params::{RANGE, USDC, WSOL}, strategies::limit_order::is_limit_trigger_satisfied, telegram_service::tl_engine::ServiceCommand, types::{PoolConfig, Range}
};
use crate::exchange::helpers::Candle;
use crate::exchange::helpers::decide;
use crate::exchange::helpers::{get_atr, range_coefficient, calculate_price_bounds, Mode};
use crate::exchange::helpers::get_rsi;
use crate::comp_strategy::stream_candles;
use crate::exchange::helpers::convert_timeframe;
use crate::exchange::helpers::Unzip5;

const POST_CLOSE_RESTART_DELAY:  u64 = 10;
const RPC_RETRY_DELAY:           u64 = 20;
const LOOKBACK_1M: usize = 30;    // сколько 1-минуток держим в расчёте
const ATR_PER: usize     = 14;
const RSI_PER: usize     = 14;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    init_database().await?;

    let close_notify = Arc::new(tokio::sync::Notify::new());

    let (tx_tg, _commander) = telegram_service::tl_engine::start(close_notify.clone());

    let need_new_pos = Arc::new(AtomicBool::new(false)); //true - будут открываться новые при запуске; false - не будут
    let auto_trade = true;

    init_default_triggers(&need_new_pos, auto_trade).await?;
    let init_wallet_balance = utils::fetch_wallet_balance_info().await?;
    println!("Wallet Balance: {}", init_wallet_balance);
    let _ = tx_tg.send(ServiceCommand::SendMessage(init_wallet_balance.to_string()));

    let solusdc_cfg = make_solusdc_config()?;
    let report_cfgs = vec![solusdc_cfg.clone()];

    tokio::spawn(run_pool_with_restart(
        solusdc_cfg.clone(), 
        tx_tg.clone(), need_new_pos.clone(), close_notify.clone(), 1, Some(0.03)
    ));

    // ─── C)  единый репортёр каждые 5 минут ────────────────────────────
    let tx_tel = tx_tg.clone();
    tokio::spawn({
        let report_cfgs = report_cfgs.clone();   // если нужно
        let tx_tel      = tx_tel.clone();
        async move {
            use tokio::time::{Duration};
            use std::collections::VecDeque;
    
            let mut prev_total   = 0.0;
            let mut last_profits = VecDeque::<f64>::with_capacity(12); // 12×5 мин = час
            let mut first_loop = true;
            let mut counter = 0;

            loop {

                let auto_trade = triggers::get_trigger("auto_trade").await;
                let pool_report_run = triggers::get_trigger("pool_report_run").await;
                if pool_report_run.state && !auto_trade.state {
                    break;
                } else {
                    sleep(Duration::from_secs(1)).await;
                }
            }
    
            loop {

                let report_info_reset = triggers::get_trigger("report_info_reset").await;
                let opening = triggers::get_trigger("opening").await;
                if opening.state {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                if !report_info_reset.state  {
                    let mins = match general_settings::get_general_settings().await {
                        Ok(Some(cfg)) => cfg.info_interval,
                        _             => 5,
                    };
                    sleep(Duration::from_secs((mins as u64) * 60)).await;
                } 

                counter+=1;

                // 1) собираем отчёты
                let mut grand_total = 0.0;
                let mut msg = String::new();

                let trig = triggers::get_trigger("pool_report_run").await;

                if !trig.state {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
    
                for cfg in &report_cfgs {
                    match orchestrator::build_pool_report(cfg, tx_tg.clone(), init_wallet_balance.total_usd).await {
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

                
                if report_info_reset.state {
                    first_loop = true;
                    last_profits.clear();
                    let _ = triggers::report_info_reset(false, None).await;
                }

                

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
    tx_tg:      UnboundedSender<ServiceCommand>,
    need_new: Arc<AtomicBool>,
    close_ntf:  Arc<Notify>,
    min_restart: u64,
    range: Option<f32>,
) -> Result<()> {
    use tokio::time::{sleep, Duration};

    let mut candles_arc: Option<Arc<RwLock<Vec<Candle>>>> = None;
    let report_interval = Duration::from_secs(300);
    let mut last_report = Instant::now() - report_interval;
    
    loop {
        let auto_trade = triggers::get_trigger("auto_trade").await;

        println!("Got: {:?}", auto_trade);
        let limit = is_limit_trigger_satisfied(&tx_tg).await?;
        if limit && auto_trade.state {
            need_new.store(true, Ordering::SeqCst);
            triggers::auto_trade_switch(false, None).await?;
            triggers::limit_switcher(false, Some(&tx_tg)).await?;
        } else if auto_trade.state == true {
            if candles_arc.is_none() {
                candles_arc = Some(stream_candles("SOLUSDT", 1, 300).await?); // ★
            }

            // читаем последние LOOKBACK_1M баров
            let ready = {
                let src = {
                    let guard = candles_arc.as_ref().unwrap().read().await;
                    guard.iter()
                            .rev()
                            .take(LOOKBACK_1M)
                            .cloned()
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                };
                if src.len() == LOOKBACK_1M {
                    let (o,h,l,c,v) = src.iter()
                        .map(|c| (c.open,c.high,c.low,c.close,c.volume))
                        .unzip5();
                    let (o5,h5,l5,c5,v5) = convert_timeframe(&o,&h,&l,&c,&v,5,0);
                    let atr = get_atr(&o5,&h5,&l5,&c5,&v5,ATR_PER)?;
                    let atr_last = atr[atr.len()-1];
                    let (pr_up, pr_low) = calculate_price_bounds(c[c.len()-1]);
                    println!("pr_up: {:.2} pr_low: {:.2}", pr_up, pr_low);
                    let centre_kof = range_coefficient(&o,&h,&l,&c, 70, pr_low, pr_up, Mode::Full).unwrap();
                    println!("Ждем вход в позицию ATR: {} CNT: {}", &atr_last, &centre_kof);

                    if last_report.elapsed() >= report_interval {
                        let msg = format!(
                            "📊 ATR: {:.6}\n📈 CNT: {:.6}",
                            atr_last,
                            centre_kof
                        );
                        let _ = tx_tg.send(ServiceCommand::SendMessage(msg));
                        last_report = Instant::now();
                    }

                    centre_kof > 0.99 && atr_last < 0.40            // ← true / false
                } else { 
                    false 
                }
            };

            if !ready {
                // ждём и начинаем итерацию заново
                sleep(Duration::from_secs(RPC_RETRY_DELAY)).await;
                continue;
            } else {
                need_new.store(true, Ordering::SeqCst);
                // «решение принято» — стрим больше не нужен
                triggers::auto_trade_switch(false, None).await?;
                candles_arc = None;             // ★ drop Arc => ws завершается
            }
        }
        triggers::report_info_reset(true, Some(&tx_tg.clone())).await?;

        let settings = general_settings::get_general_settings().await?
        .context("General settings not found in database")?;

        let pct: [f64; 4]   = if settings.pct_number == 1 {settings.pct_list_1} else { settings.pct_list_2 };


        triggers::opening_switcher(true, Some(&tx_tg)).await?;

        let weights = if RANGE == Range::Two {params::weights_2()} else if RANGE == Range::Three {params::weights_1()} else {params::weights_1()};

        let res = orchestrator::orchestrator_pool(
            cfg.clone(), settings.amount, pct, weights, tx_tg.clone(), need_new.clone(), close_ntf.clone(), min_restart, range, settings.compress
        ).await;

        match res {
            Ok(()) => sleep(Duration::from_secs(POST_CLOSE_RESTART_DELAY)).await,

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
                    sleep(Duration::from_secs(RPC_RETRY_DELAY)).await;
                    continue;              // НЕ закрываем позиции
                }

                // любая другая ошибка → сообщаем и пробуем снова
                let _ = tx_tg.send(ServiceCommand::SendSignal(format!(
                    "⚠️ {} упал: {txt}\nПерезапуск через 15 с…",
                    cfg.name
                )));
                sleep(Duration::from_secs(RPC_RETRY_DELAY)).await;
            }
        }
    }
    return Ok(());
}

fn make_solusdc_config() -> Result<PoolConfig> {
    Ok(PoolConfig {
        program: "whirlpool".to_string(),
        name:    "SOL/USDC".to_string(),
        pool_address: env::var("SOLUSDC_POOL")?,
        mint_a:  WSOL.to_string(),  decimal_a: 9,
        mint_b:  USDC.to_string(),  decimal_b: 6,
        amount:  0.0,
        position_1: None, position_2: None, position_3: None,
        date_opened:         Utc::now(),
        is_closed:           false,
        commission_collected_1: 0.0,
        commission_collected_2: 0.0,
        commission_collected_3: 0.0,
        total_value_open:    0.0,
        total_value_current: 0.0,
        wallet_balance: 0.0
    })
}

/// Создаёт или обновляет набор «базовых» триггеров.
async fn init_default_triggers(need_new_pos: &Arc<AtomicBool>, auto_trade: bool) -> Result<()> {
    let new_pos_flag = !need_new_pos.load(Ordering::SeqCst);

    // helper-закрывалка
    async fn upsert(name: &str, state: bool) -> Result<()> {
        let tg = Trigger { name: name.into(), state, position: String::new() };
        triggers::upsert_trigger(&tg).await?;
        Ok(())
    }

    upsert("pool_report_run", new_pos_flag).await?;
    upsert("closing",              false        ).await?;
    upsert("limit",                false        ).await?;
    upsert("report_info_reset",         true         ).await?;
    upsert("auto_trade",         auto_trade         ).await?;
    upsert("opening",         false         ).await?;

    Ok(())
}

/// Инициализируем все таблицы/коллекции БД.
async fn init_database() -> Result<()> {
    triggers::init().await?;
    positions::init_positions_module().await?;
    history::init_history_module().await?;
    general_settings::init_general_settings_module().await?;
    general_settings::init_settings_from_params().await?;
    Ok(())
}
