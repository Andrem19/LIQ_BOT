use anyhow::{anyhow, Result};
use std::{str::FromStr, sync::Arc, time::Duration};

use solana_client::{
    nonblocking::rpc_client::RpcClient,
};
use spl_associated_token_account;
use spl_token;
use orca_whirlpools_client::ID;
use solana_sdk::pubkey::Pubkey;
use anyhow::Context;
use tokio::time::sleep;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::{
    signature::{Keypair, Signer},
};
use crate::database::history;
use crate::database::triggers;
use orca_whirlpools::increase_liquidity_instructions;
use orca_whirlpools_core::sqrt_price_to_tick_index;
use spl_associated_token_account::instruction::create_associated_token_account;
use orca_whirlpools_client::Position;
use anyhow::bail;
use crate::{utils::get_sol_price_usd, params::WSOL};
use orca_whirlpools::{
    fetch_positions_for_owner,
    PositionOrBundle,
};
use crate::database::positions;
use orca_whirlpools::HydratedBundledPosition;
use orca_whirlpools::ClosePositionInstruction;
use orca_whirlpools::OpenPositionInstruction;
use orca_whirlpools_core::IncreaseLiquidityQuote;
use orca_whirlpools_client::{Whirlpool};
use orca_whirlpools_core::price_to_tick_index;
use orca_whirlpools::{
    close_position_instructions, harvest_position_instructions, open_position_instructions, 
    set_whirlpools_config_address, HarvestPositionInstruction, IncreaseLiquidityParam,
    WhirlpoolsConfigInput,
};
use crate::utils::utils;
use crate::params::{WALLET_MUTEX, USDC, OVR};
use orca_whirlpools_core::tick_index_to_price;
use orca_whirlpools_core::{CollectFeesQuote, U128, sqrt_price_to_price};
use crate::wirlpool_services::swap::execute_swap_tokens;
use crate::types::{PoolConfig, OpenPositionResult};
use crate::utils::op;
use crate::utils::fetch_wallet_balance_info;


const GAP_SOL:  f64 = 0.002;
const GAP_B: f64 = 0.005;

#[derive(Debug)]
pub enum Mode { OnlyA, OnlyB, Mixed }

pub async fn close_whirlpool_position(
    position_mint: Pubkey,
    base_slippage: u16,        // оставил параметр, но он будет «первой попыткой»
) -> anyhow::Result<()> {
    use solana_sdk::signature::Signer;

    const STEPS: &[u16] = &[150, 500, 1_200];   // bps

    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(op("set_whirlpools_config_address"))?;

    let rpc        = utils::init_rpc();
    let wallet     = utils::load_wallet()?;
    let wallet_pk  = wallet.pubkey();

    // перебираем slippage из STEPS, но первую попытку берём base_slippage
    for (idx, &slip) in std::iter::once(&base_slippage).chain(STEPS.iter()).enumerate() {
        let ClosePositionInstruction {
            instructions,
            additional_signers,
            ..
        } = match close_position_instructions(&rpc, position_mint, Some(slip), Some(wallet_pk)).await {
            Ok(v) => v,
            Err(e) => {
                if idx == STEPS.len() - 1 {
                    return Err(anyhow!("all attempts failed: {}", e.to_string()));
                }
                continue;           // пробуем следующий slippage
            }
        };

        let mut signers: Vec<&Keypair> = vec![&wallet];
        signers.extend(additional_signers.iter());

        match utils::send_and_confirm(rpc.clone(), instructions, &signers).await {
            Ok(_) => return Ok(()),                     // 🎉 всё ок
            Err(e) if e.to_string().contains("0x1782") && idx < STEPS.len() => {
                // только TokenMinSubceeded → эскалируем slippage
                log::warn!(
                    "close_position: {:?} failed with TokenMinSubceeded; retry with slippage {} bps",
                    position_mint, STEPS[idx]
                );
                continue;
            }
            Err(e) => return Err(e),                    // любая другая ошибка
        }
    }

    // если вдруг вышли из цикла без return
    Err(anyhow::anyhow!("close_position: all attempts failed"))
}


/// Собирает комиссии и возвращает `CollectFeesQuote`.
pub async fn harvest_whirlpool_position(position_mint: Pubkey) -> Result<CollectFeesQuote> {
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(op("set_whirlpools_config_address"))?;
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    let HarvestPositionInstruction {
        instructions,
        additional_signers,
        fees_quote,
        ..
    } = harvest_position_instructions(&rpc, position_mint, Some(wallet_pk))
        .await
        .map_err(op("harvest_position_instructions"))?;

    if fees_quote.fee_owed_a == 0 && fees_quote.fee_owed_b == 0 {
        return Err(anyhow!("No fees to collect for position {}", position_mint));
    }

    let mut signers = Vec::with_capacity(1 + additional_signers.len());
    signers.push(&wallet);
    for kp in &additional_signers {
        signers.push(kp);
    }

    utils::send_and_confirm(rpc, instructions, &signers)
        .await
        .map_err(op("send_and_confirm"))?;
    Ok(fees_quote)
}

pub struct HarvestSummary {
    /// Собрано токена A (пример: WSOL) в «целых» единицах.
    pub amount_a: f64,
    /// Собрано токена B (пример: USDC) в «целых» единицах.
    pub amount_b: f64,
    /// Цена токена A в токенах B (SOL → USDC).
    pub price_a_in_usd: f64,
    /// Общая стоимость сборов в USD (amount_b + amount_a * price).
    pub total_usd: f64,
}

/// Преобразует `CollectFeesQuote` в «читаемые» суммы и считает стоимость SOL и общую сумму в USD.
/// Асинхронно читает on-chain `sqrt_price` из пула, чтобы получить актуальный SOL/USD.
pub async fn summarize_harvest_fees(
    pool: &PoolConfig,
    fees: &CollectFeesQuote,
) -> Result<HarvestSummary> {
    // 1) RPC
    let rpc: Arc<RpcClient> = utils::init_rpc();
    
    // 2) Состояние пула
    let whirl_pk = Pubkey::from_str(&pool.pool_address)?;
    let acct = rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&acct.data)?;

    // 3) Приводим decimals к u8
    let dec_a: u8 = pool.decimal_a
        .try_into()
        .map_err(|_| anyhow!("decimal_a {} does not fit into u8", pool.decimal_a))?;
    let dec_b: u8 = pool.decimal_b
        .try_into()
        .map_err(|_| anyhow!("decimal_b {} does not fit into u8", pool.decimal_b))?;

    // 4) Цена A в B (SOL → USDC например)
    let price_a_in_usd = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);
    

    // 5) «Читаемые» количества
    let amount_a = fees.fee_owed_a as f64 / 10f64.powi(dec_a as i32);
    let amount_b = fees.fee_owed_b as f64 / 10f64.powi(dec_b as i32);
    let price_a = price_a_in_usd*amount_a;

    // 6) Общая стоимость в USD
    let total_usd = amount_b + (amount_a * price_a_in_usd);

    Ok(HarvestSummary {
        amount_a,
        amount_b,
        price_a_in_usd: price_a,
        total_usd,
    })
}

pub async fn open_with_funds_check_universal(
    price_low: f64,
    price_high: f64,
    initial_amount_b: f64,
    pool: PoolConfig,
    slippage: u16,
    number: usize
) -> Result<OpenPositionResult> {
    let gap_b = if pool.name == "RAY/SOL" || pool.name == "SOL/USDC" {
        GAP_B
    } else if pool.name == "WBTC/SOL" {
        0.000002
    } else {
        0.00002
    };
    // ───────── 1. RPC / Wallet / SDK ───────────────────────────────────────

    let rpc       = utils::init_rpc();
    let _wallet_guard = WALLET_MUTEX.lock().await;

    let wallet    = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();


    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow!("SDK config failed: {e}"))?;

        let native_mint = Pubkey::from_str(WSOL)?;
        let ata_a = get_associated_token_address(&wallet_pk, &native_mint);
        
        if rpc.get_account(&ata_a).await.is_err() {
            let ix = create_associated_token_account(
                &wallet_pk,             // funding_address
                &wallet_pk,             // wallet_address
                &native_mint,           
                &spl_token::id(),
            );
            utils::send_and_confirm(rpc.clone(), vec![ix], &[&wallet]).await?;
        }

    // ───────── 2. Пул, децималы, tick-spacing ──────────────────────────────
    let whirl_pk  = Pubkey::from_str(&pool.pool_address)?;

    let whirl     = Whirlpool::from_bytes(&rpc.get_account(&whirl_pk).await?.data)?;
    let dec_a     = pool.decimal_a as u8;                // WSOL → 9
    let dec_b     = pool.decimal_b as u8;                // USDC(6) / RAY(6) / whETH(8)
    let spacing   = whirl.tick_spacing as i32;

    // ───────── 3. Валидные тики, выровненные цены ─────────────────────────
    let (tick_l, tick_u) = nearest_valid_ticks(price_low, price_high, spacing, dec_a, dec_b);

    let price_low_aligned  = tick_index_to_price(tick_l, dec_a, dec_b);
    let price_high_aligned = tick_index_to_price(tick_u, dec_a, dec_b);

    // ───────── 4. Депозиты в атомах ───────────────────────────────────────
    // token B (USDC/RAY/whETH) в атомах
    let dep_b_atoms = (initial_amount_b * 10f64.powi(dec_b as i32)) as u64;

    // цена WSOL (token A) в B, нужна для конвертации
    let price_a_in_b = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);

    // эквивалент token A в атомах
    let dep_a_atoms = ((initial_amount_b / price_a_in_b) * 10f64.powi(dec_a as i32)) as u64;


    // ───────── 5. Выбор liquidity_param ───────────────────────────────────
    let liquidity_param = if price_low_aligned > price_a_in_b {

        // диапазон выше рынка → 100 % SOL (A)
        IncreaseLiquidityParam::TokenA(dep_a_atoms.max(1))
    } else if price_high_aligned < price_a_in_b {

        // диапазон ниже рынка → 100 % токен B
        IncreaseLiquidityParam::TokenB(dep_b_atoms.max(1))
    } else {

        // диапазон пересекает рынок → сначала TokenB, потом Liquidity
        let OpenPositionInstruction { quote: q, .. } = open_position_instructions(
            &rpc,
            whirl_pk,
            price_low_aligned,
            price_high_aligned,
            IncreaseLiquidityParam::TokenB(dep_b_atoms.max(1)),
            None,
            Some(wallet_pk),
        )
        .await
        .map_err(|e| anyhow!("first quote failed: {e}"))?;

        IncreaseLiquidityParam::Liquidity(q.liquidity_delta.max(1))
    };

    // ───────── 6. Финальный набор инструкций ──────────────────────────────
    let slippage_bps = if matches!(liquidity_param, IncreaseLiquidityParam::Liquidity(_)) {
        slippage.max(200)
    } else {
        slippage
    };
    println!("DEBUG: using slippage_bps = {}", slippage_bps);
    let OpenPositionInstruction {
        position_mint,
        quote: IncreaseLiquidityQuote { token_max_a, token_max_b, .. },
        initialization_cost,            // ИЗМЕНЕНО: теперь захватываем стоимость инициализации
        instructions,
        additional_signers,
        ..
    } = open_position_instructions(
            &rpc,
            whirl_pk,
            price_low_aligned,
            price_high_aligned,
            liquidity_param.clone(),
            Some(slippage_bps),
            Some(wallet_pk),
        )
        .await
        .map_err(|e| anyhow!("open_position_instructions failed: {e}"))?;
    println!(
        "DEBUG: open_position_instructions returned — \
         position_mint = {}, token_max_a = {}, token_max_b = {}, initialization_cost = {}, \
         instructions_count = {}, additional_signers_count = {}",
        position_mint, token_max_a, token_max_b, initialization_cost,
        instructions.len(), additional_signers.len()
    );

    // ───────── 7. Сколько нужно токенов по факту ──────────────────────────
    let need_sol  = (token_max_a as f64 / 10f64.powi(dec_a as i32)) * OVR;
    let need_tokb = (token_max_b as f64 / 10f64.powi(dec_b as i32)) * OVR;
    println!(
        "DEBUG: need_sol = {:.6}, need_tokb = {:.6}",
        need_sol, need_tokb
    );

    const RESERVE_LAMPORTS: u64 = 120_000_000;
    
    let lamports_full = rpc.get_balance(&wallet_pk).await?;

    // sol_free — сколько мы позволим потратить именно на сам депозит
    let mut sol_free = ((lamports_full.saturating_sub(RESERVE_LAMPORTS)) as f64) / 1e9;
    
    let ata_b = get_associated_token_address(&wallet_pk, &Pubkey::from_str(&pool.mint_b)?);
    let mut tokb_free = rpc.get_token_account_balance(&ata_b).await
        .ok()
        .and_then(|r| r.amount.parse::<u64>().ok())
        .map(|a| a as f64 / 10f64.powi(dec_b as i32))
        .unwrap_or(0.0);
    println!(
        "DEBUG: initial sol_free = {:.6}, tokb_free = {:.6}",
        sol_free, tokb_free
    );
 
    rebalance_before_open(
        &rpc,
        &wallet_pk,
        &pool,
        need_sol,
        need_tokb,
        price_a_in_b,
        gap_b,
        dec_b,
        &mut sol_free,
        &mut tokb_free,
    )
    .await?;

    // ───────── 11. Отправляем транзакцию ──────────────────────────────────
    let mut signers: Vec<&Keypair> = vec![&wallet];
    signers.extend(additional_signers.iter());
    let mut instr = instructions;
    let mut slip = slippage_bps;

    loop {
        // 11-A: получаем инструкции + новых сигнеров
        let OpenPositionInstruction {
            initialization_cost,
            quote: IncreaseLiquidityQuote { token_max_a, token_max_b, .. },
            instructions,
            additional_signers,
            ..
        } = open_position_instructions(
                &rpc,
                whirl_pk,
                price_low_aligned,
                price_high_aligned,
                liquidity_param.clone(),
                Some(slip),
                Some(wallet_pk),
            )
            .await
            .map_err(|e| anyhow!("open_position_instructions failed: {e}"))?;
    
        // 11-B: формируем fresh-signers для конкретного набора инструкций
        let mut signers: Vec<&Keypair> = vec![&wallet];
        signers.extend(additional_signers.iter());
    
        // 11-C: пробуем отправить
        match utils::send_and_confirm(rpc.clone(), instructions, &signers).await {
            Ok(_) => break, // успех
            Err(e) if is_token_max(&e) && slip < 1200 => {
                slip += 300;                    // пробуем ещё раз с большим slippage
                println!("Retry with slippage = {slip} bps");
                continue;
            }
            Err(e) => return Err(anyhow!("send_and_confirm failed: {e}")),
        }
    }

    drop(_wallet_guard);

    Ok(OpenPositionResult {
        position_mint,
        amount_wsol: need_sol,
        amount_usdc: need_tokb,      // поле переиспользуем даже для RAY / whETH
    })
}

fn is_token_max(e: &anyhow::Error) -> bool {
    e.to_string().contains("6017") || e.to_string().contains("0x1781")
}

pub async fn list_positions_for_owner(
    pool: Option<Pubkey>,
) -> Result<Vec<PositionOrBundle>> {
    
    let _wallet_guard = WALLET_MUTEX.lock().await;
    // 1) Инициализируем SDK для mainnet
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow!("SDK config failed: {}", e))?;

    // 2) RPC и владелец
    let rpc    = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner  = wallet.pubkey();
    // 3) Фетчим **все** позиции владельца
    let positions = fetch_positions_for_owner(&rpc, owner)
        .await
        .map_err(|e| anyhow!("fetch_positions_for_owner failed: {}", e))?;
    // 4) Если указан `pool`, фильтруем
    let filtered = if let Some(pool_address) = pool {
        positions
            .into_iter()
            .filter(|pos| match pos {
                // одиночная позиция — сравниваем поле `whirlpool` в данных
                PositionOrBundle::Position(hp) => hp.data.whirlpool == pool_address,
                // бандл — проверяем, есть ли в нём хотя бы одна позиция из нужного пула
                PositionOrBundle::PositionBundle(pb) => pb
                    .positions
                    .iter()
                    .any(|hpb: &HydratedBundledPosition| hpb.data.whirlpool == pool_address),
            })
            .collect()
    } else {
        // иначе — возвращаем без изменений
        positions
    };

    Ok(filtered)
}


pub async fn close_all_positions(slippage: u16, pool: Option<Pubkey>) -> Result<()> {

    triggers::closing_switcher(true, None).await?;
    // 1) Список всех позиций
    let positions = list_positions_for_owner(pool)
        .await
        .with_context(|| "Failed to fetch positions for owner")?;

    let total = positions.len();
    log::debug!("Found {} positions for owner", total);

    if total == 0 {
        log::debug!("У вас нет открытых позиций.");
        return Ok(());
    }

    // ИЗМЕНЕНО: вектор для тех, что не удалось закрыть в первом проходе
    let mut failed_mints: Vec<Pubkey> = Vec::new();         // ИЗМЕНЕНО

    // ──────── ПЕРВЫЙ ПРОХОД ────────────────────────────────────────
    for (idx, p) in positions.into_iter().enumerate() {
        let slot = idx + 1;
        if let PositionOrBundle::Position(hp) = p {
            let mint = hp.data.position_mint;
            log::debug!("Closing {}/{} mint={}", slot, total, mint);

            // ИЗМЕНЕНО: не возвращаем Err, а запоминаем неудачи
            if let Err(err) = close_whirlpool_position(mint, slippage).await {
                triggers::closing_switcher(false, None).await?;
                log::error!("❌ First-pass failed mint={} err={:?}", mint, err);  // ИЗМЕНЕНО
                failed_mints.push(mint);                                         // ИЗМЕНЕНО
            } else {
                log::debug!("✅ Closed mint={} in first pass", mint);
            }

            // пауза между транзакциями
            sleep(Duration::from_millis(500)).await;
        } else {
            // bundles пропускаем, как раньше
            if let PositionOrBundle::PositionBundle(pb) = p {
                log::warn!(
                    "Skipping bundle {} with {} inner positions",
                    pb.address,
                    pb.positions.len()
                );
            }
        }
    }



    // Если все закрылись с первого раза — выходим
    if failed_mints.is_empty() {
        history::record_session_history().await?;
        positions::delete_pool_config().await?;
        log::debug!("🎉 All positions closed in first pass.");
        return Ok(());
    }

    // ──────── ВТОРОЙ ПРОХОД ────────────────────────────────────────
    log::debug!(
        "Retrying {} failed positions with doubled slippage = {}",
        failed_mints.len(),
        slippage * 2
    );

    // Перечитываем актуальный список — возьмём только те, что остались открыты
    let positions2 = list_positions_for_owner(pool)
        .await
        .with_context(|| "Failed to fetch positions for retry")?;

    let mut remaining: Vec<Pubkey> = Vec::new();              // ИЗМЕНЕНО
    for p in positions2.into_iter() {
        if let PositionOrBundle::Position(hp) = p {
            let mint = hp.data.position_mint;
            if failed_mints.contains(&mint) {
                remaining.push(mint);                         // ИЗМЕНЕНО
            }
        }
    }

    // Если между проходами кто-то закрылся «сам», — поздравляем
    if remaining.is_empty() {
        history::record_session_history().await?;
        positions::delete_pool_config().await?;
        log::debug!("🎉 All failed positions closed by external factors.");
        return Ok(());
    }

    // Второй проход: increased slippage
    let retry_slippage = slippage.saturating_mul(2);         // ИЗМЕНЕНО
    for (i, mint) in remaining.iter().enumerate() {
        log::debug!("Retrying close {}/{} mint={} slip={}", i+1, remaining.len(), mint, retry_slippage);

        if let Err(err) = close_whirlpool_position(*mint, retry_slippage).await {
            log::error!("❌ Second-pass failed mint={} err={:?}", mint, err); // ИЗМЕНЕНО
        } else {
            log::debug!("✅ Closed mint={} in second pass", mint);
        }

        sleep(Duration::from_millis(500)).await;              // ИЗМЕНЕНО: сохраняем ту же паузу
    }

    log::debug!("🎉 Done attempts to close all positions (with retry).");
    history::record_session_history().await?;
    positions::delete_pool_config().await?;
    triggers::closing_switcher(false, None).await?;

    Ok(())
}

pub fn nearest_valid_ticks(
    price_low:  f64,
    price_high: f64,
    tick_spacing: i32,
    dec_a: u8,
    dec_b: u8,
) -> (i32, i32) {
    // округлитель
    fn align_tick(idx: i32, spacing: i32, round_up: bool) -> i32 {
        let rem = idx.rem_euclid(spacing);
        if rem == 0 {
            idx
        } else if round_up {
            idx + (spacing - rem)
        } else {
            idx - rem
        }
    }

    let raw_l = price_to_tick_index(price_low,  dec_a, dec_b);
    let raw_u = price_to_tick_index(price_high, dec_a, dec_b);

    let mut tick_l = align_tick(raw_l, tick_spacing, false); // вниз
    let mut tick_u = align_tick(raw_u, tick_spacing,  true); // вверх

    if tick_l == tick_u {
        tick_u += tick_spacing; // гарантируем Δtick > 0
    }
    (tick_l, tick_u)
}


pub async fn refresh_balances(
    rpc: &RpcClient,
    wallet: &Pubkey,
    token_b: &Pubkey,        // USDC / RAY / …
    dec_b: u8,
) -> Result<(f64/*SOL*/, f64/*B*/)> {
    let lamports = rpc.get_balance(wallet).await?;
    let sol = lamports as f64 / 1e9;

    let ata_b = get_associated_token_address(wallet, token_b);
    let tok_b = rpc
        .get_token_account_balance(&ata_b)
        .await
        .ok()
        .and_then(|r| r.amount.parse::<u64>().ok())
        .unwrap_or(0) as f64 / 10f64.powi(dec_b as i32);

    Ok((sol, tok_b))
}

#[allow(clippy::too_many_arguments)]
pub async fn rebalance_before_open(
    rpc:            &solana_client::nonblocking::rpc_client::RpcClient,
    wallet_pk:      &Pubkey,
    pool:           &PoolConfig,
    need_sol:       f64,
    need_tokb:      f64,
    price_a_in_b:   f64,
    gap_b:          f64,
    dec_b:          u8,
    sol_free:       &mut f64,
    tokb_free:      &mut f64,
) -> Result<()> {
    let mut round = 0;
    while (need_sol  - *sol_free  > GAP_SOL) ||
          (need_tokb - *tokb_free > gap_b)
    {
        round += 1;
        if round > 2 {        // страховка от вечного цикла
            println!("DEBUG: reached max rounds, breaking");
            break;
        }

        let mut changed = false;

        // ── a)  докупаем SOL -------------------------------------------------
        if need_sol - *sol_free > GAP_SOL {
            let miss   = need_sol - *sol_free;
            let cost_b = miss * price_a_in_b;

            if *tokb_free - cost_b >= need_tokb + gap_b {
                // меняем токен-B → SOL
                execute_swap_tokens(&pool.mint_b, &pool.mint_a, cost_b * OVR).await?;
                changed = true;
            } else {
                // меняем USDC → SOL
                let sol_usd  = get_sol_price_usd(WSOL, true).await?;
                let usdc_need = miss * sol_usd * OVR;
                execute_swap_tokens(USDC, &pool.mint_a, usdc_need).await?;
                changed = true;
            }
        }

        // ── b)  докупаем token-B -------------------------------------------
        if need_tokb - *tokb_free > gap_b {
            let miss     = need_tokb - *tokb_free;
            let cost_sol = miss / price_a_in_b;

            if *sol_free - cost_sol >= need_sol + GAP_SOL {
                // меняем SOL → B
                execute_swap_tokens(&pool.mint_a, &pool.mint_b, cost_sol * OVR).await?;
                changed = true;
            } else if pool.mint_b != USDC {
                // меняем USDC → B
                let sol_usd  = get_sol_price_usd(WSOL, true).await?;
                let b_usd    = (1.0 / price_a_in_b) * sol_usd;
                let usdc_need = miss * b_usd * OVR;
                execute_swap_tokens(USDC, &pool.mint_b, usdc_need).await?;
                changed = true;
            }
        }

        // ── c)  обновляем свободные остатки ----------------------------------
        if changed {
            let (s_now, b_now) = refresh_balances(
                rpc,
                wallet_pk,
                &Pubkey::from_str(&pool.mint_b)?,
                dec_b
            )
            .await?;
            *sol_free  = s_now;
            *tokb_free = b_now;
        } else {
            break;
        }
    }

    // финальная проверка: хватает ли теперь
    if need_sol  - *sol_free  > GAP_SOL {
        bail!("SOL всё ещё не хватает: need {:.6}, free {:.6}", need_sol,  sol_free);
    }
    if need_tokb - *tokb_free > gap_b   {
        bail!("Token-B всё ещё не хватает: need {:.6}, free {:.6}", need_tokb, tokb_free);
    }

    Ok(())
}



pub async fn decrease_liquidity_partial(
    position_mint: Pubkey,
    pct:           f64,   // процент 0< pct ≤100
    base_slip:     u16,   // первый slippage
) -> anyhow::Result<()> {
    use orca_whirlpools::{decrease_liquidity_instructions, DecreaseLiquidityParam};
    use orca_whirlpools_client::Position;
    use orca_whirlpools_core::{U128, sqrt_price_to_price};
    use solana_sdk::pubkey::Pubkey;

    // 1) Sanity-check
    if !(0.0 < pct && pct <= 100.0) {
        anyhow::bail!("pct must be within (0;100]");
    }
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow::anyhow!("SDK config failed: {}", e))?;

    let rpc    = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner  = wallet.pubkey();

    // 2) Собираем on-chain данные позиции
    let (pos_addr, _) = Pubkey::find_program_address(
        &[b"position", position_mint.as_ref()],
        &ID,
    );
    let pos_acc = rpc.get_account(&pos_addr).await?;
    let pos     = Position::from_bytes(&pos_acc.data)?;
    let total_liq: u128 = pos.liquidity.into();
    if total_liq == 0 {
        anyhow::bail!("Position has zero liquidity");
    }

    // 3) Считаем, какой объём ликвидности надо снять
    let mut delta = ((total_liq as f64) * pct / 100.0).round() as u64;
    if delta == 0 { delta = 1; }

    // 4) Эскалируем slippage, на underflow — делим delta пополам
    const SLIPS: &[u16] = &[150, 500, 1_200];
    loop {
        for &slip in std::iter::once(&base_slip).chain(SLIPS.iter()) {
            let instrs = match decrease_liquidity_instructions(
                &rpc,
                position_mint,
                DecreaseLiquidityParam::Liquidity(delta as u128),
                Some(slip),
                Some(owner),
            )
            .await
            {
                Ok(i) => i,
                Err(e) => {
                    if e.to_string().contains("Amount exceeds max u64") {
                        // слишком большой delta — сразу уменьшаем
                        break;
                    }
                    // иначе пробуем следующий slip
                    continue;
                }
            };

            // пытаемся отправить
            let mut signers = vec![&wallet];
            signers.extend(instrs.additional_signers.iter());
            match utils::send_and_confirm(rpc.clone(), instrs.instructions, &signers).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let s = e.to_string();
                    if s.contains("0x177f") /* LiquidityUnderflow */ {
                        // уменьшить delta и начать сначала
                        break;
                    }
                    if s.contains("TokenMinSubceeded") {
                        // попробовать увеличить slippage
                        continue;
                    }
                    return Err(anyhow::anyhow!(s));
                }
            }
        }
        // делим delta, пока не дойдём до 0
        delta /= 2;
        if delta == 0 {
            break;
        }
    }

    Err(anyhow::anyhow!(
        "Could not decrease liquidity – every attempt failed"
    ))
}




/// Добавляет ликвидность в существующую позицию, оперируя **только** целевым
/// бюджетом в USDC.  Функция сама вычисляет, сколько SOL (Token A) и USDC
/// (Token B) нужно внести — в зависимости от того, находится ли цена пары
/// выше, ниже или внутри ценового диапазона позиции.
///
/// * **usd_budget** — сколько долларов пользователь хочет завести в позицию;  
/// * если позиция выше рынка — весь бюджет конвертируется в SOL;  
/// * если позиция ниже рынка — весь бюджет остаётся USDC;  
/// * если цена попадает внутрь диапазона — бюджет делится между SOL и USDC
///   пропорционально теоретической формуле Uniswap v3:
///   ```text
///   ΔA = L · (√P − √P_L) / (√P · √P_L)
///   ΔB = L · (√P_U − √P)
///   ```
///   где  L  выбирается так, чтобы общая стоимость (в USDC) равнялась
///   `usd_budget`.
#[allow(clippy::too_many_lines)]
pub async fn increase_liquidity_partial(
    position_mint: Pubkey,
    usd_budget:    f64,           // ← сумма, которой распоряжаемся (в USDC)
    pool:          &PoolConfig,
    base_slip:     u16,
) -> anyhow::Result<()> {
    // ─────────────────────────────────────────────────────────────────────
    const BUFFER: f64       = 1.12;   // 12 % запас на ценовые колебания
    const OVR:    f64       = 1.015;  // +1.5 % к объёму свапов
    const WSOL_MINT: &str   = WSOL;   // из params
    const SOL_RESERVE: f64  = 0.07;   // минимум SOL, который нельзя трогать
    // ─────────────────────────────────────────────────────────────────────

    // 1. Инициализация SDK / RPC / кошелька ──────────────────────────────
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow!("SDK config failed: {e}"))?;

    let native_mint = Pubkey::from_str(WSOL_MINT)?;
    let rpc         = utils::init_rpc();
    let wallet      = utils::load_wallet()?;
    let wallet_pk   = wallet.pubkey();
    let owner       = wallet.pubkey();

    let dec_a = pool.decimal_a as u8;     // 9 для SOL
    let dec_b = pool.decimal_b as u8;     // 6 для USDC

    // 2. Читаем позицию и пул ────────────────────────────────────────────
    let (pos_addr, _) = Pubkey::find_program_address(&[b"position", position_mint.as_ref()], &ID);
    let pos_acc   = rpc.get_account(&pos_addr).await?;
    let pos       = Position::from_bytes(&pos_acc.data)?;
    let whirl_acc = rpc.get_account(&pos.whirlpool).await?;
    let whirl     = Whirlpool::from_bytes(&whirl_acc.data)?;

    // 3. Текущее состояние цен и режим позиции ───────────────────────────
    let tick_c  = sqrt_price_to_tick_index(U128::from(whirl.sqrt_price));
    let price_c = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b); // USDC за 1 SOL
    let price_l = tick_index_to_price(pos.tick_lower_index, dec_a, dec_b);
    let price_u = tick_index_to_price(pos.tick_upper_index, dec_a, dec_b);

    let mode = position_mode(
        tick_c,
        pos.tick_lower_index,
        pos.tick_upper_index,
        price_c,
        price_l,
        price_u,
    );
    println!("→ целевая позиция: {:?}, spot = {:.4}", mode, price_c);

    // 4. Рассчитываем требуемые ΔA и ΔB (+12 % буфер) ────────────────────
    let (mut add_a, mut add_b) = match mode {
        Mode::OnlyA => ((usd_budget / price_c) * BUFFER, 0.0),

        Mode::OnlyB => (0.0, usd_budget * BUFFER),

        Mode::Mixed => {
            // формулы Uniswap v3
            let (sqrt_pl, sqrt_pu, sqrt_p) = (price_l.sqrt(), price_u.sqrt(), price_c.sqrt());

            let per_l_usd =
                  ((sqrt_p - sqrt_pl) / (sqrt_p * sqrt_pl) * price_c)
                +  (sqrt_pu - sqrt_p);

            if per_l_usd <= 0.0 {
                return Err(anyhow!("Некорректный per_liquidity_usd = {per_l_usd}"));
            }
            let liquidity = usd_budget / per_l_usd;

            let delta_sol  = liquidity * (sqrt_pu - sqrt_p) / (sqrt_pu * sqrt_p); // SOL-часть
            let delta_usdc = liquidity * (sqrt_p  - sqrt_pl);                     // USDC-часть
            (delta_sol * BUFFER, delta_usdc * BUFFER)
        }
    };

    println!(
        "⮑ расчёт: add_a = {:.6} SOL, add_b = {:.2} USDC (budget = {:.2} USD)",
        add_a, add_b, usd_budget
    );

    // 5. Гарантируем, что ATA для wSOL существует ────────────────────────
    let ata_a = get_associated_token_address(&wallet_pk, &native_mint);
    if rpc.get_account(&ata_a).await.is_err() {
        let ix = create_associated_token_account(
            &wallet_pk, &wallet_pk, &native_mint, &spl_token::id(),
        );
        utils::send_and_confirm(rpc.clone(), vec![ix], &[&wallet]).await?;
    }

    // 6. Балансировка кошелька под рассчитанные add_a / add_b ────────────
    match mode {
        // ─── позиция «только SOL» ───────────────────────────────────────
        Mode::OnlyA => {
            if add_a > 1e-9 {
                execute_swap_tokens(USDC, WSOL, usd_budget * OVR).await?;
            }
            add_b = 0.0;
        }

        // ─── позиция «только USDC» – новая логика с резервом SOL ────────
        Mode::OnlyB => {
            // текущее содержимое «кошелька SOL»
            let wallet_sol  = rpc.get_balance(&wallet_pk).await? as f64 / 1e9;
            let wsol_on_ata = rpc
                .get_token_account_balance(&ata_a).await.ok()
                .and_then(|ui| ui.amount.parse::<u64>().ok())
                .map(|v| v as f64 / 1e9)
                .unwrap_or(0.0);                                     // ★ NEW
            let have_sol_tot = wallet_sol + wsol_on_ata;             // ★ NEW
        
            // текущее содержимое «кошелька USDC»
            let ata_b   = get_associated_token_address(
                            &wallet_pk, &Pubkey::from_str(USDC)?);
            let have_usdc = rpc.get_token_account_balance(&ata_b).await.ok()
                .and_then(|ui| ui.amount.parse::<u64>().ok())
                .map(|v| v as f64 / 10f64.powi(dec_b as i32))
                .unwrap_or(0.0);
        
            // сколько USDC ещё нужно?
            let need_usdc = (add_b - have_usdc).max(0.0);
            if need_usdc > 1e-6 {
                let sol_to_swap = need_usdc / price_c * OVR;
        
                // сначала пробуем продать то, что уже лежит в WSOL
                let sell_from_wsol = sol_to_swap.min(wsol_on_ata);   // ★ NEW
                if sell_from_wsol > 1e-9 {
                    execute_swap_tokens(WSOL, USDC, sell_from_wsol).await?;
                }
        
                // если ещё не хватило — докидываем лампорты из кошелька
                let remaining = sol_to_swap - sell_from_wsol;        // ★ NEW
                if remaining > 1e-9 {
                    let free_lamports = wallet_sol - SOL_RESERVE;
                    if remaining > free_lamports + 1e-9 {
                        bail!("Недостаточно свободного SOL для обмена (нужно {:.3}, есть {:.3})",
                              remaining, free_lamports);
                    }
                    execute_swap_tokens(WSOL, USDC, remaining).await?;
                }
            }
        
            add_a = 0.0;           // в позицию всё равно не кладём SOL
        }

        // ─── позиция «смешанная» – прежняя логика ────────────────────────
        Mode::Mixed => {
            // проверяем текущие кошельковые запасы
            let have_a = rpc.get_balance(&wallet_pk).await? as f64 / 1e9;
            let ata_b  = get_associated_token_address(&wallet_pk, &Pubkey::from_str(USDC)?);
            let have_b_atoms = rpc
                .get_token_account_balance(&ata_b)
                .await
                .ok()
                .and_then(|ui| ui.amount.parse::<u64>().ok())
                .unwrap_or(0);
            let have_b = have_b_atoms as f64 / 10f64.powi(dec_b as i32);

            if add_a > have_a + 1e-9 {
                let need_sol = add_a - have_a;
                execute_swap_tokens(USDC, WSOL, need_sol * price_c * OVR).await?;
            }
            if add_b > have_b + 1e-6 {
                let need_usdc = add_b - have_b;
                execute_swap_tokens(WSOL, USDC, need_usdc / price_c * OVR).await?;
            }
        }
    }

    let ata_a = get_associated_token_address(&wallet_pk, &native_mint);
    if rpc.get_account(&ata_a).await.is_err() {
        let ix = create_associated_token_account(
            &wallet_pk, &wallet_pk, &native_mint, &spl_token::id(),
        );
        utils::send_and_confirm(rpc.clone(), vec![ix], &[&wallet]).await?;
    }

    // ─── 7. Формируем IncreaseLiquidityParam ────────────────────────────────
    let (param, final_quote) = match mode {
        // ─────────────────────────────────────────────────────────
        Mode::OnlyA => {
            let lamports = (add_a * 10f64.powi(dec_a as i32)).ceil() as u64;
            let q = increase_liquidity_instructions(
                &rpc,
                position_mint,
                IncreaseLiquidityParam::TokenA(lamports.max(1)),
                Some(base_slip),
                Some(owner),
            ).await
            .map_err(|e| anyhow!("increase_liquidity_instructions failed: {e}"))?;
            (IncreaseLiquidityParam::TokenA(lamports.max(1)), q)
        }
        // ─────────────────────────────────────────────────────────
        Mode::OnlyB => {
            let atoms_b = (add_b * 10f64.powi(dec_b as i32)).ceil() as u64;
            let q = increase_liquidity_instructions(
                &rpc,
                position_mint,
                IncreaseLiquidityParam::TokenB(atoms_b.max(1)),
                Some(base_slip),
                Some(owner),
            ).await
            .map_err(|e| anyhow!("increase_liquidity_instructions failed: {e}"))?;
            (IncreaseLiquidityParam::TokenB(atoms_b.max(1)), q)
        }
        // ─────────────────────────────────────────────────────────
        Mode::Mixed => {
            // 1) USDC-бюджет сразу переводим в атомы
            let atoms_b_budget = (add_b * 10f64.powi(dec_b as i32)).ceil() as u64;
        
            // 2) Первый (полный) quote по бюджету токена-B
            let full_quote = increase_liquidity_instructions(
                &rpc,
                position_mint,
                IncreaseLiquidityParam::TokenB(atoms_b_budget.max(1)),
                Some(base_slip),
                Some(owner),
            )
            .await
            .map_err(|e| anyhow!("full quote failed: {e}"))?;
        
            // 3) Сколько это $ стоит на самом деле?
            let want_sol  = full_quote.quote.token_est_a as f64 / 10f64.powi(dec_a as i32);
            let want_usdc = full_quote.quote.token_est_b as f64 / 10f64.powi(dec_b as i32);
            let want_usd  = want_usdc + want_sol * price_c;
        
            // 4) При необходимости уменьшаем liquidity, чтобы уложиться в BUFFER-бюджет
            let (final_quote, liquidity_delta) = if want_usd > usd_budget * BUFFER {
                // нужен «шринк»
                let ratio = (usd_budget * BUFFER) / want_usd;
                let liq   = ((full_quote.quote.liquidity_delta as f64) * ratio * 0.995).floor() as u128;
        
                let q = increase_liquidity_instructions(
                    &rpc,
                    position_mint,
                    IncreaseLiquidityParam::Liquidity(liq.max(1)),
                    Some(base_slip),
                    Some(owner),
                )
                .await
                .map_err(|e| anyhow!("scaled quote failed: {e}"))?;
        
                (q, liq)
            } else {
                // полный quote целиком укладывается в лимит
                let liq = full_quote.quote.liquidity_delta;
                (full_quote, liq)
            };
        
            // 5) Докупаем недостающее *по фактическим* token_max из final_quote
            let need_sol  = final_quote.quote.token_max_a as f64 / 10f64.powi(dec_a as i32);
            let need_usdc = final_quote.quote.token_max_b as f64 / 10f64.powi(dec_b as i32);
        
            // — текущие остатки —
            let wallet_sol = rpc.get_balance(&wallet_pk).await? as f64 / 1e9;
            let wsol_on_ata = rpc.get_token_account_balance(&ata_a).await.ok()
                             .and_then(|ui| ui.amount.parse::<u64>().ok())
                             .map(|v| v as f64 / 1e9)
                             .unwrap_or(0.0);
            let have_sol_total = wallet_sol + wsol_on_ata;
        
            let ata_b = get_associated_token_address(&wallet_pk, &Pubkey::from_str(USDC)?);
            let have_usdc = rpc.get_token_account_balance(&ata_b).await.ok()
                .and_then(|ui| ui.amount.parse::<u64>().ok())
                .map(|v| v as f64 / 10f64.powi(dec_b as i32))
                .unwrap_or(0.0);
        
            if need_sol  > have_sol_total + 1e-9 {
                execute_swap_tokens(
                    USDC, WSOL,
                    (need_sol - have_sol_total) * price_c * OVR
                ).await?;
            }
            if need_usdc > have_usdc + 1e-6 {
                execute_swap_tokens(
                    WSOL, USDC,
                    (need_usdc - have_usdc) / price_c * OVR
                ).await?;
            }
        
            // 6) Готовые param / quote для продолжения внешней логики
            (
                IncreaseLiquidityParam::Liquidity(liquidity_delta.max(1)),
                final_quote
            )
        }
    };

    // ─── 7-bis.  Досвап при нехватке (только после final_quote) ─────────────
    if let Mode::Mixed = mode {
        let need_sol  = final_quote.quote.token_max_a as f64 / 10f64.powi(dec_a as i32);
        let need_usdc = final_quote.quote.token_max_b as f64 / 10f64.powi(dec_b as i32);
    
        let have_sol  = rpc.get_balance(&wallet_pk).await? as f64 / 10f64.powi(dec_a as i32);
        let ata_b     = get_associated_token_address(&wallet_pk, &Pubkey::from_str(USDC)?);
        let have_usdc = rpc.get_token_account_balance(&ata_b).await.ok()
            .and_then(|ui| ui.amount.parse::<u64>().ok())
            .map(|v| v as f64 / 10f64.powi(dec_b as i32))
            .unwrap_or(0.0);
    
        if need_sol  > have_sol  + 1e-9 {
            execute_swap_tokens(USDC, WSOL,
                (need_sol  - have_sol ) * price_c * OVR).await?;
        }
        if need_usdc > have_usdc + 1e-6 {
            execute_swap_tokens(WSOL, USDC,
                (need_usdc - have_usdc) / price_c * OVR).await?;
        }
    }


    // 8. Пытаемся добавить ликвидность, расширяя слиппедж ────────────────
    const SLIPS: &[u16] = &[150, 500, 1_200];
    for &slip in std::iter::once(&base_slip).chain(SLIPS.iter()) {
        let ix = increase_liquidity_instructions(
            &rpc,
            position_mint,
            param.clone(),
            Some(slip),
            Some(owner),
        )
        .await
        .map_err(|e| anyhow!("increase_liquidity_instructions failed: {e}"))?;

        let mut signers = vec![&wallet];
        signers.extend(ix.additional_signers.iter());

        match utils::send_and_confirm(rpc.clone(), ix.instructions, &signers).await {
            Ok(_) => {
                println!("✓ ликвидность добавлена (slip = {slip} bps)");
                return Ok(());
            }
            Err(e)
                if  e.to_string().contains("TokenMinSubceeded")
                 || e.to_string().contains("TokenMaxExceeded")
                 || e.to_string().contains("custom program error: 0x1781") =>
            {
                // увеличиваем slippage и повторяем
                continue;
            }
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Err(anyhow!("increase_liquidity_partial: все попытки неудачны"))
}



pub fn position_mode(
    tick_c: i32, tick_l: i32, tick_u: i32,
    price_c: f64, price_l: f64, price_u: f64
) -> Mode {
    // 1. Сначала попробуем по тикам:
    if tick_u > tick_l {
        if tick_c < tick_l      { return Mode::OnlyA }
        if tick_c > tick_u      { return Mode::OnlyB }
        return Mode::Mixed;
    }

    // 2. Если tick_u == tick_l — fallback на цены:
    if price_c <= price_l    { Mode::OnlyA }
    else if price_c >= price_u{ Mode::OnlyB }
    else                      { Mode::Mixed }
}

