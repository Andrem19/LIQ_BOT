use anyhow::{anyhow, Result};
use std::{str::FromStr, sync::Arc, time::Duration};

use solana_client::{
    nonblocking::rpc_client::RpcClient,
};
use anyhow::Context;
use tokio::time::sleep;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use anyhow::bail;
use crate::orca_logic::helpers::get_sol_price_usd;
use orca_whirlpools::{
    fetch_positions_for_owner,
    PositionOrBundle,
};
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


const GAP_SOL:  f64 = 0.002;
const GAP_B: f64 = 0.005;

/// Закрывает позицию целиком (сбор комиссий + вывод ликвидности + close).
pub async fn close_whirlpool_position(
    position_mint: Pubkey,
    slippage: u16,
) -> anyhow::Result<()> {
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
    .map_err(op("set_whirlpools_config_address"))?;
    let rpc     = utils::init_rpc();
    let wallet  = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    // единый комплект Collect + Decrease + Close
    let ClosePositionInstruction { instructions, additional_signers, .. } =
        close_position_instructions(&rpc, position_mint, Some(slippage), Some(wallet_pk)).await
        .map_err(op("close_position_instructions"))?;

    let mut signers: Vec<&Keypair> = vec![&wallet];
    signers.extend(additional_signers.iter());
    utils::send_and_confirm(rpc, instructions, &signers).await?;
    Ok(())
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
    let whirl_pk = Pubkey::from_str(pool.pool_address)?;
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

    // ───────── 2. Пул, децималы, tick-spacing ──────────────────────────────
    let whirl_pk  = Pubkey::from_str(pool.pool_address)?;

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
         position_mint = {}, token_max_a = {}, token_max_b = {}, \
         instructions_count = {}, additional_signers_count = {}",
        position_mint, token_max_a, token_max_b,
        instructions.len(), additional_signers.len()
    );

    // ───────── 7. Сколько нужно токенов по факту ──────────────────────────
    let need_sol  = (token_max_a as f64 / 10f64.powi(dec_a as i32)) * OVR;
    let need_tokb = (token_max_b as f64 / 10f64.powi(dec_b as i32)) * OVR;
    println!(
        "DEBUG: need_sol = {:.6}, need_tokb = {:.6}",
        need_sol, need_tokb
    );

    const RESERVE_LAMPORTS: u64 = 50_000_000;        // ≈ 0.050 SOL
    const RESERVE_SOL: f64     = RESERVE_LAMPORTS as f64 / 1e9;
    
    let mut sol_free = {
        let lam = rpc.get_balance(&wallet_pk).await?;
        ((lam.saturating_sub(RESERVE_LAMPORTS)) as f64) / 1e9
    }.max(0.0);
    
    let ata_b = get_associated_token_address(&wallet_pk, &Pubkey::from_str(pool.mint_b)?);
    let mut tokb_free = rpc.get_token_account_balance(&ata_b).await
        .ok()
        .and_then(|r| r.amount.parse::<u64>().ok())
        .map(|a| a as f64 / 10f64.powi(dec_b as i32))
        .unwrap_or(0.0);
    println!(
        "DEBUG: initial sol_free = {:.6}, tokb_free = {:.6}",
        sol_free, tokb_free
    );
    
    //---------------------------------------------------------------------
    // 9. Пополняем дефицит, не трогая резерв
    //---------------------------------------------------------------------
    let mut round = 0;
    while (need_sol  - sol_free  > GAP_SOL) ||
          (need_tokb - tokb_free > gap_b)
    {
        round += 1;
        println!("DEBUG: === round {} ===", round);
        if round > 2 { 
            println!("DEBUG: reached max rounds, breaking");
            break 
        }                          // страховка от цикла
        let mut changed = false;
    
        // ── a)  SOL
        if need_sol - sol_free > GAP_SOL {
            let miss = need_sol - sol_free;
            let cost_b = miss * price_a_in_b;
            println!(
                "DEBUG: a) miss_sol = {:.6}, price_a_in_b = {:.6}, cost_b = {:.6}, tokb_free = {:.6}",
                miss, price_a_in_b, cost_b, tokb_free
            );
    
            if tokb_free - cost_b >= need_tokb + gap_b {
                println!("DEBUG: executing swap B→SOL amount = {:.6}", cost_b * OVR);
                execute_swap_tokens(pool.mint_b, pool.mint_a, cost_b * OVR).await?;
                changed = true;
            } else {
                let sol_usd  = get_sol_price_usd().await?;
                let usdc_need = miss * sol_usd * OVR;
                println!("DEBUG: executing swap USDC→SOL amount = {:.6}", usdc_need);
                execute_swap_tokens(USDC, pool.mint_a, usdc_need).await?;
                changed = true;
            }
        }
    
        // ── b)  token-B
        if need_tokb - tokb_free > gap_b {
            let miss = need_tokb - tokb_free;
            let cost_sol = miss / price_a_in_b;
            println!(
                "DEBUG: b) miss_tokb = {:.6}, price_a_in_b = {:.6}, cost_sol = {:.6}, sol_free = {:.6}",
                miss, price_a_in_b, cost_sol, sol_free
            );
    
            if sol_free - cost_sol >= need_sol + GAP_SOL {
                println!("DEBUG: executing swap SOL→B amount = {:.6}", cost_sol * OVR);
                execute_swap_tokens(pool.mint_a, pool.mint_b, cost_sol * OVR).await?;
                changed = true;
            } else if pool.mint_b != USDC {
                let sol_usd  = get_sol_price_usd().await?;
                let b_usd    = (1.0 / price_a_in_b) * sol_usd;
                let usdc_need = miss * b_usd * OVR;
                println!(
                    "DEBUG: executing swap USDC→B amount = {:.6}",
                    usdc_need
                );
                execute_swap_tokens(USDC, pool.mint_b, usdc_need).await?;
                changed = true;
            }
        }
    
        // ── c) обновляем
        if changed {
            let (s_now, b_now) = refresh_balances(
                &rpc, &wallet_pk, &Pubkey::from_str(pool.mint_b)?, dec_b
            ).await?;
            sol_free = (s_now - RESERVE_SOL).max(0.0);
            tokb_free = b_now;
            println!(
                "DEBUG: updated sol_free = {:.6}, tokb_free = {:.6}",
                sol_free, tokb_free
            );
        } else {
            break;
        }
    }
    
    //---------------------------------------------------------------------
    // 10. Финальный чек: должно хватать **с учётом резерва**
    //---------------------------------------------------------------------
    if need_sol  - sol_free  > GAP_SOL {
        bail!("SOL всё ещё не хватает: need {:.6}, free {:.6}", need_sol, sol_free);
    }
    if need_tokb - tokb_free > gap_b {
        bail!("Token-B всё ещё не хватает: need {:.6}, free {:.6}", need_tokb, tokb_free);
    }
    
        

    // ───────── 11. Отправляем транзакцию ──────────────────────────────────
    let mut signers: Vec<&Keypair> = vec![&wallet];
    signers.extend(additional_signers.iter());
    let mut instr = instructions;
    let mut slip = slippage_bps;

    loop {
        // 11-A: получаем инструкции + новых сигнеров
        let OpenPositionInstruction {
            position_mint,
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
    // let _wallet_guard = WALLET_MUTEX.lock().await;
    // 1) Получаем все позиции
    let positions = list_positions_for_owner(pool)
        .await
        .with_context(|| "Failed to fetch positions for owner")?;
    let total = positions.len();
    log::debug!("Found {} positions for owner", total);

    if total == 0 {
        log::debug!("У вас нет открытых позиций.");
        return Ok(());
    }

    // 2) Итерируем по каждой позиции
    for (idx, p) in positions.into_iter().enumerate() {
        let slot = idx + 1;
        match p {
            PositionOrBundle::Position(hp) => {
                // Собираем детали
                let account   = hp.address;
                let whirlpool = hp.data.whirlpool;
                let mint      = hp.data.position_mint;
                let liquidity = hp.data.liquidity;
                let lo        = hp.data.tick_lower_index;
                let hi        = hp.data.tick_upper_index;
                let fee_a     = hp.data.fee_owed_a;
                let fee_b     = hp.data.fee_owed_b;

                log::debug!(
                    "Closing {}/{}:\n\
                     → account:   {}\n\
                     → pool:      {}\n\
                     → mint:      {}\n\
                     → liquidity: {}\n\
                     → ticks:     [{} .. {}]\n\
                     → fees owed: A={}  B={}",
                    slot, total, account, whirlpool, mint, liquidity, lo, hi, fee_a, fee_b
                );

                // 3) Попытка закрыть позицию
                if let Err(err) = close_whirlpool_position(mint, slippage).await {
                    log::error!(
                        "❌ Error closing position {}/{} (mint={}): {:?}",
                        slot, total, mint, err
                    );
                    // возвращаем ошибку с контекстом
                    return Err(anyhow!("Failed at position {}/{} mint={}", slot, total, mint))
                        .with_context(|| format!("Underlying error: {:?}", err));
                }

                log::debug!("✅ Successfully closed position {}/{}", slot, total);
                // 4) Пауза между транзакциями
                sleep(Duration::from_millis(500)).await;
            }

            PositionOrBundle::PositionBundle(pb) => {
                log::warn!(
                    "Skipping bundled position {}/{} — bundle account {} contains {} inner positions",
                    slot, total,
                    pb.address,
                    pb.positions.len()
                );
            }
        }
    }

    log::debug!("🎉 All positions processed successfully.");
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


async fn refresh_balances(
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