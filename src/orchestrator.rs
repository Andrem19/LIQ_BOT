
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::{read_keypair_file, Keypair}, signer::Signer};
use spl_associated_token_account::get_associated_token_address;
use std::{str::FromStr, time::Duration};
use anyhow::Result;
use orca_whirlpools::PositionOrBundle;
use crate::{exchange::helpers, types::PoolConfig};
use std::time::Instant;
use std::sync::atomic::AtomicBool;
use orca_whirlpools_core::U128;
use crate::database::positions;
use anyhow::bail;
use crate::types::PoolPositionInfo;
use crate::exchange::helpers::get_kline;
use crate::orchestrator::helpers::convert_timeframe;
use anyhow::Context;
use crate::pyth_ws::subscribe;
use std::sync::atomic::Ordering;
use crate::database::positions::record_position_metrics;

use crate::params::{WEIGHTS};
use crate::types::{LiqPosition, Role};
use crate::orca_logic::helpers::{calc_bound_prices_struct, calc_range_allocation_struct};
use crate::wirlpool_services::wirlpool::{open_with_funds_check_universal, close_all_positions, list_positions_for_owner};
use crate::wirlpool_services::get_info::fetch_pool_position_info;
use crate::telegram_service::tl_engine::ServiceCommand;
use tokio::sync::mpsc::UnboundedSender;
use spl_token::state::Mint;
use tokio::sync::Notify;
use crate::database::triggers;

use crate::params::{WALLET_MUTEX, USDC, WSOL};
use crate::utils::utils;
use crate::utils::{safe_get_account, swap_excess_to_usdc};
use std::sync::Arc;
use spl_token::solana_program::program_pack::Pack;
use crate::exchange::helpers::get_atr;
use crate::orca_logic::helpers::get_sol_price_usd;
use crate::wirlpool_services::wirlpool::close_whirlpool_position;


fn get_pyth_feed_id(symbol: &str) -> &'static str {
    match symbol {
        "SOL/USDC"  => "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
        "SOL/WETH" => "0xde87506dabfadbef89af2d5d796ebae80ddaea240fc7667aa808fce3629cd8fb",
        _ => panic!("unsupported symbol: {}", symbol),
    }
}
//-------------------------------- helper -----------------------------------
async fn close_and_report(
    rpc: &RpcClient,
    pool_cfg: &PoolConfig,
    whirl_pk: Pubkey,
    tx_tg: &UnboundedSender<ServiceCommand>,
    lower: bool,
) -> Result<()> {
    // 1) закрываем все позиции ЭТОГО пула
    close_all_positions(150, Some(whirl_pk)).await?;
    if lower {
        let candels_1m = get_kline("SOLUSDT", 250, 1).await?;

        let opens:   Vec<f64> = candels_1m.iter().map(|c| c.open).collect();
        let highs:   Vec<f64> = candels_1m.iter().map(|c| c.high).collect();
        let lows:    Vec<f64> = candels_1m.iter().map(|c| c.low).collect();
        let closes:  Vec<f64> = candels_1m.iter().map(|c| c.close).collect();
        let volumes: Vec<f64> = candels_1m.iter().map(|c| c.volume).collect();
    
        // 3) Конвертируем в hourly
        let (o1h, h1h, l1h, c1h, v1h) =
            convert_timeframe(&opens, &highs, &lows, &closes, &volumes, 5, 28);

        let atr: Vec<f64> = get_atr(&o1h, &h1h, &l1h, &c1h, &v1h, 14)?;
        let mean_atr_2 = (atr.last().unwrap() + atr.get(atr.len().wrapping_sub(2)).unwrap()) / 2.0;
        if mean_atr_2 > 0.5 {
            _ = swap_excess_to_usdc(WSOL, 9, 0.05).await?;
            triggers::auto_trade_switch(true, tx_tg).await?;
            let _ = tx_tg.send(ServiceCommand::SendSignal("Signal! Lower breakthrough".to_string()));
        }
    }

    // 2) Балансы после закрытия
    let _lock   = WALLET_MUTEX.lock().await;              // единый замок
    let wallet  = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    let lamports = rpc.get_balance(&wallet_pk).await?;
    let bal_sol  = lamports as f64 / 1e9;

    let mint_b  = Pubkey::from_str(&pool_cfg.mint_b)?;
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
        let sol_usd: f64 = get_sol_price_usd(WSOL, true).await?;
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
    min_restart: u64,
    range: Option<f32>,
) -> Result<()> {
    let need_open_new = need_new.load(Ordering::SeqCst);
    need_new.store(true, Ordering::SeqCst);
    
    if need_open_new {
        close_existing_owner_positions(&pool_cfg).await?;
    }
    // ───── 0. RPC / Whirlpool meta ───────────────────────────────────────
    let rpc       = utils::init_rpc();
    let whirl_pk  = Pubkey::from_str(&pool_cfg.pool_address)?;
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
                    idx,
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

        let (low_perc, high_perc) = match calculate_range(range) {
            Some((low, high)) => (low, high),
            None => {
                println!("Не удалось вычислить диапазон");
                return Ok(()); // или return Err(...), если в async fn
            }
        };
        let low_disp  = low_perc * price_disp;
        let high_disp = high_perc * price_disp;
        println!("range: {} - {}", low_disp, high_disp);


        // ── 2. Цена 1 токена-B в SOL, а затем в USD ─────────────────────────
        let price_b_in_sol = price_disp;                 // invert всегда true здесь
        let sol_usd        = get_sol_price_usd(WSOL, true).await?;
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
            0
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
        infos.sort_by(|a, b| {
            // теперь сравниваем сначала b с a
            b.lower_price.partial_cmp(&a.lower_price).unwrap()
        });

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
            "🔄 {}: найдено уже открытых позиций {:} — переходим к мониторингу",
            pool_cfg.name, infos.len()
        )));
    }

    _ = triggers::open_position_switch(true, &tx_tg).await?;
    // ───── 3. Мониторинг ────────────────────────────────────────────────

    // ➊ Price-feed Pyth для базового токена пула
    //    (подберите ID один раз и храните в cfg, здесь для примера SOL/USD)
    let feed_id = get_pyth_feed_id(&pool_cfg.name).to_string();

    // канал, в который pyth_ws::subscribe будет писать Option<f64>
    let mut pyth_rx = subscribe(feed_id)
        .await
        .context("pyth subscribe")?;

    // fallback-таймер HTTP (Whirlpool RPC) – раз в 15 с
    let mut http_itv = tokio::time::interval(Duration::from_secs(15));

    let mut out_of_range_since: Option<Instant> = None;
    let mut wait_before_redeploy = Duration::from_secs(min_restart * 60);

    loop {
        tokio::select! {
            // ➋ получили новое значение из Pyth-канала
            _ = pyth_rx.changed() => {
                // ➊ извлекаем Option<f64> и явно завершаем borrow до await
                let price_opt = {
                    // guard живёт только в этом блоке!
                    let g = pyth_rx.borrow();
                    *g
                };
            
                if let Some(p) = price_opt {
                    let price_display = if invert { 1.0 / p } else { p };
            
                    // ➋ теперь guard уже drop-нут, можно safely await
                    if check_bounds_and_maybe_close(
                        price_display,
                        upper_exit,
                        lower_exit,
                        min_restart,
                        &pool_cfg,
                        &tx_tg,
                        &rpc,
                        whirl_pk,
                        &mut out_of_range_since,
                        &mut wait_before_redeploy,
                    ).await? {
                        break;            // позиции закрыты — выходим
                    }
                }
            }
            

            // ➌ fallback — старая логика через getAccount раз в 15 с
            _ = http_itv.tick() => {
                let acct = match safe_get_account(&rpc, &whirl_pk).await {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = tx_tg.send(ServiceCommand::SendMessage(
                            format!("🌐 {}: RPC error ({e}), ждём следующий тик", pool_cfg.name)));
                        continue;
                    }
                };

                let curr_raw = orca_whirlpools_core::sqrt_price_to_price(
                    orca_whirlpools_client::Whirlpool::from_bytes(&acct.data)?
                        .sqrt_price
                        .into(),
                    dec_a,
                    dec_b,
                );
                let price_display = norm_price(curr_raw, invert);

                if check_bounds_and_maybe_close(
                    price_display,
                    upper_exit,
                    lower_exit,
                    min_restart,
                    &pool_cfg,
                    &tx_tg,
                    &rpc,
                    whirl_pk,
                    &mut out_of_range_since,
                    &mut wait_before_redeploy
                ).await? {
                    break;
                }
            }

            // ➍ внешняя команда «закрыть всё»
            _ = close_ntf.notified() => {
                let _ = tx_tg.send(ServiceCommand::SendMessage(
                    format!("🔔 {}: получен сигнал CLOSE ALL — выходим из пула", pool_cfg.name)
                ));
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
pub struct PoolReport {
    pub text:   String, // готовый текст для Telegram
    pub total:  f64,    // итоговая $-стоимость позиций
}
// reporter.rs
pub async fn build_pool_report(cfg: &PoolConfig) -> Result<PoolReport> {
    // 1. RPC / Whirlpool
    let rpc      = utils::init_rpc();
    let whirl_pk = Pubkey::from_str(&cfg.pool_address)?;
    let whirl_ac = safe_get_account(&rpc, &whirl_pk).await?;
    let whirl    = orca_whirlpools_client::Whirlpool::from_bytes(&whirl_ac.data)?;

    // 2. Текущая цена
    let raw = orca_whirlpools_core::sqrt_price_to_price(U128::from(whirl.sqrt_price),
                                  cfg.decimal_a as u8, cfg.decimal_b as u8);
    let price_disp = if cfg.name.starts_with("SOL/") { raw } else { 1.0 / raw };

    // 3. Все позиции владельца
    let list = list_positions_for_owner(Some(whirl_pk)).await?;
    if list.is_empty() {
        return Ok(PoolReport{
            text:  format!("📊 {} — позиций нет.\n", cfg.name),
            total: 0.0,
        });
    }
    let candels_1m = get_kline("SOLUSDT", 250, 1).await?;

    let opens:   Vec<f64> = candels_1m.iter().map(|c| c.open).collect();
    let highs:   Vec<f64> = candels_1m.iter().map(|c| c.high).collect();
    let lows:    Vec<f64> = candels_1m.iter().map(|c| c.low).collect();
    let closes:  Vec<f64> = candels_1m.iter().map(|c| c.close).collect();
    let volumes: Vec<f64> = candels_1m.iter().map(|c| c.volume).collect();

    // 3) Конвертируем в hourly
    let (o1h, h1h, l1h, c1h, v1h) =
        convert_timeframe(&opens, &highs, &lows, &closes, &volumes, 5, 28);

    let atr: Vec<f64> = get_atr(&o1h, &h1h, &l1h, &c1h, &v1h, 14)?;


    let penult_idx = o1h.len().saturating_sub(2);
    let last_idx   = o1h.len().saturating_sub(1);

    let penultimate_open_close = helpers::percentage_change(o1h[penult_idx], c1h[penult_idx]);
    let last_open_close = helpers::percentage_change(o1h[last_idx], c1h[last_idx]);
    let last_atr = atr.last().unwrap();

    let report = format!("ATR: {:.2}, P: {:.4} L: {:.4}\n\n", last_atr, penultimate_open_close, last_open_close);

    // 4. Информация по позициям
    let mut infos = Vec::new();
    for p in list {
        if let PositionOrBundle::Position(hp) = p {
            if let Ok(i) = fetch_pool_position_info(cfg, Some(&hp.address.to_string())).await {
                infos.push(i);
            }
        }
    }
    infos.sort_by(|a, b| b.lower_price.partial_cmp(&a.lower_price).unwrap());

    // 5. Формируем текст и суммируем total
    let icons = ["🍏","🍊","🍎"];
    let mut txt   = format!("📊 {} — Price {:.6}\n", cfg.name, price_disp);
    let mut total = 0.0;
    let mut tv    = 0.0;

    for (idx, i) in infos.iter().enumerate() {
        let l = if cfg.name == "SOL/USDT" { 1.0/i.upper_price } else { i.lower_price };
        let u = if cfg.name == "SOL/USDT" { 1.0/i.lower_price } else { i.upper_price };
        let mark = if price_disp > l && price_disp < u {
            icons.get(idx).unwrap_or(&"✅")
        } else { "----" };
        txt.push_str(&format!("{mark}P{}: R[{:.4}–{:.4}], ${:.4}\n", idx+1, l, u, i.sum));
        total += i.sum;
        tv    += i.value_a + i.value_b;
    }
    txt.push_str(&format!("\n"));

    let mut db_cfg = cfg.clone();

    // ВСТАВЛЯЕМ В RANGE из infos через helper:
    apply_position_range(&mut db_cfg.position_1, infos.get(0));
    apply_position_range(&mut db_cfg.position_2, infos.get(1));
    apply_position_range(&mut db_cfg.position_3, infos.get(2));

    // ────────── дальше считаем комиссии и TV ───────────────────────────────────
    let comm1 = infos.get(0).map(|i| i.sum).unwrap_or(0.0);
    let comm2 = infos.get(1).map(|i| i.sum).unwrap_or(0.0);
    let comm3 = infos.get(2).map(|i| i.sum).unwrap_or(0.0);
    let total_current = tv;

    if let Err(e) = record_position_metrics(&db_cfg, comm1, comm2, comm3, total_current).await {
        log::error!("Не удалось сохранить метрики для {}: {}", cfg.name, e);
    }

    let mut init_value = 0.0;
    match positions::get_pool_config().await {
        Ok(Some(ps))  => {
            init_value = ps.total_value_open;
        }, // выходим, если триггер включён
        Ok(None) => {
        },
        Err(e) => {
            // не удалось прочитать из БД — логируем, ждём и пробуем снова
            log::error!("Error reading positions from bd: {}", e);
        }
    }
    let flag = if init_value > tv {"▼"} else {"▲"};

    txt.push_str(&format!("{} — Init TV: {:.2} — Now TV: ${:.2}\n",flag, init_value, tv));
    txt.push_str(&report);

    // ──────────────────────────────────────────────────────────────────────

    Ok(PoolReport { text: txt, total })
}

async fn close_existing_owner_positions(pool: &PoolConfig) -> anyhow::Result<()> {
    let list = list_positions_for_owner(Some(Pubkey::from_str(&pool.pool_address)?)).await?;
    for p in list {
        if let PositionOrBundle::Position(pos) = p {
            // закрываем любую позицию owner-а в этом пуле
            let _ = close_whirlpool_position(pos.data.position_mint, 150.0 as u16).await;
        }
    }
    Ok(())
}

pub fn calculate_range(range: Option<f32>) -> Option<(f64, f64)>{
    let value = match range {
        Some(v) => v,
        None => {
            println!("Значение отсутствует");
            return None;
        }
    };
    let half = value /2.0;
    Some((1.0-half as f64, 1.0+half as f64))
}

async fn check_bounds_and_maybe_close(
    price: f64,
    upper_exit: f64,
    lower_exit: f64,
    min_restart: u64,
    pool_cfg: &PoolConfig,
    tx_tg: &UnboundedSender<ServiceCommand>,
    rpc: &RpcClient,
    whirl_pk: Pubkey,
    out_of_range_since: &mut Option<Instant>,
    wait_before_redeploy: &mut Duration,
) -> Result<bool> {
    // 3.2 допустимое отклонение
    const KOF: f64 = 0.0015;

    let out_of_range = price > upper_exit * (1.0 + KOF)
        || price < lower_exit;

    if out_of_range {
        // если цена ушла ниже — немедленно выполняем redeploy
        let lower = price < lower_exit;
        if lower {
            *wait_before_redeploy = Duration::from_secs(0);
        }

        match out_of_range_since {
            // первый выход — запускаем таймер
            None => {
                *out_of_range_since = Some(Instant::now());
                let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                    "⚠️ {}: price {:.6} вышла за [{:.6}; {:.6}]. Ждём {:.2} мин",
                    pool_cfg.name, price, lower_exit, upper_exit, min_restart as f64
                )));
                // не выходим из цикла сразу, ждём
                Ok(false)
            }
            // таймер уже запущен — проверяем, не пора ли перезапускать
            Some(t0) if t0.elapsed() >= *wait_before_redeploy => {
                let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                    "⏰ {}: прошло {:.2} мин, цена {:.6} всё ещё вне диапазона — перевыставляем позиции",
                    pool_cfg.name, min_restart as f64, price
                )));
                // пытаемся закрыть
                if let Err(e) = close_and_report(rpc, pool_cfg, whirl_pk, tx_tg, lower).await {
                    let _ = tx_tg.send(ServiceCommand::SendMessage(
                        format!("❌ Ошибка при закрытии {}: {:?}", pool_cfg.name, e),
                    ));
                }
                // сигнализируем, что нужно break
                Ok(true)
            }
            // ещё не время
            _ => Ok(false),
        }
    } else {
        // вернулись в диапазон — сбрасываем таймер
        if out_of_range_since.is_some() {
            let _ = tx_tg.send(ServiceCommand::SendMessage(format!(
                "✅ {}: price {:.6} снова в диапазоне, продолжаем работу",
                pool_cfg.name, price
            )));
            *out_of_range_since = None;
        }
        Ok(false)
    }
}

fn apply_position_range(
    slot: &mut Option<LiqPosition>,
    info_opt: Option<&PoolPositionInfo>,
) {
    if let Some(info) = info_opt {
        let role = slot
            .as_ref()
            .map(|p| p.role.clone())
            .unwrap_or(Role::Middle);
        *slot = Some(LiqPosition {
            role,
            position_address: None,
            position_nft:     None,
            upper_price:      info.upper_price,
            lower_price:      info.lower_price,
        });
    }
}
