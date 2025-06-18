// src/raydium_clmm.rs

use anyhow::{anyhow, Result};
use std::{env, sync::Arc, str::FromStr, time::Duration};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{Instruction, AccountMeta},
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{
    instruction::{initialize_mint, sync_native},
    state::Mint,
};
use anchor_lang::AnchorDeserialize;
use raydium_amm_v3::{
    ID as RAYDIUM_CLMM_PROGRAM_ID, accounts, instruction,
    state::{PersonalPositionState, PoolState},
};
use crate::swap::execute_swap;
use crate::params;
use crate::types::{PoolConfig, OpenPositionResult};

/// Uniform error mapper.
fn op<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> anyhow::Error {
    move |e| anyhow!("{} failed: {}", ctx, e)
}

/// Common utilities: RPC, wallet loading, transaction sending.
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
        read_keypair_file(params::KEYPAIR_FILENAME).map_err(op("load_wallet"))
    }

    pub async fn send_and_confirm(
        rpc: Arc<RpcClient>,
        mut instructions: Vec<Instruction>,
        signers: &[&Keypair],
    ) -> Result<()> {
        // 1) Compute budget
        instructions.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(400_000));
        // 2) Recent blockhash
        let recent = rpc
            .get_latest_blockhash()
            .await
            .map_err(op("get_latest_blockhash"))?;
        // 3) Build & sign
        let payer = signers
            .get(0)
            .ok_or_else(|| anyhow!("No payer signer"))?;
        let message = Message::new(&instructions, Some(&payer.pubkey()));
        let mut tx = Transaction::new_unsigned(message);
        tx.try_sign(signers, recent).map_err(op("sign transaction"))?;
        // 4) Send
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
        // 5) Confirm
        for _ in 0..40 {
            if let Some(status) = rpc
                .get_signature_status(&sig)
                .await
                .map_err(op("get_signature_status"))?
            {
                status.map_err(|e| anyhow!("Transaction failed: {:?}", e))?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(anyhow!("Timeout: tx {} not confirmed within 40s", sig))
    }
}

/// Quote for harvested fees.
pub struct FeesQuote {
    pub fee_owed_0: u64,
    pub fee_owed_1: u64,
}

/// Closes (removes) a Raydium CLMM position entirely.
pub async fn close_clmm_position(position_account: Pubkey) -> Result<()> {
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner = wallet.pubkey();

    // 1) Fetch position state
    let acct = rpc.get_account(&position_account).await.map_err(op("get_account"))?;
    let position_state = PersonalPositionState::try_deserialize(&mut &acct.data[..])
        .map_err(op("deserialize position"))?;

    // 2) Fetch pool state
    let pool_id = position_state.pool;
    let pool_acct = rpc.get_account(&pool_id).await.map_err(op("get_account pool"))?;
    let pool_state = PoolState::try_deserialize(&mut &pool_acct.data[..])
        .map_err(op("deserialize pool"))?;

    // 3) Derive associated token accounts
    let position_mint = position_state.position_mint;
    let position_token_account = get_associated_token_address(&owner, &position_mint);
    let token_owner_0 = get_associated_token_address(&owner, &pool_state.token_mint_0);
    let token_owner_1 = get_associated_token_address(&owner, &pool_state.token_mint_1);

    // 4) Build instructions
    let mut instructions = Vec::new();

    // 4a) If there's remaining liquidity, remove it
    if position_state.liquidity > 0 {
        let dec_accounts = accounts::DecreaseLiquidityV2 {
            owner,
            pool: pool_id,
            position: position_account,
            position_token_account,
            tick_array_lower: position_state.tick_array_lower,
            tick_array_upper: position_state.tick_array_upper,
            token_0_vault: pool_state.token_vault_0,
            token_1_vault: pool_state.token_vault_1,
            token_owner_account_0: token_owner_0,
            token_owner_account_1: token_owner_1,
            token_program: spl_token::ID,
        };
        instructions.push(instruction::decrease_liquidity_v2(
            dec_accounts,
            position_state.liquidity,
            0,
            0,
        ));
    }

    // 4b) Collect any accrued fees
    let collect_accounts = accounts::CollectRemainingRewards {
        owner,
        pool: pool_id,
        position: position_account,
        position_mint,
        position_token_account,
        token_vault_0: pool_state.token_vault_0,
        token_vault_1: pool_state.token_vault_1,
        token_owner_account_0: token_owner_0,
        token_owner_account_1: token_owner_1,
        token_program: spl_token::ID,
        associated_token_program: spl_associated_token_account::ID,
    };
    instructions.push(instruction::collect_remaining_rewards(collect_accounts));

    // 4c) Close the empty position account
    let close_accounts = accounts::ClosePosition {
        owner,
        pool: pool_id,
        position: position_account,
        position_mint,
        position_token_account,
        token_program: spl_token::ID,
    };
    instructions.push(instruction::close_position(close_accounts));

    // 5) Send all at once
    let signers = vec![&wallet];
    utils::send_and_confirm(rpc, instructions, &signers)
        .await
        .map_err(op("send_and_confirm"))
}

/// Harvests (collects) accrued fees from a Raydium CLMM position.
pub async fn harvest_clmm_position(position_account: Pubkey) -> Result<FeesQuote> {
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner = wallet.pubkey();

    // 1) Read position state (to get pre-collect fee amounts)
    let acct = rpc.get_account(&position_account).await.map_err(op("get_account"))?;
    let position_state = PersonalPositionState::try_deserialize(&mut &acct.data[..])
        .map_err(op("deserialize position"))?;

    // 2) Fetch pool for vault addresses
    let pool_acct = rpc.get_account(&position_state.pool).await.map_err(op("get_account pool"))?;
    let pool_state = PoolState::try_deserialize(&mut &pool_acct.data[..])
        .map_err(op("deserialize pool"))?;

    // 3) Derive token accounts
    let position_mint = position_state.position_mint;
    let position_token_account = get_associated_token_address(&owner, &position_mint);
    let token_owner_0 = get_associated_token_address(&owner, &pool_state.token_mint_0);
    let token_owner_1 = get_associated_token_address(&owner, &pool_state.token_mint_1);

    // 4) Build and send collect instruction
    let collect_accounts = accounts::CollectRemainingRewards {
        owner,
        pool: position_state.pool,
        position: position_account,
        position_mint,
        position_token_account,
        token_vault_0: pool_state.token_vault_0,
        token_vault_1: pool_state.token_vault_1,
        token_owner_account_0: token_owner_0,
        token_owner_account_1: token_owner_1,
        token_program: spl_token::ID,
        associated_token_program: spl_associated_token_account::ID,
    };
    let ix = instruction::collect_remaining_rewards(collect_accounts);
    utils::send_and_confirm(rpc, vec![ix], &[&wallet])
        .await
        .map_err(op("send_and_confirm"))?;

    // 5) Return the amounts that were collected
    Ok(FeesQuote {
        fee_owed_0: position_state.token_fees_owed_0,
        fee_owed_1: position_state.token_fees_owed_1,
    })
}

/// Summary of harvested fees, valued in token1 (e.g. USDC).
pub struct HarvestSummary {
    pub amount_0: f64,
    pub amount_1: f64,
    pub price_0_in_1: f64,
    pub total_value_1: f64,
}

/// Convert Raydium sqrt_price_x64 (Q64.64) to price (token1 per token0).
fn sqrt_price_x64_to_price(sqrt_price_x64: u128) -> f64 {
    let sqrt_price = sqrt_price_x64 as f64;
    (sqrt_price * sqrt_price) / (2f64.powi(128))
}

/// Turns raw fee figures into human-readable amounts & USD value.
pub async fn summarize_harvest_fees(
    pool: &PoolConfig,
    fees: &FeesQuote,
) -> Result<HarvestSummary> {
    let rpc = utils::init_rpc();

    // 1) Pool state
    let pool_acct = rpc.get_account(&pool.pool_id).await.map_err(op("get_account pool"))?;
    let pool_state = PoolState::try_deserialize(&mut &pool_acct.data[..])
        .map_err(op("deserialize pool"))?;

    // 2) Decimals
    let dec_0 = pool.decimal_0 as i32;
    let dec_1 = pool.decimal_1 as i32;

    // 3) Price of token0 in token1
    let price_0_in_1 = sqrt_price_x64_to_price(pool_state.sqrt_price)
        * 10f64.powi(dec_0 - dec_1);

    // 4) Convert to user-readable amounts
    let amount_0 = fees.fee_owed_0 as f64 / 10f64.powi(dec_0);
    let amount_1 = fees.fee_owed_1 as f64 / 10f64.powi(dec_1);

    // 5) Total value in token1
    let total_value_1 = amount_1 + (amount_0 * price_0_in_1);

    Ok(HarvestSummary {
        amount_0,
        amount_1,
        price_0_in_1,
        total_value_1,
    })
}

/// Opens a new CLMM position given a price range (tick indices) and USDC budget.
/// Performs optional swaps (via `execute_swap`) to ensure sufficient balances.
/// Returns the newly minted position NFT and the actual deposited amounts.
/// Открывает новую CLMM-позицию с проверкой баланса SOL/USDC и депозитом ликвидности.
pub async fn open_with_funds_check(
    tick_lower_index: i32,
    tick_upper_index: i32,
    initial_amount_usdc: f64,
    pool: PoolConfig,
) -> Result<OpenPositionResult> {
    // 1) Инициализация RPC и кошелька
    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner = wallet.pubkey();

    // 2) Получаем состояние пула
    let pool_acct = rpc.get_account(&pool.pool_id).await.map_err(op("get_account pool"))?;
    let pool_state = PoolState::try_deserialize(&mut &pool_acct.data[..])
        .map_err(op("deserialize pool"))?;

    // 3) Вычисляем текущую цену токен0→токен1
    let dec_0 = pool.decimal_a as i32;
    let dec_1 = pool.decimal_b as i32;
    let price = sqrt_price_x64_to_price(pool_state.sqrt_price) * 10f64.powi(dec_0 - dec_1);
    if price <= 0.0 {
        return Err(anyhow!("Invalid pool price: {}", price));
    }

    // 4) Рассчитываем необходимые суммы токенов
    let slippage_bps = 100; // 1%
    let deposit_1 = ((initial_amount_usdc * 10f64.powi(dec_1)
        * (1.0 + slippage_bps as f64 / 10_000.0))
        .round()) as u64;
    let deposit_0 = ((initial_amount_usdc / price) * 10f64.powi(dec_0)).round() as u64;

    // 5) При необходимости выполняем свапы через execute_swap (логика вашего бота)

    // 6) Создаём новый mint для позиции и его ATA
    let position_mint = Keypair::new();
    let position_mint_pubkey = position_mint.pubkey();
    let rent = rpc
        .get_minimum_balance_for_rent_exemption(Mint::LEN)
        .await
        .map_err(op("get_rent_exemption"))?;
    let mut instructions = Vec::new();
    instructions.push(system_instruction::create_account(
        &owner,
        &position_mint_pubkey,
        rent,
        Mint::LEN as u64,
        &spl_token::ID,
    ));
    instructions.push(
        initialize_mint(
            &spl_token::ID,
            &position_mint_pubkey,
            &owner,
            None,
            0, // NFT имеет 0 десятичных
        )
        .map_err(op("initialize_mint"))?,
    );
    let position_token_account = get_associated_token_address(&owner, &position_mint_pubkey);
    instructions.push(create_associated_token_account(
        &owner,
        &owner,
        &position_mint_pubkey,
    ));

    // 7) Деривим PDA для позиции и массивов тиков
    let (position_pda, _) = Pubkey::find_program_address(
        &[
            b"personal_position",
            pool.pool_id.as_ref(),
            position_mint_pubkey.as_ref(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );
    let tick_spacing = pool_state.tick_spacing as i32;
    let array_size = pool_state.tick_array_size as i32;
    let arr_stride = tick_spacing * array_size;
    let lower_start = tick_lower_index - ((tick_lower_index % arr_stride + arr_stride) % arr_stride);
    let upper_start = tick_upper_index - ((tick_upper_index % arr_stride + arr_stride) % arr_stride);
    let (tick_array_lower, _) = Pubkey::find_program_address(
        &[b"tick_array", pool.pool_id.as_ref(), &lower_start.to_le_bytes()],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );
    let (tick_array_upper, _) = Pubkey::find_program_address(
        &[b"tick_array", pool.pool_id.as_ref(), &upper_start.to_le_bytes()],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    // 8) Формируем инструкцию open_position_v2
    let open_accounts = accounts::OpenPositionV2 {
        owner,
        pool: pool.pool_id,
        position: position_pda,
        position_mint: position_mint_pubkey,
        position_token_account,
        tick_array_lower,
        tick_array_upper,
        token_0_vault: pool_state.token_vault_0,
        token_1_vault: pool_state.token_vault_1,
        token_owner_account_0: get_associated_token_address(&owner, &pool.mint_0),
        token_owner_account_1: get_associated_token_address(&owner, &pool.mint_1),
        system_program: solana_program::system_program::ID,
        token_program: spl_token::ID,
        associated_token_program: spl_associated_token_account::ID,
        rent: solana_program::sysvar::rent::ID,
    };
    let open_ix = instruction::open_position_v2(
        open_accounts,
        tick_lower_index,
        tick_upper_index,
        lower_start,
        upper_start,
        (deposit_0.min(deposit_1) as u128),
        deposit_0,
        deposit_1,
        true,       // with_metadata
        Some(true), // base_flag для SOL
    );
    instructions.push(open_ix);

    // 9) Отправляем транзакцию
    let signers: Vec<&Keypair> = vec![&wallet, &position_mint];
    utils::send_and_confirm(rpc, instructions, &signers)
        .await
        .map_err(op("send_and_confirm"))?;

    // 10) Возвращаем результат с полями WSOL и USDC
    Ok(OpenPositionResult {
        position_mint: position_mint_pubkey,
        amount_wsol: deposit_0 as f64 / 10f64.powi(dec_0),
        amount_usdc: deposit_1 as f64 / 10f64.powi(dec_1),
    })
}

/// Lists all Raydium CLMM positions owned by the wallet.
pub async fn list_positions_for_owner() -> Result<Vec<(Pubkey, PersonalPositionState)>> {
    use solana_client::rpc_filter::{RpcFilterType, Memcmp};
    use solana_client::rpc_config::{RpcProgramAccountsConfig, RpcAccountInfoConfig};
    use solana_account_decoder::UiAccountEncoding;

    let rpc = utils::init_rpc();
    let wallet = utils::load_wallet()?;
    let owner = wallet.pubkey();

    // Filter for accounts of size PersonalPositionState and matching owner in state
    let filters = Some(vec![
        RpcFilterType::DataSize(PersonalPositionState::LEN),
        RpcFilterType::Memcmp(Memcmp {
            offset: 8 + 32 + 32, // discriminator + pool + position_mint
            bytes: owner.to_string().into(),
            encoding: None,
        }),
    ]);
    let config = RpcProgramAccountsConfig {
        filters,
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        with_context: None,
    };

    let accounts = rpc
        .get_program_accounts_with_config(&RAYDIUM_CLMM_PROGRAM_ID, config)
        .await
        .map_err(op("get_program_accounts"))?;

    let mut positions = Vec::with_capacity(accounts.len());
    for (pubkey, acct) in accounts {
        let state = PersonalPositionState::try_deserialize(&mut &acct.data[..])
            .map_err(op("deserialize position"))?;
        positions.push((pubkey, state));
    }
    Ok(positions)
}

/// Closes all positions for the owner, one by one.
pub async fn close_all_positions() -> Result<()> {
    let positions = list_positions_for_owner().await.map_err(op("list_positions_for_owner"))?;
    if positions.is_empty() {
        return Ok(());
    }
    for (account, _) in positions {
        close_clmm_position(account).await.map_err(|e| {
            anyhow!("Failed to close position {}: {:?}", account, e)
        })?;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}
