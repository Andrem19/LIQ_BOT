// whirlpool_ops.rs
// ─────────────────────────────────────────────────────────────────────────
//! Высоко-уровневые функции для работы с Orca Whirlpools:
//! • open_position
//! • add_liquidity
//! • reduce_liquidity
//! • harvest_position
//! • close_position
//!
//! Все операции атомарны: TickArray (если нужны) и сама логика идут
//!   в одном tx  → либо всё успешно, либо всё откатывается.
//! RPC-эндпоинт выбирается из .env  (HELIUS → QUICKNODE → ANKR → CHAINSTACK → fallback)
//! Подтверждение ждётся опросом `get_signature_status` (до 40 с).


use anyhow::{anyhow, Result};
use dotenv::dotenv;
use std::{env, str::FromStr, sync::Arc, time::Duration};
use orca_tx_sender::CommitmentLevel;
use orca_tx_sender::{build_and_send_transaction, get_rpc_client, set_rpc};
use orca_whirlpools::{
    close_position_instructions, decrease_liquidity_instructions,
    harvest_position_instructions, increase_liquidity_instructions,
    open_position_instructions, set_whirlpools_config_address, DecreaseLiquidityParam,
    IncreaseLiquidityParam, WhirlpoolsConfigInput,
};
use orca_whirlpools_client::Whirlpool;
use orca_whirlpools_client::ID as whirlpools_program_id;
use orca_whirlpools_client::Position;

use orca_whirlpools_core::{sqrt_price_to_price, U128};

use solana_client::{
    nonblocking::rpc_client::RpcClient
};
use spl_associated_token_account::{get_associated_token_address, instruction as ata_ix};
use spl_token::ID as TOKEN_PROGRAM_ID;

use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};

use spl_token::solana_program::program_pack::Pack;
use spl_token::state::Mint;
use tokio::time::sleep;
use crate::params;
/* ════════════════════════ public API ════════════════════════ */
use spl_token::{
    state::{Account as TokenAccount, Mint as TokenMint},
};
use solana_sdk::instruction::AccountMeta;
use solana_sdk::hash::Hash;
use solana_sdk::system_program;

const TICKS_PER_ARRAY: i32 = 88;

const DEFAULT_SLIPPAGE_BPS: u16 = 5_000;            // 1 %
const CONFIRM_TIMEOUT_S: u64   = 40;      
/// Конфиг пула, чтобы не таскать кучу строк по коду.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub program:               &'static str, // всегда whirLbM…  на mainnet, но оставляем для devnet
    pub name:                  &'static str,
    pub pool_address:          &'static str,
    pub position_address:      Option<&'static str>, // None  → позиции ещё нет
    pub mint_a:                &'static str,
    pub mint_b:                &'static str,
    pub decimal_a:             u16,
    pub decimal_b:             u16,
    pub initial_amount_usdc:   f64,
}
fn tick_array_start_index(tick_index: i32, tick_spacing: i32) -> i32 {
    let arr_size = tick_spacing * TICKS_PER_ARRAY;
    tick_index.div_euclid(arr_size) * arr_size        // правильный floor и для отрицательных
}

/// PDA tick-array по whirlpool + start_index
fn tick_array_pda(whirlpool: &Pubkey, start_index: i32) -> Pubkey {
    let seed = start_index.to_le_bytes();             // точно little-endian
    Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), &seed],
        &whirlpools_program_id,                     // константа из orca_whirlpools_client
    )
    .0
}



/// Дискриминант Anchor для global fn `initialize_tick_array`
const INIT_TICK_ARRAY_DISCR: [u8; 8] =
    *b"\x0b\xbc\xc1\xd6\x8d\x5b\x95\xb8";

/// Строит Instruction `initialize_tick_array`
/// (funder = signer, payer ренты)
fn make_initialize_tick_array_ix(
    funder: &Pubkey,
    whirlpool: &Pubkey,
    tick_array: &Pubkey,
    start_index: i32,
) -> Instruction {
    // payload: discrim + start_index (i32 LE)
    let mut data = INIT_TICK_ARRAY_DISCR.to_vec();
    data.extend_from_slice(&start_index.to_le_bytes());

    let accounts = vec![
        // 0. Whirlpool
        AccountMeta::new_readonly(*whirlpool, false),
        // 1. Tick-array PDA (создаём/инициализируем)
        AccountMeta::new(*tick_array, true),
        // 2. Плательщик ренты + подпись
        AccountMeta::new(*funder, true),
        // 3. System program
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
    ];

    Instruction {
        program_id: whirlpools_program_id,  // whirLbMi…
        accounts,
        data,
    }
}
pub async fn normalize_position_mint(rpc: &RpcClient, key: Pubkey) -> Result<Pubkey> {
    let acc = rpc.get_account(&key).await?;

    match acc.owner {
        // 1) PDA позиции
        owner if owner == whirlpools_program_id => {
            let pos = Position::from_bytes(&acc.data)?;
            Ok(pos.position_mint)
        }

        // 2) Всё, что принадлежит SPL-Token-программе
        owner if owner == TOKEN_PROGRAM_ID => {
            match acc.data.len() {
                TokenMint::LEN => Ok(key),                         // это уже mint
                TokenAccount::LEN => {
                    // это ATA/обычный счёт: вытягиваем mint из поля
                    let token_acc = TokenAccount::unpack(&acc.data)?;
                    Ok(token_acc.mint)
                }
                _ => Err(anyhow!("unknown SPL-Token account type for {}", key)),
            }
        }

        _ => Err(anyhow!("{} is not a Position PDA, mint, nor token account", key)),
    }
}

fn dedup_instructions(ixs: Vec<Instruction>) -> Vec<Instruction> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut out  = Vec::with_capacity(ixs.len());

    for ix in ixs {
        // «подпись» инструкции — (program_id, data-bytes, account-keys)
        let mut sig = ix.program_id.to_bytes().to_vec();
        sig.extend(&ix.data);
        for acc in &ix.accounts {
            sig.extend(acc.pubkey.to_bytes());
        }

        if seen.insert(sig) {
            out.push(ix); // ещё не встречалась
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cluster {
    Mainnet,
    Devnet,
    Testnet,
}

impl Cluster {
    /// Очень грубое, но надёжное определение по URL RPC.
    pub fn from_rpc_url(url: &str) -> Self {
        if url.contains("devnet") {
            Cluster::Devnet
        } else if url.contains("testnet") {
            Cluster::Testnet
        } else {
            Cluster::Mainnet
        }
    }

    pub fn whirlpool_cfg(self) -> orca_whirlpools::WhirlpoolsConfigInput {
        use orca_whirlpools::WhirlpoolsConfigInput::*;
        match self {
            Cluster::Mainnet => SolanaMainnet,
            Cluster::Devnet  => SolanaDevnet,
            Cluster::Testnet => EclipseTestnet,
        }
    }
}
/// Открывает позицию с диапазоном `cur±pct` и сразу кладёт ликвидность.
/// Возвращает PDA позиции (mint NFT).
pub async fn open_position(
    pool: &PoolConfig,
    pct_dn: f64,
    pct_up: f64,
) -> Result<Pubkey> {
    let ctx = Ctx::init().await?;

    // ───── достаём данные пула ─────
    let whirl_pk = Pubkey::from_str(pool.pool_address)?;
    let whirl_acct = ctx.rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&whirl_acct.data)?;

    // decimals пары (полезно позже)
    let dec_a = pool.decimal_a as u8;
    let dec_b = pool.decimal_b as u8;

    // текущая цена
    let price_now = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);

    // нужные абсолютные границы
    let price_low  = price_now * (1.0 - pct_dn);
    let price_high = price_now * (1.0 + pct_up);

    if !(price_low > 0.0 && price_high > price_low) {
        return Err(anyhow!(
            "invalid range: price_low={}, price_high={}", price_low, price_high
        ));
    }


    // переводим $USDC ➜ эквивалент SOL и вносим **Token A**
    let deposit_token_a = ((pool.initial_amount_usdc / price_now)
                           * 10f64.powi(pool.decimal_a as i32))
                          .floor() as u64;

    let quote = open_position_instructions(
        &ctx.rpc,
        whirl_pk,
        price_low,
        price_high,
        IncreaseLiquidityParam::TokenA(deposit_token_a),
        Some(DEFAULT_SLIPPAGE_BPS),
        Some(ctx.wallet_pubkey),
    )
    .await
    .map_err(|e| anyhow!("open_position_instructions: {}", e))?;

    // отправляем
    ctx.send_instructions(quote.instructions, &quote.additional_signers)
        .await?;

    Ok(quote.position_mint)
}


/// возвращает набор инструкций «создай ATA, если нужно» для обоих токенов пула
async fn ensure_wallet_atas(
    ctx: &Ctx,
    mints: &[Pubkey],
) -> Result<()> {
    let mut ixs = Vec::<Instruction>::new();

    for mint in mints {
        if let Some(ix) = maybe_create_ata(&ctx.rpc, &ctx.wallet_pubkey, mint).await? {
            ixs.push(ix);
        }
    }

    if ixs.is_empty() {
        return Ok(());
    }
    // отдельный маленький tx
    ctx.send_instructions(ixs, &[]).await
}
/// Добавить ликвидность в уже открытую позицию (`usd` – эквивалент в token-B).
pub async fn add_liquidity(pool: &PoolConfig, position_mint: Pubkey, usd: f64) -> Result<()> {
    let ctx = Ctx::init().await?;

    let amount_b = (usd * 10f64.powi(pool.decimal_b as i32)) as u64;

    let quote = increase_liquidity_instructions(
        &ctx.rpc,
        position_mint,
        IncreaseLiquidityParam::TokenB(amount_b),
        Some(DEFAULT_SLIPPAGE_BPS),
        Some(ctx.wallet_pubkey),
    )
    .await
    .map_err(|e| anyhow!("increase_liquidity_instructions: {}", e))?;

    ctx.send_instructions(quote.instructions, &quote.additional_signers)
        .await
}

/// Снять часть ликвидности (по token-B).
pub async fn reduce_liquidity(pool: &PoolConfig, position_mint: Pubkey, usd: f64) -> Result<()> {
    let ctx = Ctx::init().await?;
    let amount_b = (usd * 10f64.powi(pool.decimal_b as i32)) as u64;

    let quote = decrease_liquidity_instructions(
        &ctx.rpc,
        position_mint,
        DecreaseLiquidityParam::TokenB(amount_b),
        Some(DEFAULT_SLIPPAGE_BPS),
        Some(ctx.wallet_pubkey),
    )
    .await
    .map_err(|e| anyhow!("decrease_liquidity_instructions: {}", e))?;

    ctx.send_instructions(quote.instructions, &quote.additional_signers)
        .await
}

/// Собрать комиссии + награды, не закрывая позицию.
pub async fn harvest_position(position_mint: Pubkey) -> Result<()> {
    let ctx = Ctx::init().await?;

    let quote = harvest_position_instructions(
        &ctx.rpc,
        position_mint,
        Some(ctx.wallet_pubkey),
    )
    .await
    .map_err(|e| anyhow!("harvest_position_instructions: {}", e))?;

    ctx.send_instructions(quote.instructions, &quote.additional_signers)
        .await
}

/// Полностью закрыть позицию (снять ликвидность, собрать всё, закрыть NFT).
pub async fn close_position(
    position_mint: Pubkey,
    pool: &PoolConfig,
) -> Result<()> {
    // 1) init RPC/wallet context (exactly as before)
    let ctx = Ctx::init().await?;

    // 2) fetch the whirlpool account so we know all the reward‐mint addresses
    let whirl_pk = Pubkey::from_str(pool.pool_address)?;
    let whirl_data = ctx.rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&whirl_data.data)?;

    // 3) collect all the mints we need ATAs for:
    //    • the position NFT mint
    //    • token_mint_a, token_mint_b
    //    • any reward mints
    let mut mints = Vec::new();
    mints.push(position_mint);
    mints.push(whirl.token_mint_a);
    mints.push(whirl.token_mint_b);
    for r in &whirl.reward_infos {
        if r.mint != Pubkey::default() {
            mints.push(r.mint);
        }
    }

    // 4) build a small vector of ATA‐creation instructions (if needed)
    let mut ata_ixs = Vec::new();
    for mint in mints.iter() {
        if let Some(ix) = maybe_create_ata(&ctx.rpc, &ctx.wallet_pubkey, mint).await? {
            ata_ixs.push(ix);
        }
    }
    // send them in one small tx (skipping if empty)
    if !ata_ixs.is_empty() {
        ctx.send_instructions(ata_ixs, &[]).await?;
    }

    // 5) generate the “close position” instructions via the Orca SDK
    let quote = close_position_instructions(
        &ctx.rpc,
        position_mint,
        Some(DEFAULT_SLIPPAGE_BPS),
        Some(ctx.wallet_pubkey),
    )
    .await
    .map_err(|e| anyhow!("close_position_instructions: {}", e))?;

    // 6) send the actual close‐position TX; the SDK will include
    //    any tick‐array inits in the right shape/order
    ctx.send_instructions(quote.instructions, &quote.additional_signers)
        .await
}

/// Helper: check an ATA, return `Some(create_ix)` if it doesn’t exist.
async fn maybe_create_ata(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<Option<Instruction>> {
    let ata = get_associated_token_address(owner, mint);
    let resp = rpc
        .get_account_with_commitment(&ata, CommitmentConfig::confirmed())
        .await?;
    if resp.value.is_some() {
        Ok(None)
    } else {
        Ok(Some(spl_associated_token_account::instruction::create_associated_token_account(
            owner, owner, mint, &spl_token::ID,
        )))
    }
}



/* ═════════════ helpers / context ═════════════ */

        // ожидание tx, 40 сек
/// Храним общий контекст (wallet + rpc) чтобы не инициализировать каждый раз.
struct Ctx {
    /// used to build all of the `open_…_instructions` / `increase_…_instructions`
    /// so that get_account sees “confirmed” state immediately
    rpc:           Arc<RpcClient>,
    wallet:        Keypair,
    wallet_pubkey: Pubkey,
}

impl Ctx {
    async fn init() -> Result<Self> {
        dotenv().ok();
        // pick the URL from your .env
        let url = env::var("HELIUS_HTTP")
            .or_else(|_| env::var("QUICKNODE_HTTP"))
            .or_else(|_| env::var("ANKR_HTTP"))
            .or_else(|_| env::var("CHAINSTACK_HTTP"))
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

        // 1) configure the “sending” client (used by orca_tx_sender)
        set_rpc(&url).await.map_err(anyhow::Error::msg)?;

        // 2) build a *separate* client in “confirmed” mode for all .get_account calls
        let rpc = Arc::new(RpcClient::new_with_commitment(
            url.clone(),
            CommitmentConfig::confirmed(),
        ));

        // 3) set the whirlpools program config
        set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
            .map_err(|e| anyhow!("set_whirlpools_config_address: {}", e))?;

        // 4) load your wallet
        let wallet = read_keypair_file(env::var("KEYPAIR_PATH")
            .unwrap_or_else(|_| "id.json".to_string()))
            .map_err(|e| anyhow!("read_keypair_file: {}", e))?;
        let wallet_pubkey = wallet.pubkey();

        Ok(Ctx { rpc, wallet, wallet_pubkey })
    }

    /// Отправить инструкции **одним** tx + ожидание подтверждения.
    async fn send_instructions(
        &self,
        ixs: Vec<Instruction>,
        extra: &[Keypair],
    ) -> Result<()> {
    
        let ixs = dedup_instructions(ixs);

        let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + extra.len());
        signers.push(&self.wallet);
        signers.extend(extra);
    
        let sig = build_and_send_transaction(
            ixs, &signers, Some(CommitmentLevel::Confirmed), None
        )
        .await
        .map_err(|e| anyhow!("build_and_send_transaction: {e}"))?;
    
        // ждём подтверждения …
        for _ in 0..CONFIRM_TIMEOUT_S {
            if let Some(st) = self.rpc.get_signature_status(&sig).await? {
                return st.map(|_| {
                    println!("✅ tx confirmed {sig}");
                }).map_err(|e| anyhow!("Transaction failed: {:?}", e));
            }
            sleep(Duration::from_secs(1)).await;
        }
        Err(anyhow!("Timeout: tx {sig} not confirmed"))
    }
}
