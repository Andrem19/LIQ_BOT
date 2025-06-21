
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::{read_keypair_file, Keypair}, signer::Signer};
use spl_associated_token_account::get_associated_token_address;
use std::{str::FromStr, time::Duration};
use anyhow::Result;
use orca_whirlpools::PositionOrBundle;
use crate::types::PoolConfig;
use std::time::Instant;
use std::sync::atomic::AtomicBool;
use anyhow::bail;
use std::env;
use dotenv::dotenv;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::params::{WEIGHTS};
use crate::types::{LiqPosition, Role};
use crate::orca_logic::helpers::{calc_bound_prices_struct, calc_range_allocation_struct};
use crate::wirlpool_services::wirlpool::{open_with_funds_check_universal, close_all_positions, list_positions_for_owner};
use crate::wirlpool_services::get_info::fetch_pool_position_info;
use crate::telegram_service::tl_engine::ServiceCommand;
use crate::types::WorkerCommand;
use tokio::sync::mpsc::UnboundedSender;
use spl_token::state::Mint;
use tokio::sync::Notify;

use crate::params::{WALLET_MUTEX, USDC};
use crate::utils::utils;
use crate::utils::safe_get_account;
use crate::types::PoolRuntimeInfo;
use std::sync::Arc;

use spl_token::solana_program::program_pack::Pack;
use crate::exchange::helpers::get_atr_1h;
use crate::orca_logic::helpers::get_sol_price_usd;
use crate::wirlpool_services::wirlpool::close_whirlpool_position;



//-------------------------------- helper -----------------------------------
async fn close_and_report(
    rpc: &RpcClient,
    pool_cfg: &PoolConfig,
    whirl_pk: Pubkey,
    tx_tg: &UnboundedSender<ServiceCommand>,
) -> Result<()> {
    // 1) закрываем все позиции ЭТОГО пула
    close_all_positions(150, Some(whirl_pk)).await?;

    // 2) Балансы после закрытия
    let _lock   = WALLET_MUTEX.lock().await;              // единый замок
    let wallet  = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    let lamports = rpc.get_balance(&wallet_pk).await?;
    let bal_sol  = lamports as f64 / 1e9;

    let mint_b  = Pubkey::from_str(pool_cfg.mint_b)?;
    let ata_b   = get_associated_token_address(&wallet_pk, &mint_b);
    let bal_b   = rpc.get_token_account_balance(&ata_b)
        .await?.amount.parse::<f64>()?
        / 10f64.powi(pool_cfg.decimal_b as i32);

    // 3) Конвертируем SOL в эквивалент токена B или USDC
    let whirl_acct = safe_get_account(&rpc, &whirl_pk).await?;
    let whirl = orca_whirlpools_client::Whirlpool::from_bytes(&whirl_acct.data)?;
    let price_ab = orca_whirlpools_core::sqrt_price_to_price(
        whirl.sqrt_price.into(),
        pool_cfg.decimal_a as u8,
        pool_cfg.decimal_b as u8,
    );                       // price(B per A): сколько B за 1 SOL

    let total_usd = if pool_cfg.mint_b == USDC {
        bal_b + bal_sol * price_ab            // price_ab == USDC per SOL
    } else {
        // для RAY/SOL считаем «через SOL»: всё в экв. доллары
        // берём цену SOL-USD из биржи Jupiter (один HTTP-запрос)
        let sol_usd: f64 = get_sol_price_usd().await?;
        bal_b * price_ab * sol_usd  +  bal_sol * sol_usd
    };

    // 4) отчёт
    let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
        "🏦 {} закрыт.\n► SOL {:.6}\n► token B {:.6}\n► Всего ≈ ${:.2}",
        pool_cfg.name, bal_sol, bal_b, total_usd
    )));
    Ok(())
}


pub async fn orchestrator_pool(
    mut pool_cfg: PoolConfig,          // шаблон пула
    capital_usd: f64,                  // общий размер “кошелька” под пул
    pct_list: [f64; 4],                // как раньше (игнорируется для 1-диапаз.)
    three_ranges: bool,                // true = SOL/USDC, false = RAY/SOL
    tx_tg: UnboundedSender<ServiceCommand>,
    need_new: Arc<AtomicBool>,
    close_ntf:    Arc<Notify>,
) -> Result<()> {
    let need_open_new = need_new.load(Ordering::SeqCst);
    need_new.store(true, Ordering::SeqCst);
    
    if need_open_new {
        close_existing_owner_positions(&pool_cfg).await?;
    }
    // ───── 0. RPC / Whirlpool meta ───────────────────────────────────────
    let rpc       = utils::init_rpc();
    let whirl_pk  = Pubkey::from_str(pool_cfg.pool_address)?;
    let whirl_acct = safe_get_account(&rpc, &whirl_pk).await?;
    let whirl = orca_whirlpools_client::Whirlpool::from_bytes(&whirl_acct.data)?;
    let dec_a     = Mint::unpack(&rpc.get_account(&whirl.token_mint_a).await?.data)?.decimals;
    let dec_b     = Mint::unpack(&rpc.get_account(&whirl.token_mint_b).await?.data)?.decimals;


    // ───── 1. Текущая цена ───────────────────────────────────────────────
    let mut start_time = Instant::now();
    let price_raw = orca_whirlpools_core::sqrt_price_to_price(whirl.sqrt_price.into(), dec_a, dec_b);
    let invert    = !three_ranges;          // false для SOL/USDC, true для RAY/SOL
    let price_disp = if invert { 1.0 / price_raw } else { price_raw };
    let mut price = norm_price(price_raw, invert);

    // ───── 2. Формируем диапазоны / аллокации  ───────────────────────────
    let mut upper_exit = 0.0;
    let mut lower_exit = 0.0;

    if three_ranges && need_open_new {
        
        /* === трёх-диапазонное открытие для SOL/USDC ==================== */
        let bounds = calc_bound_prices_struct(price, &pct_list);
        let allocs = calc_range_allocation_struct(price, &bounds, &WEIGHTS, capital_usd);
        
        let _ = tx_tg.send(ServiceCommand::SendMessage(
            format!("🔔 Пытаюсь открыть 3 позиции SOL/USDC ({} USDC)…", capital_usd)
        ));
        
        let mut minted: Vec<(usize, String)> = Vec::new();   // (index, mint)
        let mut slippage = 150u16;
        
        'outer: for round in 1..=2 {                          // максимум 3 раунда
            let mut progress = false;
        
            for (idx, alloc) in allocs.iter().enumerate() {
                if minted.iter().any(|&(i, _)| i == idx) { continue } // уже есть
        
                let deposit = if idx == 1 { alloc.usdc_amount } else { alloc.usdc_equivalent };
        
                match open_with_funds_check_universal(
                    alloc.lower_price,
                    alloc.upper_price,
                    deposit,
                    pool_cfg.clone(),
                    slippage,
                ).await {
                    Ok(res) => {
                        // ↓ запоминаем открытую
                        minted.push((idx, res.position_mint.to_string()));
                        progress = true;
        
                        let liq = LiqPosition {
                            role: [Role::Up, Role::Middle, Role::Down][idx].clone(),
                            position_address: None,
                            position_nft:     None,
                            upper_price: alloc.upper_price,
                            lower_price: alloc.lower_price,
                        };
                        match idx {
                            0 => pool_cfg.position_1 = Some(liq),
                            1 => pool_cfg.position_2 = Some(liq),
                            _ => pool_cfg.position_3 = Some(liq),
                        }
        
                        let _ = tx_tg.send(ServiceCommand::SendMessage(
                            format!("✅ Открыта P{} (mint {})", idx + 1, res.position_mint),
                        ));
                    }
                    Err(e) => {
                        let _ = tx_tg.send(ServiceCommand::SendMessage(
                            format!("⚠️ P{} не открылась: {e}", idx + 1),
                        ));
                    }
                }
        
                // маленький «кул-даун», чтобы цепочка tx успела пройти
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
        
            // если уже все 3 — выходим
            if minted.len() == 3 { break 'outer; }
        
            // если не продвинулись — повышаем slippage
            if !progress { slippage += 100; }
        
            let _ = tx_tg.send(ServiceCommand::SendMessage(
                format!("🔄 Раунд {round} окончен, открыто {}/3. Slippage = {} bps", minted.len(), slippage)
            ));
        }
        
        // окончательная проверка
        if minted.len() != 3 {
            let _ = tx_tg.send(ServiceCommand::SendMessage(
                format!("❌ За 3 раунда открыто только {}/3. Закрываю то, что было.", minted.len())
            ));
        
            for &(_, ref pm) in &minted {
                let _ = close_whirlpool_position(Pubkey::from_str(pm)?, 150u16).await;
            }
            bail!("Не удалось открыть все три диапазона");
        }
        
        upper_exit = pool_cfg.position_1.as_ref().unwrap().upper_price;
        lower_exit = pool_cfg.position_3.as_ref().unwrap().lower_price;
    } else if need_open_new {
        // ── 1. Текущие границы в “SOL за токен-B” (display) ─────────────────
        let low_disp  = price_disp * 0.995;
        let high_disp = price_disp * 1.005;

        // ── 2. Цена 1 токена-B в SOL, а затем в USD ─────────────────────────
        let price_b_in_sol = price_disp;                 // invert всегда true здесь
        let sol_usd        = get_sol_price_usd().await?;
        let tok_b_usd      = price_b_in_sol * sol_usd;   // ETH ≈ 2 400 $, RAY ≈ 2 $

        // ── 3. Сколько B нужно, чтобы занять половину капитала ──────────────
        let amount_tok_b = (capital_usd / 2.0) / tok_b_usd;

        // — Telegram + DEBUG —
        let telegram_msg = format!(
            "🔔 Открываю центр {} [{:.6}; {:.6}], вес ${:.2} amount Tok_b: {:.6}",
            pool_cfg.name, low_disp, high_disp, capital_usd, amount_tok_b
        );
        println!(
            "DBG: B_in_SOL={:.6}, tok_b_usd={:.2}, amount_tok_b={:.6}",
            price_b_in_sol, tok_b_usd, amount_tok_b
        );
        let _ = tx_tg.send(ServiceCommand::SendMessage(telegram_msg));

        // ── 4. Границы, которые ждёт SDK (B за SOL) ─────────────────────────
        let low_raw  = 1.0 / high_disp;
        let high_raw = 1.0 / low_disp;

        // ── 5. Открываем позицию ────────────────────────────────────────────
        open_with_funds_check_universal(
            low_raw,
            high_raw,
            amount_tok_b,
            pool_cfg.clone(),
            150,
        ).await?;


        pool_cfg.position_2 = Some(LiqPosition {
            role:          Role::Middle,
            position_address: None,
            position_nft:     None,
            upper_price: high_disp,     // храним и контролируем в display-виде
            lower_price: low_disp,
        });
        upper_exit = high_disp;
        lower_exit = low_disp;
    } else {
        //------------------------------------------------------------------
        //  ✦  Блок, когда позиции уже существуют (need_open_new == false) ✦
        //------------------------------------------------------------------

        // 1) получаем все позиции текущего owner-а в данном пуле
        let list = list_positions_for_owner(Some(whirl_pk)).await?;
        if list.is_empty() {
            bail!("need_open_new == false, но в пуле {} нет ни одной позиции", pool_cfg.name);
        }

        // 2) собираем информацию и сортируем её по lower_price
        let mut infos = Vec::<crate::types::PoolPositionInfo>::new();
        for p in list {
            if let PositionOrBundle::Position(hp) = p {
                if let Ok(i) = fetch_pool_position_info(&pool_cfg, Some(&hp.address.to_string())).await {
                    infos.push(i);
                }
            }
        }
        infos.sort_by(|a,b| a.lower_price.partial_cmp(&b.lower_price).unwrap());

        // 3) заполняем pool_cfg и границы выхода
        if three_ranges {
            // ожидаем ровно 3 диапазона
            if infos.len() != 3 {
                bail!("В пуле {} найдено {} позиций, а должно быть 3", pool_cfg.name, infos.len());
            }

            pool_cfg.position_1 = Some(LiqPosition {
                role: Role::Up,
                position_address: None,
                position_nft:     None,
                upper_price: infos[0].upper_price,
                lower_price: infos[0].lower_price,
            });
            pool_cfg.position_2 = Some(LiqPosition {
                role: Role::Middle,
                position_address: None,
                position_nft:     None,
                upper_price: infos[1].upper_price,
                lower_price: infos[1].lower_price,
            });
            pool_cfg.position_3 = Some(LiqPosition {
                role: Role::Down,
                position_address: None,
                position_nft:     None,
                upper_price: infos[2].upper_price,
                lower_price: infos[2].lower_price,
            });

            // «верхний вылет» контролируем по верхней границе первой (Up) позиции,
            // «нижний вылет» — по нижней границе третьей (Down), как и раньше
            upper_exit = infos[0].upper_price;
            lower_exit = infos[2].lower_price;
        } else {
            // одиночный диапазон (RAY/SOL, WETH/SOL) – берём первую позицию
            let i = &infos[0];
            pool_cfg.position_2 = Some(LiqPosition {
                role: Role::Middle,
                position_address: None,
                position_nft:     None,
                upper_price: i.upper_price,
                lower_price: i.lower_price,
            });
            upper_exit = i.upper_price;
            lower_exit = i.lower_price;
        }

        // 4) информируем пользователя
        let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
            "🔄 {}: найдено уже открытых позиций — переходим к мониторингу.\n\
             Диапазон контроля [{:.6}; {:.6}]",
            pool_cfg.name, lower_exit, upper_exit
        )));
    }


    // ───── 3. Мониторинг ────────────────────────────────────────────────
    let mut price_itv  = tokio::time::interval(Duration::from_secs(5));

    let mut out_of_range_since: Option<Instant> = None;       // только для «однодиапазонных»
    const WAIT_BEFORE_REDEPLOY: Duration = Duration::from_secs(3600);   // 1 ч

    loop {
        tokio::select! {
            _ = price_itv.tick() => {
                // 3.1 обновляем текущую цену
                let acct  = match safe_get_account(&rpc, &whirl_pk).await {
                    Ok(a) => a,
                    Err(e) => {           // сетевые ошибки просто пропускаем этот тик
                        let _ = tx_tg.send(ServiceCommand::SendMessage(
                            format!("🌐 {}: RPC error ({e}), ждём следующий тик", pool_cfg.name)));
                        continue;
                    }
                };
                let curr_raw = orca_whirlpools_core::sqrt_price_to_price(
                    orca_whirlpools_client::Whirlpool::from_bytes(&acct.data)?.sqrt_price.into(),
                    dec_a, dec_b
                );
                price = norm_price(curr_raw, invert);

                // 3.2 допустимое отклонение
                let kof = if pool_cfg.name != "SOL/USDC" { 0.008 } else { 0.0015 };
                let out_of_range = price > upper_exit * (1.0 + kof) ||
                                price < lower_exit * (1.0 - kof);

                // ── A)  пул SOL/USDC — закрываем немедленно ─────────────────
                if pool_cfg.name == "SOL/USDC" {
                    if out_of_range {
                        let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                            "⚠️ {}: price {:.6} вышла за [{:.6}; {:.6}] — закрываю позиции",
                            pool_cfg.name, price, lower_exit, upper_exit
                        )));
                        if let Err(e) = close_and_report(&rpc, &pool_cfg, whirl_pk, &tx_tg).await {
                            let _ = tx_tg.send(ServiceCommand::SendMessage(
                                format!("❌ Ошибка при закрытии {}: {:?}", pool_cfg.name, e)
                            ));
                        }
                        break;      // run_pool_with_restart запустит всё заново
                    }
                    continue;       // переходим к следующему tick
                }

                // ── B)  все остальные пулы — ждём 1 ч, вернётся ли цена ──────
                if out_of_range {
                    match out_of_range_since {
                        // первый выход за границы — запускаем таймер
                        None => {
                            out_of_range_since = Some(Instant::now());
                            let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                                "⚠️ {}: price {:.6} вышла за [{:.6}; {:.6}]. \
                                Ждём 1 ч, вдруг вернётся…",
                                pool_cfg.name, price, lower_exit, upper_exit
                            )));
                        }
                        // таймер уже идёт — проверяем, истёк ли час
                        Some(t0) if t0.elapsed() >= WAIT_BEFORE_REDEPLOY => {
                            let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                                "⏰ {}: прошло 1 ч, цена {:.6} всё ещё вне диапазона — \
                                перевыставляем позиции",
                                pool_cfg.name, price
                            )));
                            if let Err(e) = close_and_report(&rpc, &pool_cfg, whirl_pk, &tx_tg).await {
                                let _ = tx_tg.send(ServiceCommand::SendMessage(
                                    format!("❌ Ошибка при закрытии {}: {:?}", pool_cfg.name, e)
                                ));
                            }
                            break;      // main перезапустит корутину и откроет заново
                        }
                        _ => {} // ещё не прошёл час — просто ждём
                    }
                } else {
                    // цена вернулась внутрь диапазона — сбрасываем таймер (если был)
                    if out_of_range_since.is_some() {
                        let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                            "✅ {}: price {:.6} снова в диапазоне, продолжаем работу",
                            pool_cfg.name, price
                        )));
                        out_of_range_since = None;
                    }
                }
            }
            _ = close_ntf.notified() => {
                let _ = tx_tg.send(ServiceCommand::SendMessage(
                    format!("🔔 {}: получен сигнал CLOSE ALL — выходим из пула", pool_cfg.name)
                ));
                // просто выходим: run_pool_with_restart перезапустит
                break;
            }
        }
    }

    Ok(())
}


#[inline]
fn norm_price(raw: f64, invert: bool) -> f64 {
    if invert { 1.0 / raw } else { raw }
}

// reporter.rs
pub async fn build_pool_report(cfg: &PoolConfig) -> anyhow::Result<String> {

    let rpc = utils::init_rpc();
    let whirl_pk = Pubkey::from_str(cfg.pool_address)?;
    let whirl_acct = safe_get_account(&rpc, &whirl_pk).await?;
    let whirl = orca_whirlpools_client::Whirlpool::from_bytes(&whirl_acct.data)?;

    // текущая цена
    let da = cfg.decimal_a;
    let db = cfg.decimal_b;
    let raw = orca_whirlpools_core::sqrt_price_to_price(whirl.sqrt_price.into(), da as u8, db as u8);
    let price_disp = if cfg.name != "SOL/USDC"{ 1.0 / raw } else { raw };

    // все позиции owner-a в этом пуле
    let list = list_positions_for_owner(Some(whirl_pk)).await?;  // ← ваша обёртка SDK
    if list.is_empty() {
        return Ok(format!("📊 {} — позиций нет.\n", cfg.name));
    }

    // собираем инфо по позициям
    let mut infos = Vec::new();
    for p in list {
        if let PositionOrBundle::Position(hp) = p {
            if let Ok(i) = fetch_pool_position_info(cfg, Some(&hp.address.to_string())).await {
                infos.push(i);
            }
        }
    }

    // сортируем по lower_price (чтобы 🍏🍊🍎 шли «сверху вниз»)
    infos.sort_by(|a,b| a.lower_price.partial_cmp(&b.lower_price).unwrap());

    // генерируем отчёт
    let mut rep = format!("📊 {} — Price {:.6}\n", cfg.name, price_disp);
    let icons = ["🍏","🍊","🍎"];
    let mut total = 0.0;
    for (idx, i) in infos.iter().enumerate() {
        let sym = if price_disp > i.lower_price && price_disp < i.upper_price {
            icons.get(idx).unwrap_or(&"✅")
        } else { "----" };
        rep.push_str(&format!(
            "{}P{}: R[{:.4}–{:.4}], ${:.4}\n",
            sym, idx+1, i.lower_price, i.upper_price, i.sum
        ));
        total += i.sum;
    }
    rep.push_str(&format!("— Всего: ${:.4}\n\n", total));
    Ok(rep)
}

async fn close_existing_owner_positions(pool: &PoolConfig) -> anyhow::Result<()> {
    let list = list_positions_for_owner(Some(Pubkey::from_str(pool.pool_address)?)).await?;
    for p in list {
        if let PositionOrBundle::Position(pos) = p {
            // закрываем любую позицию owner-а в этом пуле
            let _ = close_whirlpool_position(pos.data.position_mint, 150.0 as u16).await;
        }
    }
    Ok(())
}

