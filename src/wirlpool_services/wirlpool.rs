use anyhow::{anyhow, Result};
use std::{env, str::FromStr, sync::Arc, time::Duration};

use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use anyhow::Context;
use tokio::time::sleep;
use spl_associated_token_account::get_associated_token_address;
use spl_token::solana_program::program_pack::Pack;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};
use orca_whirlpools::{
    fetch_positions_for_owner, 
    fetch_positions_in_whirlpool,
    PositionOrBundle,
};
use solana_sdk::system_instruction;
use spl_associated_token_account::instruction::create_associated_token_account;
use spl_token::instruction::sync_native;
use orca_whirlpools::OpenPositionInstruction;
use orca_whirlpools_core::IncreaseLiquidityQuote;
use crate::wirlpool_services::swap::SwapResult;
use spl_token::state::Mint;
use orca_whirlpools_client::{Whirlpool,Position, DecodedAccount};
use orca_whirlpools::{
    close_position_instructions, harvest_position_instructions, open_position_instructions,
    set_whirlpools_config_address, HarvestPositionInstruction, IncreaseLiquidityParam,
    WhirlpoolsConfigInput,
};
use orca_whirlpools_core::{CollectFeesQuote, U128, sqrt_price_to_price};
use crate::wirlpool_services::swap::execute_swap;
use crate::params;
use crate::types::{PoolConfig, OpenPositionResult};
use orca_whirlpools_client::ID;

/// Вспомогательная функция для единообразного маппинга ошибок.
fn op<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> anyhow::Error {
    move |e| anyhow!("{} failed: {}", ctx, e)
}

/// Общие утилиты: RPC, кошелек, отправка.
mod utils {
    use super::*;

    pub fn init_rpc() -> Arc<RpcClient> {
        let url = env::var("HELIUS_HTTP")
            .or_else(|_| env::var("QUICKNODE_HTTP"))
            .or_else(|_| env::var("ANKR_HTTP"))
            .or_else(|_| env::var("CHAINSTACK_HTTP"))
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        Arc::new(RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()))
    }

    pub fn load_wallet() -> Result<Keypair> {
        read_keypair_file(params::KEYPAIR_FILENAME)
            .map_err(op("load_wallet"))
    }

    pub async fn send_and_confirm(
        rpc: Arc<RpcClient>,
        mut instructions: Vec<Instruction>,
        signers: &[&Keypair],
    ) -> Result<()> {
        // 1. ComputeBudget
        instructions.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(400_000));

        // 2. Latest blockhash
        let recent = rpc
            .get_latest_blockhash()
            .await
            .map_err(op("get_latest_blockhash"))?;
        // 3. Build & sign
        let payer = signers
            .get(0)
            .ok_or_else(|| anyhow!("No payer signer"))?;
        let message = Message::new(&instructions, Some(&payer.pubkey()));
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(signers, recent).map_err(op("sign transaction"))?;
        // 4. Send
        let sig = rpc
            .send_transaction_with_config(
                &tx,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentConfig::processed().commitment),
                    ..Default::default()
                },
            )
            .await
            .map_err(op("send transaction"))?;
        // 5. Confirm
        for _ in 0..40 {
            if let Some(status) = rpc.get_signature_status(&sig).await.map_err(op("get_signature_status"))? {
                status.map_err(|e| anyhow!("transaction failed: {:?}", e))?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(anyhow!("Timeout: tx {} not confirmed within 40s", sig))
    }
}

/// Закрывает позицию; возвращает `()` при успехе.
pub async fn close_whirlpool_position(position_mint: Pubkey) -> Result<()> {
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(op("set_whirlpools_config_address"))?;
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    let quote = close_position_instructions(&rpc, position_mint, Some(100), Some(wallet_pk))
        .await
        .map_err(op("close_position_instructions"))?;

    let mut signers = Vec::with_capacity(1 + quote.additional_signers.len());
    signers.push(&wallet);
    for kp in &quote.additional_signers {
        signers.push(kp);
    }

    utils::send_and_confirm(rpc, quote.instructions, &signers)
        .await
        .map_err(op("send_and_confirm"))
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


pub async fn open_with_funds_check(
    price_low: f64,
    price_high: f64,
    initial_amount_usdc: f64,
    pool: PoolConfig,
) -> Result<OpenPositionResult> {
    // 1) Инициализация RPC, кошелька и SDK
    let rpc      = utils::init_rpc();
    let wallet   = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();
    log::debug!("debug: wallet = {}", wallet_pk);
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow!("SDK config failed: {}", e))?;
    log::debug!("debug: whirlpools sdk → Mainnet");

    // 2) Загружаем пул и считаем текущую цену + депозиты в атомах
    let whirl_pk = Pubkey::from_str(pool.pool_address)?;
    let acct     = rpc.get_account(&whirl_pk).await?;
    let whirl    = Whirlpool::from_bytes(&acct.data)?;

    let dec_a = pool.decimal_a as i32;
    let dec_b = pool.decimal_b as i32;

    let price_sol_in_usdc = sqrt_price_to_price(
        U128::from(whirl.sqrt_price),
        dec_a as u8,
        dec_b as u8,
    );
    if price_sol_in_usdc <= 0.0 {
        return Err(anyhow!("Invalid pool price: {}", price_sol_in_usdc));
    }
    log::debug!("debug: price SOL→USDC = {:.6}", price_sol_in_usdc);
    let slippage_bps = 100;
    let deposit_b = ((initial_amount_usdc * 10f64.powi(dec_b) as f64) * (1.0 + slippage_bps as f64 / 10_000.0))
    .round() as u64;
    let deposit_a = ((initial_amount_usdc / price_sol_in_usdc) * 10f64.powi(dec_a)).round() as u64;
    log::debug!(
        "debug: deposit_a_atoms = {}, deposit_b_atoms = {}",
        deposit_a, deposit_b
    );

    // Выбираем, какой токен вносить
    let liquidity_param = if price_low > price_sol_in_usdc {
        IncreaseLiquidityParam::TokenA(deposit_a)
    } else if price_high < price_sol_in_usdc {
        IncreaseLiquidityParam::TokenB(deposit_b)
    } else {
        IncreaseLiquidityParam::TokenB(deposit_b)
    };

    // Берём инструкции из SDK
    let OpenPositionInstruction {
        position_mint,
        quote: IncreaseLiquidityQuote { token_max_a, token_max_b, .. },
        instructions,
        additional_signers,
        ..
    } = open_position_instructions(
            &rpc,
            whirl_pk,
            price_low,
            price_high,
            liquidity_param,
            None,
            Some(wallet_pk),
        )
        .await
        .map_err(|e| anyhow!("open_position_instructions failed: {}", e))?;
    log::debug!(
        "debug: quote.token_max_a = {}, token_max_b = {}",
        token_max_a, token_max_b
    );

    // 3) Считаем, сколько нам нужно native SOL и USDC
    let need_sol  = token_max_a as f64 / 10f64.powi(pool.decimal_a as i32);
    let need_usdc = token_max_b as f64 / 10f64.powi(pool.decimal_b as i32);
    log::debug!("DEBUG: need_sol = {:.6}, need_usdc = {:.6}", need_sol, need_usdc);

    // 4) Баланс native SOL (с резервом 0.01)
    let lamports_total = rpc.get_balance(&wallet_pk).await
        .map_err(|e| anyhow!("get_balance SOL failed: {}", e))?;
    let sol_total = lamports_total as f64 / 1e9;
    let sol_reserve = 0.05;
    let mut sol_avail = (sol_total - sol_reserve).max(0.0);
    log::debug!(
        "DEBUG: SOL total = {:.6}, reserve = {:.6}, available = {:.6}",
        sol_total, sol_reserve, sol_avail
    );

    // 5) Баланс USDC в ATA
    let ata_usdc = get_associated_token_address(&wallet_pk, &Pubkey::from_str(pool.mint_b)?);
    let mut usdc_avail = rpc.get_token_account_balance(&ata_usdc).await
        .ok()
        .and_then(|r| r.amount.parse::<u64>().ok())
        .map(|a| a as f64 / 10f64.powi(pool.decimal_b as i32))
        .unwrap_or(0.0);
    log::debug!("DEBUG: USDC available = {:.6}", usdc_avail);

    // 6) Цена SOL→USDC из on-chain
    let acct  = rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&acct.data)?;
    let ma    = rpc.get_account(&whirl.token_mint_a).await?;
    let mb    = rpc.get_account(&whirl.token_mint_b).await?;
    let da    = Mint::unpack(&ma.data)?.decimals;
    let db    = Mint::unpack(&mb.data)?.decimals;
    let price_sol_in_usdc = sqrt_price_to_price(U128::from(whirl.sqrt_price), da, db);
    log::debug!("DEBUG: price_sol_in_usdc = {:.6}", price_sol_in_usdc);

    // 7) Если нативного SOL не хватает — свапаем USDC→SOL
    if sol_avail + 1e-9 < need_sol {
        let miss     = need_sol - sol_avail;
        let cost_usd = miss * price_sol_in_usdc;

        // пытаемся свапнуть
        let swap: SwapResult = match execute_swap(&pool, pool.mint_b, pool.mint_a, cost_usd).await {
            Ok(s) => s,
            Err(e) if e.to_string().contains("could not find account") => {
                log::debug!("⚠️  Jupiter «close account» error ignored: {}", e);
                // считаем, что мы действительно получили нужный SOL,
                // а USDC потратили cost_usd
                SwapResult {
                    balance_sell: usdc_avail - cost_usd,
                    balance_buy:  need_sol,
                }
            }
            Err(e) => return Err(anyhow!("swap USDC→SOL failed: {}", e)),
        };

        // обновляем остатки
        sol_avail  = swap.balance_buy;
        usdc_avail = swap.balance_sell;
    }


    // 8) Если не хватает USDC → свапаем SOL→USDC
    if usdc_avail + 1e-9 < need_usdc {
        let miss = need_usdc - usdc_avail;
        let cost = miss / price_sol_in_usdc;
        log::debug!("DEBUG: swap SOL→USDC: need {:.6} USDC costs {:.6} SOL", miss, cost);

        if sol_avail + 1e-9 < cost {
            return Err(anyhow!(
                "Недостаточно SOL для свапа: need {:.6}, have {:.6}",
                cost,
                sol_avail
            ));
        }

        // пытаться свапнуть, игнорируя "could not find account"
        let SwapResult { balance_sell: new_sol, balance_buy: new_usdc } =
            match execute_swap(&pool, pool.mint_a, pool.mint_b, miss).await {
                Ok(sr) => sr,
                Err(e) if e.to_string().contains("could not find account") => {
                    log::debug!("⚠️  Jupiter «close account» error ignored: {}", e);
                    // предполагаем, что SOL потратили cost, а USDC получили miss
                    SwapResult {
                        balance_sell: sol_avail - cost,
                        balance_buy:  usdc_avail + miss,
                    }
                }
                Err(e) => return Err(anyhow!("swap SOL→USDC failed: {}", e)),
            };

        sol_avail  = new_sol;
        usdc_avail = new_usdc;
        log::debug!("DEBUG: after swap → SOL = {:.6}, USDC = {:.6}", sol_avail, usdc_avail);
    }

    // 9) Финальные проверки
    if sol_avail  + 1e-9 < need_sol  { return Err(anyhow!("SOL всё ещё не хватает: need {:.6}, have {:.6}", need_sol, sol_avail)); }
    if usdc_avail + 1e-9 < need_usdc { return Err(anyhow!("USDC всё ещё не хватает: need {:.6}, have {:.6}", need_usdc, usdc_avail)); }

    // 10) Всё готово — шлём ровно те же инструкции из SDK
    log::debug!("DEBUG: sending SDK instructions…");
    let amount_wsol = token_max_a as f64 / 10f64.powi(dec_a);
    let amount_usdc = token_max_b as f64 / 10f64.powi(dec_b);
    let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + additional_signers.len());
    signers.push(&wallet);
    for kp in &additional_signers { signers.push(kp); }

    utils::send_and_confirm(rpc, instructions, &signers)
        .await
        .map_err(|e| anyhow!("send_and_confirm failed: {}", e))?;

    log::debug!("✅ Position opened, mint = {}", position_mint);
    Ok(OpenPositionResult {
        position_mint,
        amount_wsol,
        amount_usdc,
    })
}

pub async fn list_positions_for_owner() -> Result<Vec<PositionOrBundle>> {
    // 1) Настраиваем SDK на Mainnet
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
        .map_err(|e| anyhow!("SDK config failed: {}", e))?;

    // 2) RPC и адрес владельца
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner = wallet.pubkey();

    // 3) Фетчим позиции
    let positions = fetch_positions_for_owner(&rpc, owner)
        .await
        .map_err(|e| anyhow!("fetch_positions_for_owner failed: {}", e))?;

    Ok(positions)
}


pub async fn close_all_positions() -> Result<()> {
    // 1) Получаем все позиции
    let positions = list_positions_for_owner()
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
                if let Err(err) = close_whirlpool_position(mint).await {
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