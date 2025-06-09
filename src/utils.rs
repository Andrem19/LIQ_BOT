// src/utils.rs

use anyhow::{Context, Result};
use anchor_client::{Client, Cluster, Program};
use raydium_amm_v3::{
    ID as CLMM_PROGRAM_ID,
    instruction::{increase_liquidity, decrease_liquidity, collect_fees, swap},
    state::PoolState,
    util::tick_math::{price_to_tick, tick_to_price},
};
use solana_account_decoder::UiAccountData;
use solana_client::{
    rpc_client::RpcClient,
    rpc_filter::RpcTokenAccountsFilter,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
use crate::params::{RPC_URL, KEYPAIR_FILENAME};

/// Читает Keypair из ~/.config/solana/<KEYPAIR_FILENAME>
pub fn load_keypair() -> Result<Keypair> {
    let path = dirs::home_dir()
        .context("Не удалось определить домашний каталог")?
        .join(".config/solana")
        .join(KEYPAIR_FILENAME);
    read_keypair_file(&path)
        .with_context(|| format!("Не удалось прочитать keypair из {:?}", path))
}

/// Создаёт RPC-клиент Solana
pub fn create_rpc_client() -> RpcClient {
    RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed())
}

/// Создаёт Anchor-программу для CLMM
pub fn create_program_client(keypair: &Keypair) -> Program {
    let client = Client::new_with_options(
        Cluster::Custom(RPC_URL.to_string(), RPC_URL.to_string()),
        keypair.clone(),
        CommitmentConfig::confirmed(),
    );
    client.program(CLMM_PROGRAM_ID)
}

/// Возвращает состояние пула, текущую цену и tick_spacing
pub fn get_pool_state(program: &Program, pool: &Pubkey) -> Result<(PoolState, f64, u16)> {
    let state: PoolState = program.account(*pool)
        .context("Не удалось получить состояние пула")?;
    let price = state.price();
    let spacing = state.tick_spacing as u16;
    Ok((state, price, spacing))
}

/// Рассчитывает нижний и верхний тик из цены и полуширины диапазона
pub fn compute_tick_range(current_price: f64, tick_spacing: u16, range_half: f64) -> Result<(i32, i32)> {
    let lower_price = current_price * (1.0 - range_half);
    let upper_price = current_price * (1.0 + range_half);
    let lower_tick = price_to_tick(lower_price, tick_spacing)
        .context("Ошибка конвертации lower_price в tick")?;
    let upper_tick = price_to_tick(upper_price, tick_spacing)
        .context("Ошибка конвертации upper_price в tick")?;
    Ok((lower_tick, upper_tick))
}

/// Возвращает баланс SPL-токена (в минимальных единицах)
pub fn get_token_balance(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
    let accounts = rpc.get_token_accounts_by_owner(owner, RpcTokenAccountsFilter::Mint(*mint))
        .context("Не удалось получить SPL-аккаунты")?;
    if accounts.is_empty() {
        return Ok(0);
    }
    if let UiAccountData::Json(parsed) = &accounts[0].1.data {
        let ui_amount = parsed.token_amount.ui_amount.unwrap_or(0.0);
        let decimals = parsed.token_amount.decimals;
        let lamports = (ui_amount * 10u64.pow(decimals as u32) as f64).round() as u64;
        Ok(lamports)
    } else {
        Ok(0)
    }
}

/// Посылает и подтверждает транзакцию с набором инструкций
pub fn send_tx(rpc: &RpcClient, keypair: &Keypair, ix: &[solana_sdk::instruction::Instruction]) -> Result<String> {
    let payer = keypair.pubkey();
    let mut tx = Transaction::new_with_payer(ix, Some(&payer));
    let (recent_hash, _) = rpc.get_latest_blockhash()?;
    tx.sign(&[keypair], recent_hash);
    let sig = rpc.send_and_confirm_transaction(&tx)
        .context("Ошибка при отправке транзакции")?;
    Ok(sig.to_string())
}

/// Инструкции для снятия всей ликвидности и сбора комиссий
pub fn withdraw_and_collect(
    program: &Program,
    pool: &Pubkey,
    lower_tick: i32,
    upper_tick: i32,
) -> Vec<solana_sdk::instruction::Instruction> {
    let payer = program.payer();
    let dec = decrease_liquidity(&CLMM_PROGRAM_ID, pool, &payer, lower_tick, upper_tick, u128::MAX);
    let col = collect_fees(&CLMM_PROGRAM_ID, pool, &payer);
    vec![dec, col]
}

/// Инструкция для депозита ликвидности
pub fn deposit_liquidity(
    program: &Program,
    pool: &Pubkey,
    lower_tick: i32,
    upper_tick: i32,
    amount_usdc: u128,
    amount_wsol: u128,
) -> solana_sdk::instruction::Instruction {
    let payer = program.payer();
    increase_liquidity(&CLMM_PROGRAM_ID, pool, &payer, lower_tick, upper_tick, amount_usdc, amount_wsol)
}

/// Инструкция для обмена излишка токена через CLMM
pub fn swap_token(
    program: &Program,
    pool: &Pubkey,
    amount_in: u128,
) -> solana_sdk::instruction::Instruction {
    let payer = program.payer();
    swap(&CLMM_PROGRAM_ID, pool, &payer, amount_in, 0)
}
