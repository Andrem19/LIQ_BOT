
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::{read_keypair_file, Keypair}, signer::Signer};
use spl_associated_token_account::get_associated_token_address;
use std::{str::FromStr, time::Duration};
use anyhow::Result;
use tokio::time::sleep;
use anyhow::anyhow;
use orca_whirlpools::PositionOrBundle;
use orca_whirlpools_client::Whirlpool;
use crate::types::PoolConfig;
use std::time::Instant;

use crate::params::{RPC_URL, POOL, PCT_LIST_1, PCT_LIST_2, ATR_BORDER, WEIGHTS, TOTAL_USDC, KEYPAIR_FILENAME};
use crate::types::{LiqPosition, Role};
use crate::orca_logic::helpers::{calc_bound_prices_struct, calc_range_allocation_struct};
use crate::wirlpool_services::wirlpool::{open_with_funds_check, close_all_positions, list_positions_for_owner};
use crate::wirlpool_services::get_info::fetch_pool_position_info;
use crate::telegram_service::tl_engine::ServiceCommand;
use crate::types::WorkerCommand;
use tokio::sync::mpsc::UnboundedSender;
use spl_token::state::Mint;
use std::env;
use spl_token::solana_program::program_pack::Pack;
use crate::exchange::helpers::get_atr_1h;


pub async fn orchestrator_cycle(
    tx_telegram: &UnboundedSender<ServiceCommand>
) -> Result<()> {
    let pool_address = env::var("POOL_ADDRESS")
        .map_err(|e| anyhow!("Не удалось прочитать POOL_ADDRESS из .env: {}", e))?;
    let pool_address_static: &'static str = Box::leak(pool_address.into_boxed_str());

    let atr_1h = get_atr_1h("SOLUSDT", 400, 12, 14).await?;
    let avg_atr_last3: f64 = atr_1h.iter().rev().take(3).copied().sum::<f64>() / 3.0;
    println!("Average ATR over last 3 1h candles: {:.6}", avg_atr_last3);
    let pct_list = if avg_atr_last3 > ATR_BORDER {
        PCT_LIST_1
    } else {
        PCT_LIST_2
    };

    // Клонируем вашу константу (PoolConfig), чтобы не портить глобальную
    let mut pool_cfg = POOL.clone();
    // и переопределяем поле
    pool_cfg.pool_address = pool_address_static;
    // --- 1. Предварительная проверка: нет ли старых позиций? ---
    let existing = list_positions_for_owner().await?;
    if !existing.is_empty() {
        let _ = tx_telegram.send(ServiceCommand::SendMessage(
            "⚠️ Перед открытием новых позиций нужно закрыть старые!".into()
        ));
        // tx_hl.send(WorkerCommand::Off)?;
        close_all_positions(300).await?;
        return Err(anyhow!("Есть непустые позиции, прерываю цикл"));
    }

    // --- 2. Fetch on‐chain price + расчёт диапазонов и аллокаций ---
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let whirl_pk = Pubkey::from_str(&pool_cfg.pool_address)?;
    let acct = rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&acct.data)?;
    let da = Mint::unpack(&rpc.get_account(&whirl.token_mint_a).await?.data)?.decimals;
    let db = Mint::unpack(&rpc.get_account(&whirl.token_mint_b).await?.data)?.decimals;
    let price = orca_whirlpools_core::sqrt_price_to_price(whirl.sqrt_price.into(), da, db);

    let bounds = calc_bound_prices_struct(price, &pct_list);
    let allocs = calc_range_allocation_struct(price, &bounds, &WEIGHTS, TOTAL_USDC);

    let _ = tx_telegram.send(ServiceCommand::SendMessage(
        format!("🔔 Открытие 3 диапазонных позиций по цене {:.6} На сумму: {:.2} ATR: {:.2}", price, TOTAL_USDC, avg_atr_last3),
    ));

    // --- 3. Две попытки открытия с fallback-логикой ---
    let mut sol_total: f64 = 0.0;
    // будем хранить только (index, position_mint)
    let mut minted: Vec<(usize, Pubkey)> = Vec::with_capacity(3);

    let mut slippage: u16 = 100;

    for attempt in 1..=2 {
        for (idx, alloc) in allocs.iter().enumerate() {
            // если эта позиция ещё не открыта — пробуем
            if minted.iter().all(|&(i, _)| i != idx) {
                let deposit = if idx == 1 {
                    alloc.usdc_amount
                } else {
                    alloc.usdc_equivalent
                };
                
                match open_with_funds_check(
                    alloc.lower_price,
                    alloc.upper_price,
                    deposit,
                    pool_cfg.clone(),
                    slippage
                ).await {
                    Ok(res) => {
                        sol_total += res.amount_wsol;
                        minted.push((idx, res.position_mint));
                        let _ = tx_telegram.send(ServiceCommand::SendMessage(
                            format!(
                                "✅ [{}] открыт: mint {} (WSOL {:.6} / USDC {:.6})",
                                alloc.range_type, res.position_mint, res.amount_wsol, res.amount_usdc
                            ),
                        ));
                    }
                    Err(e) => {
                        let _ = tx_telegram.send(ServiceCommand::SendMessage(
                            format!("❌ [{}] не открылся: {:?}", alloc.range_type, e)
                        ));
                    }
                }

                sleep(Duration::from_secs(1)).await;
            }
        }

        // даём блокчейну обновиться
        sleep(Duration::from_secs(2)).await;

        // Проверяем, сколько из трёх реально появились on‐chain
        let positions = list_positions_for_owner().await?;
        let got = positions.iter().filter_map(|pb| {
            if let PositionOrBundle::Position(hp) = pb {
                minted.iter()
                    .find(|&&(_, m)| m == hp.data.position_mint)
                    .map(|&(i, _)| i)
            } else { None }
        }).collect::<Vec<usize>>();

        if got.len() == 3 {
            break; // все три успешно открыты
        }

        if attempt == 1 {
            let _ = tx_telegram.send(ServiceCommand::SendMessage(
                format!("⚠️ После первой попытки открыто {}/3, пробуем недостающие…", got.len())
            ));
            slippage = 250;
            sleep(Duration::from_secs(1)).await;
        } else {
            close_all_positions(150).await?;
            let _ = tx_telegram.send(ServiceCommand::SendMessage(
                format!("❌ Не удалось открыть все 3 позиции (открыто {}). Была попытка их закрыть. Перерыв.", got.len())
            ));
            std::process::exit(1);
        }
    }

    // --- 4. Формируем PoolConfig и запускаем хедж ---
    let start_time = Instant::now();
    let mut cfg: PoolConfig = pool_cfg.clone();
    cfg.sol_init = sol_total;

    let positions = list_positions_for_owner().await?;
    for &(idx, mint) in &minted {
        if let Some(hp) = positions.iter().find_map(|pb| {
            if let PositionOrBundle::Position(hp) = pb {
                if hp.data.position_mint == mint { Some(hp) } else { None }
            } else { None }
        }) {
            let addr: &'static str = Box::leak(hp.address.to_string().into_boxed_str());
            let nft:  &'static str = Box::leak(hp.data.position_mint.to_string().into_boxed_str());
            let liq = LiqPosition {
                role: match idx {
                    0 => Role::Up,
                    1 => Role::Middle,
                    2 => Role::Down,
                    _ => unreachable!(),
                },
                position_address: Some(addr),
                position_nft:    Some(nft),
                upper_price:     allocs[idx].upper_price,
                lower_price:     allocs[idx].lower_price,
            };
            match idx {
                0 => cfg.position_1 = Some(liq),
                1 => cfg.position_2 = Some(liq),
                2 => cfg.position_3 = Some(liq),
                _ => {}
            }
        }
    }

    // tx_hl.send(WorkerCommand::On(cfg.clone()))?;

    // --- 5. Мониторинг: отчёт каждые 5 мин, проверка цены каждые 30 сек ---
    let upper_exit = cfg.position_1.as_ref().unwrap().upper_price;
    let lower_exit = cfg.position_3.as_ref().unwrap().lower_price;
    let mut report_interval = tokio::time::interval(Duration::from_secs(300));
    let mut price_interval  = tokio::time::interval(Duration::from_secs(3));

    loop {
        tokio::select! {
            // a) report
            _ = report_interval.tick() => {
                // 1) Сначала получаем актуальную on‐chain цену
                let acct = rpc.get_account(&whirl_pk).await?;
                let sqrt_p = orca_whirlpools_client::Whirlpool::from_bytes(&acct.data)?.sqrt_price.into();
                let current_price = orca_whirlpools_core::sqrt_price_to_price(sqrt_p, da, db);
            
                // 2) Собираем отчёт по каждой позиции, аккумулируем общую сумму
                let mut report = format!("📊 Pool report — Price: {:.6}\n", current_price);
                let mut total_value = 0.0;
                for (i, opt) in [
                    cfg.position_1.as_ref(),
                    cfg.position_2.as_ref(),
                    cfg.position_3.as_ref(),
                ].iter().enumerate() {
                    if let Some(lp) = opt {
                        if let Ok(info) = fetch_pool_position_info(&cfg, lp.position_address).await {
                            report.push_str(&format!(
                                "Pos {}: Range [{:.6}–{:.6}], Total ${:.6}\n",
                                i + 1,
                                info.lower_price,
                                info.upper_price,
                                info.sum,
                            ));
                            total_value += info.sum;
                        }
                    }
                }
            
                // 3) Добавляем общий итог
                report.push_str(&format!("— Всего по всем позициям: ${:.6}\n", total_value));
            
                let _ = tx_telegram.send(ServiceCommand::SendMessage(report));
            }

            // b) exit check
            _ = price_interval.tick() => {
                let acct = rpc.get_account(&whirl_pk).await?;
                let sqrt_p = Whirlpool::from_bytes(&acct.data)?.sqrt_price.into();
                let current = orca_whirlpools_core::sqrt_price_to_price(sqrt_p, da, db);
            
                if current >= upper_exit || current <= lower_exit {
                    // вычисляем, сколько времени прошло с открытия
                    let elapsed = start_time.elapsed();
                    let secs = elapsed.as_secs();
                    let hours = secs / 3600;
                    let mins  = (secs % 3600) / 60;
                    let secs  = secs % 60;
            
                    // шлём предупреждение с таймингом
                    let _ = tx_telegram.send(ServiceCommand::SendMessage(
                        format!(
                            "⚠️ Price {:.6} вышла за [{:.6}–{:.6}], закрываем…\n⏱️ Elapsed since open: {}h {}m {}s",
                            current, lower_exit, upper_exit,
                            hours, mins, secs,
                        )
                    ));
            
                    close_all_positions(150).await?;
                    // tx_hl.send(WorkerCommand::Off)?;
            
                    // балансы после
                    let keypair = read_keypair_file(KEYPAIR_FILENAME)
                        .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
                    let wallet = keypair.pubkey();
                    let lamports_total = rpc.get_balance(&wallet).await
                        .map_err(|e| anyhow!("get_balance SOL failed: {}", e))?;
                    let bal_sol = lamports_total as f64 / 1e9;
                    let ata_usdc = get_associated_token_address(&wallet, &Pubkey::from_str(cfg.mint_b)?);
                    let bal_usdc = rpc.get_token_account_balance(&ata_usdc).await?.amount.parse::<f64>()? / 1e6;

                    // on‐chain price again to convert SOL → USDC
                    let acct       = rpc.get_account(&whirl_pk).await?;
                    let whirl      = orca_whirlpools_client::Whirlpool::from_bytes(&acct.data)?;
                    let mint_a_acct = rpc.get_account(&whirl.token_mint_a).await?;
                    let mint_b_acct = rpc.get_account(&whirl.token_mint_b).await?;
                    let da         = Mint::unpack(&mint_a_acct.data)?.decimals;
                    let db         = Mint::unpack(&mint_b_acct.data)?.decimals;
                    let price_now  = orca_whirlpools_core::sqrt_price_to_price(
                        whirl.sqrt_price.into(), da, db
                    );

                    // общий баланс в USDC
                    let total_usdc = bal_usdc + bal_sol * price_now;

                    // финальный отчёт
                    let _ = tx_telegram.send(ServiceCommand::SendMessage(
                        format!(
                            "🏦 After close — WSOL: {:.6}, USDC: {:.6}, Total in USDC: {:.6}",
                            bal_sol, bal_usdc, total_usdc
                        )
                    ));
            
                    break;
                }
            }
        }
    }

    Ok(())
}