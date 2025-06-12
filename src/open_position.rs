//! Работа с позициями Whirlpool: открытие новой и долив ликвидности.

use std::{f64::EPSILON, str::FromStr};

use anyhow::{anyhow, bail, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_program, sysvar,
    transaction::Transaction,
};
use spl_associated_token_account::{get_associated_token_address, ID as ATA_PID};
use spl_token::ID as TOKEN_PID;

use orca_whirlpools_client::{
    get_tick_array_address, IncreaseLiquidityBuilder, OpenPositionWithMetadataBuilder, Position,
    Whirlpool,
};

use crate::{
    get_info::fetch_pool_position_info,
    params::{PoolConfig, KEYPAIR_FILENAME, RPC_URL},
    swap::execute_swap,
};

/* -------------------------------------------------------------------------- */
/*                    === константы и утилиты ===                              */
/* -------------------------------------------------------------------------- */
const TA_SIZE: i32 = 88;
const METADATA_PID: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

fn price_to_tick(price: f64) -> i32 {
    (price.ln() / 1.0001_f64.ln()).round() as i32
}
fn align_tick(tick: i32, spacing: i32) -> i32 {
    tick - tick.rem_euclid(spacing)
}
fn ata_balance_u64(rpc: &RpcClient, ata: &Pubkey) -> u64 {
    rpc.get_token_account_balance(ata)
        .ok()
        .and_then(|b| b.amount.parse::<u64>().ok())
        .unwrap_or(0)
}
fn amounts_for_unit_liquidity(sqrt_p: f64, sqrt_l: f64, sqrt_u: f64) -> (f64, f64) {
    if sqrt_p <= sqrt_l {
        ((sqrt_u - sqrt_l) / (sqrt_l * sqrt_u), 0.0)
    } else if sqrt_p < sqrt_u {
        let a = (sqrt_u - sqrt_p) / (sqrt_p * sqrt_u);
        let b = sqrt_p - sqrt_l;
        (a, b)
    } else {
        (0.0, sqrt_u - sqrt_l)
    }
}
fn select_liquidity(
    target_usdc: f64,
    price: f64,
    dec_a: i32,
    dec_b: i32,
    sqrt_p: f64,
    sqrt_l: f64,
    sqrt_u: f64,
) -> (u128, u64, u64) {
    let (a_unit, b_unit) = amounts_for_unit_liquidity(sqrt_p, sqrt_l, sqrt_u);
    let unit_val = a_unit * price + b_unit; // usd на L=1
    let liq = if unit_val < EPSILON { 0.0 } else { target_usdc / unit_val };
    let liq_u128 = (liq * 1e6).round() as u128;

    let max_a = (a_unit * liq * 10f64.powi(dec_a)).ceil() as u64;
    let max_b = (b_unit * liq * 10f64.powi(dec_b)).ceil() as u64;
    (liq_u128, max_a, max_b)
}

/* -------------------------------------------------------------------------- */
/*                             open_position                                  */
/* -------------------------------------------------------------------------- */
pub async fn open_position(pool: &PoolConfig, pct_down: f64, pct_up: f64) -> Result<()> {
    let payer = read_keypair_file(KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("read keypair: {}", e))?;
    let wallet = payer.pubkey();
    let rpc = RpcClient::new(RPC_URL.to_string());

    // если позиция уже есть — ничего не делаем
    if let Some(addr) = pool.position_address {
        let pos_pub = Pubkey::from_str(addr)?;
        if rpc.get_account(&pos_pub).is_ok() {
            println!("⚠️  Позиция уже существует – используйте add_liquidity()");
            return Ok(());
        }
    }

    let whirl_addr = Pubkey::from_str(pool.pool_address)?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&rpc.get_account(&whirl_addr)?.data)?;
    let spacing = whirl.tick_spacing as i32;

    let info = fetch_pool_position_info(pool).await?;
    let price = info.current_price;
    let sqrt_p = whirl.sqrt_price as f64 / 1.8446744073709552e19;

    let lower_price = price * (1.0 - pct_down);
    let upper_price = price * (1.0 + pct_up);
    let lower_tick = align_tick(price_to_tick(lower_price), spacing);
    let upper_tick = align_tick(price_to_tick(upper_price), spacing);

    let sqrt_l = (1.0001_f64).powi(lower_tick / 2);
    let sqrt_u = (1.0001_f64).powi(upper_tick / 2);

    let (liq, max_a, max_b) = select_liquidity(
        pool.initial_amount_usdc,
        price,
        pool.decimal_A as i32,
        pool.decimal_B as i32,
        sqrt_p,
        sqrt_l,
        sqrt_u,
    );

    // баланс + swap
    let ata_a = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_A)?);
    let ata_b = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_B)?);
    let bal_a = ata_balance_u64(&rpc, &ata_a);
    let bal_b = ata_balance_u64(&rpc, &ata_b);

    if bal_a < max_a {
        let need = (max_a - bal_a) as f64 / 10f64.powi(pool.decimal_A as i32) * price;
        execute_swap(pool, pool.mint_B, pool.mint_A, need).await?;
    }
    if bal_b < max_b {
        let need = (max_b - bal_b) as f64 / 10f64.powi(pool.decimal_B as i32);
        execute_swap(pool, pool.mint_A, pool.mint_B, need).await?;
    }

    // PDA
    let whirl_pid = Pubkey::from_str(pool.program)?;
    let (pos_pda, pos_bump) =
        Pubkey::find_program_address(&[b"position", whirl_addr.as_ref(), wallet.as_ref()], &whirl_pid);
    let meta_prog = Pubkey::from_str(METADATA_PID)?;
    let (meta_pda, meta_bump) =
        Pubkey::find_program_address(&[b"metadata", meta_prog.as_ref(), pos_pda.as_ref()], &meta_prog);

    // TickArray
    let span = spacing * TA_SIZE;
    let base = align_tick(lower_tick, span);
    let (ta_lower, _) = get_tick_array_address(&whirl_addr, base)?;
    let (ta_upper, _) = get_tick_array_address(&whirl_addr, base + span)?;

    // open position ix
    let open_ix = OpenPositionWithMetadataBuilder::new()
        .funder(wallet)
        .owner(wallet)
        .position(pos_pda)
        .position_mint(pos_pda)
        .position_metadata_account(meta_pda)
        .position_token_account(get_associated_token_address(&wallet, &pos_pda))
        .whirlpool(whirl_addr)
        .token_program(TOKEN_PID)
        .system_program(system_program::ID)
        .rent(sysvar::rent::ID)
        .associated_token_program(ATA_PID)
        .metadata_program(meta_prog)
        .metadata_update_auth(wallet)
        .position_bump(pos_bump)
        .metadata_bump(meta_bump)
        .tick_lower_index(lower_tick)
        .tick_upper_index(upper_tick)
        .instruction();

    // increase liquidity ix
    let inc_ix = IncreaseLiquidityBuilder::new()
        .whirlpool(whirl_addr)
        .token_program(TOKEN_PID)
        .position_authority(wallet)
        .position(pos_pda)
        .position_token_account(get_associated_token_address(&wallet, &pos_pda))
        .token_owner_account_a(ata_a)
        .token_owner_account_b(ata_b)
        .token_vault_a(whirl.token_vault_a)
        .token_vault_b(whirl.token_vault_b)
        .tick_array_lower(ta_lower)
        .tick_array_upper(ta_upper)
        .liquidity_amount(liq)
        .token_max_a(max_a)
        .token_max_b(max_b)
        .instruction();

    let bh = rpc.get_latest_blockhash()?;
    let tx =
        Transaction::new_signed_with_payer(&[open_ix, inc_ix], Some(&wallet), &[&payer], bh);
    rpc.send_and_confirm_transaction(&tx)?;

    println!(
        "✅ Открыта позиция [{:.4}–{:.4}] L={} (maxA={} maxB={})",
        lower_price, upper_price, liq, max_a, max_b
    );
    Ok(())
}

/* -------------------------------------------------------------------------- */
/*                              add_liquidity                                 */
/* -------------------------------------------------------------------------- */
pub async fn add_liquidity(pool: &PoolConfig, usd_add: f64) -> Result<()> {
    let payer = read_keypair_file(KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("read keypair: {}", e))?;
    let wallet = payer.pubkey();
    let rpc = RpcClient::new(RPC_URL.to_string());

    let pos_addr = match pool.position_address {
        Some(addr) => Pubkey::from_str(addr)?,
        None => {
            println!("⚠️  Позиция ещё не открыта — используйте open_position()");
            return Ok(());
        }
    };

    let pos_acc = match rpc.get_account(&pos_addr) {
        Ok(acc) => acc,
        Err(_) => {
            println!("⚠️  Позиция ещё не открыта — используйте open_position()"); return Ok(())
        }
    };
    let pos: Position = Position::from_bytes(&pos_acc.data)?;
    let whirl_addr = Pubkey::from_str(pool.pool_address)?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&rpc.get_account(&whirl_addr)?.data)?;
    let spacing = whirl.tick_spacing as i32;

    // Цена
    let info = fetch_pool_position_info(pool).await?;
    let price = info.current_price;
    let sqrt_p = whirl.sqrt_price as f64 / 1.8446744073709552e19;

    // sqrt(l/u)
    let sqrt_l = (1.0001_f64).powi(pos.tick_lower_index / 2);
    let sqrt_u = (1.0001_f64).powi(pos.tick_upper_index / 2);

    let (liq, max_a, max_b) = select_liquidity(
        usd_add,
        price,
        pool.decimal_A as i32,
        pool.decimal_B as i32,
        sqrt_p,
        sqrt_l,
        sqrt_u,
    );

    // Балансы → swap
    let ata_a = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_A)?);
    let ata_b = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_B)?);
    let bal_a = ata_balance_u64(&rpc, &ata_a);
    let bal_b = ata_balance_u64(&rpc, &ata_b);

    if bal_a < max_a {
        let need = (max_a - bal_a) as f64 / 10f64.powi(pool.decimal_A as i32) * price;
        execute_swap(pool, pool.mint_B, pool.mint_A, need).await?;
    }
    if bal_b < max_b {
        let need = (max_b - bal_b) as f64 / 10f64.powi(pool.decimal_B as i32);
        execute_swap(pool, pool.mint_A, pool.mint_B, need).await?;
    }

    // TickArray
    let span = spacing * TA_SIZE;
    let base = align_tick(pos.tick_lower_index, span);
    let (ta_lower, _) = get_tick_array_address(&whirl_addr, base)?;
    let (ta_upper, _) = get_tick_array_address(&whirl_addr, base + span)?;

    let inc_ix = IncreaseLiquidityBuilder::new()
        .whirlpool(whirl_addr)
        .token_program(TOKEN_PID)
        .position_authority(wallet)
        .position(pos_addr)
        .position_token_account(get_associated_token_address(&wallet, &pos_addr))
        .token_owner_account_a(ata_a)
        .token_owner_account_b(ata_b)
        .token_vault_a(whirl.token_vault_a)
        .token_vault_b(whirl.token_vault_b)
        .tick_array_lower(ta_lower)
        .tick_array_upper(ta_upper)
        .liquidity_amount(liq)
        .token_max_a(max_a)
        .token_max_b(max_b)
        .instruction();

    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(&[inc_ix], Some(&wallet), &[&payer], bh);
    rpc.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        CommitmentConfig::confirmed(),
        solana_client::rpc_config::RpcSendTransactionConfig::default(),
    )?;

    println!("➕ Liquidity +{} USD добавлена в позицию", usd_add);
    Ok(())
}
