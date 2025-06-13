//! liquidity.rs ─ безопасное открытие/добавление ликвидности в Orca Whirlpool
//! ----------------------------------------------------------------------------
//! • TickArray (если нужны) создаются в ТОМ ЖЕ tx, что и Open+Increase →
//!   лампорты списываются только при полном успехе.
//! • Подробная отладка (`debug!`, `info!`, `error!`) на всех RPC шагах.
//! • Исправлено BlockhashNotFound: симуляция теперь включает актуальный blockhash.
//! • Адрес последней позиции кэшируется в  last_position.txt
//! ----------------------------------------------------------------------------
use anyhow::bail;
use std::{
    env, f64::EPSILON, fs, str::FromStr, thread::sleep, time::Duration,
};
use orca_whirlpools_client::ID as WHIRLPOOL_PID;
use anyhow::{anyhow, Result};
use dotenv::dotenv;
use orca_whirlpools_client::{
    get_position_address, get_tick_array_address, IncreaseLiquidityBuilder,
    InitializeTickArrayBuilder, OpenPositionBuilder, Position, Whirlpool,
};
use solana_client::{
    client_error::ClientError,
    rpc_client::RpcClient,
    rpc_response::{RpcSimulateTransactionResult, RpcVersionInfo},
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::Message,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction, system_program, sysvar, transaction::Transaction,
};
use uint::construct_uint;
construct_uint! { pub struct U256(4); }
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account, ID as ATA_PID,
};
use spl_token::{instruction as token_ix, state::Mint, ID as TOKEN_PID};
use tracing::{debug, error, info, warn};

use crate::{
    get_info::fetch_pool_position_info,
    params::{PoolConfig, KEYPAIR_FILENAME, RPC_URL},
};

/* ─────────────────────────  maths / const  ───────────────────────── */
const MIN_TICK: i32 = -443_636;
const MAX_TICK: i32 =  443_636;
const TA_SIZE: i32 = 88;
const Q64: f64 = 1.844_674_407_370_955_2e19;
const WSOL: &str = "So11111111111111111111111111111111111111112";
fn align_tick(t: i32, spacing: i32) -> i32    { t - t.rem_euclid(spacing) }
fn price_to_tick(p: f64) -> i32               { (p.ln() / 1.0001_f64.ln()).round() as i32 }
fn align_tick_down(t: i32, spacing: i32) -> i32 {
    t - ((t % spacing) + spacing) % spacing
}
fn align_tick_up(t: i32, spacing: i32) -> i32 {
    align_tick_down(t, spacing) + spacing
}
fn amounts_for_unit_liq(sp: f64, sl: f64, su: f64) -> (f64, f64) {
    if sp <= sl {
        ((su - sl) / (sl * su), 0.0)
    } else if sp < su {
        ((su - sp) / (sp * su), sp - sl)
    } else {
        (0.0, su - sl)
    }
}
/// √(1.0001^tick) * 2⁶⁴  (с точностью, идентичной on-chain)
pub fn sqrt_price_x64_from_tick(tick: i32) -> u128 {
    assert!((MIN_TICK..=MAX_TICK).contains(&tick), "tick out of range");

    // -------- 1. таблица констант (как в Uniswap V3) ----------
    const MUL: [u128; 20] = [
        0xfffcb933bd6fad37aa2d162d1a594001,
        0xfff97272373d413259a46990580e213a,
        0xfff2e50f5f656932ef12357cf3c7fdcc,
        0xffe5caca7e10e4e61c3624eaa0941cd0,
        0xffcb9843d60f6159c9db58835c926644,
        0xff973b41fa98c081472e6896dfb254c0,
        0xff2ea16466c96a3843ec78b326b52861,
        0xfe5dee046a99a2a811c461f1969c3053,
        0xfcbe86c7900a88aedcffc83b479aa3a4,
        0xf987a7253ac413176f2b074cf7815e54,
        0xf3392b0822b70005940c7a398e4b70f3,
        0xe7159475a2c29b7443b29c7fa6e889d9,
        0xd097f3bdfd2022b8845ad8f792aa5825,
        0xa9f746462d870fdf8a65dc1f90e061e5,
        0x70d869a156d2a1b890bb3df62baf32f7,
        0x31be135f97d08fd981231505542fcfa6,
        0x9aa508b5b7a84e1c677de54f3e99bc9,
        0x5d6af8deddb81196699c329225ee604,
        0x2216e584f5fa1ea926041bedfe98,
        0x48a170391f7dc42444e8fa2,
    ];

    // -------- 2. алгоритм Uniswap getSqrtRatioAtTick ----------
    let mut ratio = if tick & 1 != 0 {
        U256::from(MUL[0])
    } else {
        // 1 << 128
        U256::from(1) << 128
    };

    let mut abs_tick = if tick < 0 { (-tick) as u32 } else { tick as u32 };

    for i in 1..20 {
        if (abs_tick & 1) != 0 {
            ratio = (ratio * U256::from(MUL[i])) >> 128;
        }
        abs_tick >>= 1;
        if abs_tick == 0 { break; }
    }

    if tick > 0 {
        ratio = U256::from(!0u128) / ratio;          // invert
    }

    // -------- 3. перевод в Q64 (ceil-round) ----------
    // сейчас ratio ~ Q128.128; нам нужен Q64:
    let upper = ratio >> 64;
    let rem   = ratio & (U256::from(1u128) << 64) - U256::one();
    let sqrt_x64 = if rem != U256::zero() { upper + U256::one() } else { upper };

    sqrt_x64.as_u128()
}
fn select_liq(
    usd: f64,
    price: f64,
    dec_a: i32,
    dec_b: i32,
    sp: f64,
    sl: f64,
    su: f64,
) -> (u128, u64, u64) {
    let (a_u, b_u) = amounts_for_unit_liq(sp, sl, su);
    let unit_cost = a_u * price + b_u;             // USDC за 1 юнит ликвидити
    let l = if unit_cost < EPSILON { 0.0 } else { usd / unit_cost };

    const SCALE_LIQ: f64 = 1_000_000_000.0;        // ← FIX: было 1e6, теперь 1e9
    (
        (l * SCALE_LIQ).round() as u128,
        (a_u * l * 10f64.powi(dec_a)).ceil() as u64,
        (b_u * l * 10f64.powi(dec_b)).ceil() as u64,
    )
}
fn ceil_div(a: U256, b: U256) -> U256 {
    if a % b == U256::zero() { a / b } else { a / b + 1 }
}

/// точный расчёт потребных токенов по on-chain формулам
fn amounts_from_liquidity(
    liq:      u128,
    sqrt_p:   u128,
    sqrt_low: u128,
    sqrt_up:  u128,
) -> (u64, u64) {
    let l  = U256::from(liq);
    let sp = U256::from(sqrt_p);
    let sl = U256::from(sqrt_low);
    let su = U256::from(sqrt_up);

    if sp <= sl {
        // цена ниже диапазона – нужен только token A
        let num   = l * (su - sl);
        let denom = ceil_div(sl * su, U256::from(1u128 << 64));
        (ceil_div(num, denom).as_u64(), 0)
    } else if sp < su {
        // цена внутри – нужны оба токена
        let amt_b = ceil_div(l * (sp - sl), U256::from(1u128 << 64)).as_u64();
        let num_a = l * (su - sp);
        let denom_a = ceil_div(sp * su, U256::from(1u128 << 64));
        (ceil_div(num_a, denom_a).as_u64(), amt_b)
    } else {
        // цена выше – нужен только token B
        let amt_b = ceil_div(l * (su - sl), U256::from(1u128 << 64)).as_u64();
        (0, amt_b)
    }
}
/* ─────────────────────────  RPC helpers  ───────────────────────── */
fn derive_position_pda(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[b"position", mint.as_ref()], &WHIRLPOOL_PID)
    }
    


fn best_rpc() -> RpcClient {
    let _ = dotenv();
    let urls = [
        env::var("HELIUS_HTTP").ok(),
        env::var("QUICKNODE_HTTP").ok(),
        env::var("ANKR_HTTP").ok(),
        env::var("CHAINSTACK_HTTP").ok(),
        Some(env::var("RPC_URL").unwrap_or_else(|_| RPC_URL.to_string())),
    ];
    for url in urls.into_iter().flatten() {
        match try_rpc(&url) {
            Ok(c) => return c,
            Err(e) => warn!("RPC {url} skipped: {e}"),
        }
    }
    RpcClient::new_with_commitment(
        "https://api.mainnet-beta.solana.com".to_string(),
        CommitmentConfig::confirmed(),
    )
}
fn try_rpc(url: &str) -> Result<RpcClient> {
    let c = RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed());
    for pause in [0, 150, 300] {
        if pause > 0 { sleep(Duration::from_millis(pause)); }
        match c.get_version() {
            Ok(RpcVersionInfo { .. }) => { info!("RPC ok: {url}"); return Ok(c); }
            Err(ClientError { .. })   => break,
            Err(e)                    => return Err(anyhow!(e)),
        }
    }
    Err(anyhow!("RPC unhealthy"))
}

/* ─────────────────────────  WSOL helpers  ───────────────────────── */

fn ata_balance(rpc: &RpcClient, ata: &Pubkey) -> u64 {
    rpc.get_token_account_balance(ata)
        .ok().and_then(|b| b.amount.parse().ok()).unwrap_or(0)
}
fn wrap_if_needed(rpc: &RpcClient, payer: &Keypair, ata: Pubkey, need: u64) -> Result<()> {
    if need == 0 { return Ok(()); }
    let wallet = payer.pubkey();
    debug!("wrap {} lamports to WSOL ATA {}", need, ata);
    if rpc.get_account(&ata).is_err() {
        send_tx(rpc,
                &[create_associated_token_account(&wallet, &wallet,
                                                  &Pubkey::from_str(WSOL)?, &TOKEN_PID)],
                &[payer])?;
    }
    let ix1 = system_instruction::transfer(&wallet, &ata, need);
    let ix2 = token_ix::sync_native(&TOKEN_PID, &ata)?;
    send_tx(rpc, &[ix1, ix2], &[payer])
}

/* ─────────────────────────  tx helpers  ───────────────────────── */

fn send_tx(rpc: &RpcClient, ixs: &[Instruction], signers: &[&Keypair]) -> Result<()> {
    let payer = signers[0].pubkey();

    for _ in 0..3 {
        let bh = rpc.get_latest_blockhash()?;

        // 1. message с актуальным hash
        let mut msg = Message::new(ixs, Some(&payer));
        msg.recent_blockhash = bh;

        // 2. подписываем
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(signers, bh);

        match rpc.send_and_confirm_transaction_with_spinner(&tx) {
            Ok(sig) => {
                info!("tx sent: {sig}");
                return Ok(());
            }
            Err(e) if format!("{e:?}").contains("BlockhashNotFound") => continue,
            Err(e) => return Err(anyhow!(e)),
        }
    }
    Err(anyhow!("tx: blockhash kept expiring"))
}

fn simulate_ok(rpc: &RpcClient, ixs: &[Instruction], payer: &Pubkey) -> Result<()> {
    // 1. берём свежий block-hash
    let bh: Hash = rpc.get_latest_blockhash()?;

    // 2. строим message и подставляем hash
    let mut msg = Message::new(ixs, Some(payer));
    msg.recent_blockhash = bh;

    // 3. собираем unsigned-tx
    let tx = Transaction::new_unsigned(msg);

    // 4. симуляция
    let RpcSimulateTransactionResult { err, logs, .. } = rpc.simulate_transaction(&tx)?.value;
    debug!("sim-logs: {logs:?}");        // <-- подробная отладка
    err.map_or(Ok(()), |e| Err(anyhow!("simulation failed: {e:?}")))
}

// ------------------ самые важные изменения ------------------



fn ensure_balance(rpc: &RpcClient,
    payer:&Keypair,
    ata: Pubkey,
    need: u64) -> Result<()> {
if need == 0 { return Ok(()); }   // ничего покупать не нужно
let have = ata_balance(rpc, &ata);
if need > have {
wrap_if_needed(rpc, payer, ata, need - have)?;
}
Ok(())
}
/* ════════════════════ OPEN POSITION ════════════════════ */
/// Открывает позицию в Orca Whirlpool и сразу кладёт ликвидность.
///
/// * `pct_dn`, `pct_up` — ширина диапазона (0.003 = ±0.3 %)
/// Открыть новую позицию и сразу внести ликвидность
///
/// * `pct_dn`, `pct_up` — доли в процентах (0.003 = ±0.3 %)
//------------------------------------------------------------------
// open_position.rs  (вставьте вместо старой функции)
//------------------------------------------------------------------
pub async fn open_position(
    pool:   &PoolConfig,
    pct_dn: f64,          // доля вниз  (0.003 = −0.3 %)
    pct_up: f64,          // доля вверх (0.003 = +0.3 %)
) -> Result<Pubkey> {
    /* ───── подготовка RPC / Whirlpool ────────────────────────── */
    let payer  = read_keypair_file(KEYPAIR_FILENAME)
    .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
    let rpc    = best_rpc();
    let wallet = payer.pubkey();

    let whirl_addr = Pubkey::from_str(pool.pool_address)?;
    let whirl: Whirlpool =
        Whirlpool::from_bytes(&rpc.get_account(&whirl_addr)?.data)?;

    /* ───── mint-ы и decimals (A – token0, B – token1) ────────── */
    let mint_a = whirl.token_mint_a;
    let mint_b = whirl.token_mint_b;
    let dec_a  = Mint::unpack(&rpc.get_account(&mint_a)?.data)?.decimals as u32;
    let dec_b  = Mint::unpack(&rpc.get_account(&mint_b)?.data)?.decimals as u32;

    /* ───── диапазон тиков от текущего тика ───────────────────── */
    let spacing    = whirl.tick_spacing as i32;
    let cur_tick   = whirl.tick_current_index;

    fn pct_to_ticks(p: f64) -> i32 {
        ((1.0 + p).ln() / 1.0001_f64.ln()).round() as i32
    }
    let mut t_low = cur_tick - pct_to_ticks(pct_dn);
    let mut t_up  = cur_tick + pct_to_ticks(pct_up);
    t_low = align_tick_down(t_low, spacing);
    t_up  = align_tick_up  (t_up , spacing);
    if t_up <= t_low { t_up = t_low + spacing; }

    info!("tick_current {cur_tick}  range [{t_low}..{t_up}] ({} ticks)",
          t_up - t_low);

    /* ───── точные √цен для формул Uniswap V3 ─────────────────── */
    const Q64: f64 = 1.844_674_407_370_955_2e19;          // 2^64
    let sqrt_l_x64 = sqrt_price_x64_from_tick(t_low);
    let sqrt_u_x64 = sqrt_price_x64_from_tick(t_up);
    let sqrt_p_x64 = whirl.sqrt_price; 

    /* ───── бюджет: вся сумма в token B (USDC) ────────────────── */
    let budget_b_ui   = pool.initial_amount_usdc;               //  напр. 40 USDC
    let budget_b_raw  = (budget_b_ui * 10f64.powi(dec_b as i32)).ceil();

    let budget_b_int: u128 = budget_b_raw as u128;       // truncate/round as appropriate
    let delta_b = U256::from(budget_b_int);        // lamports USDC
    let liq_u128  = ((delta_b << 64) / U256::from(sqrt_p_x64 - sqrt_l_x64)).as_u128();
    
    let (need_a, need_b) =
        amounts_from_liquidity(liq_u128, sqrt_p_x64, sqrt_l_x64, sqrt_u_x64);
    
    let max_a = need_a + 1;           // +1 лампорт запас
    let max_b = need_b + 1;

    info!("needA {} ({} SOL)  needB {} ({} USDC)",
          need_a, need_a as f64 / 10f64.powi(dec_a as i32),
          need_b, need_b as f64 / 10f64.powi(dec_b as i32));

    /* ───── ATA + обёртка SOL (если надо) ─────────────────────── */
    let ata_a = get_associated_token_address(&wallet, &mint_a);
    let ata_b = get_associated_token_address(&wallet, &mint_b);
    ensure_balance(&rpc, &payer, ata_a, max_a)?;
    ensure_balance(&rpc, &payer, ata_b, max_b)?;

    /* ───── TickArray PDA (шаг 88 × spacing) ──────────────────── */
    let span      = spacing * TA_SIZE;
    let base_low  = align_tick_down(t_low, span);
    let base_up   = align_tick_down(t_up , span);
    let (ta_low, _) = get_tick_array_address(&whirl_addr, base_low)?;
    let (ta_up , _) = get_tick_array_address(&whirl_addr, base_up)?;

    /* ───── PDA позиции + mint ────────────────────────────────── */
    let mut pos_mint = Keypair::new();
    let (pos_pda, bump) = loop {
        let (pda, b) = derive_position_pda(&pos_mint.pubkey());
        if rpc.get_account(&pda).is_err() { break (pda, b); }
        pos_mint = Keypair::new();
    };
    let ata_pos = get_associated_token_address(&wallet, &pos_mint.pubkey());

    /* ───── инструкции ───────────────────────────────────────── */
    let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(400_000)];

    for (ta, start) in [(ta_low, base_low), (ta_up, base_up)] {
        if rpc.get_account(&ta).is_err() {
            ixs.push(
                InitializeTickArrayBuilder::new()
                    .whirlpool(whirl_addr)
                    .funder(wallet)
                    .tick_array(ta)
                    .start_tick_index(start)
                    .system_program(system_program::ID)
                    .instruction(),
            );
        }
    }

    ixs.push(
        OpenPositionBuilder::new()
            .funder(wallet)
            .owner(wallet)
            .position(pos_pda)
            .position_mint(pos_mint.pubkey())
            .position_token_account(ata_pos)
            .whirlpool(whirl_addr)
            .token_program(TOKEN_PID)
            .system_program(system_program::ID)
            .rent(sysvar::rent::ID)
            .associated_token_program(ATA_PID)
            .position_bump(bump)
            .tick_lower_index(t_low)
            .tick_upper_index(t_up)
            .instruction(),
    );

    ixs.push(
        IncreaseLiquidityBuilder::new()
            .whirlpool(whirl_addr)
            .token_program(TOKEN_PID)
            .position_authority(wallet)
            .position(pos_pda)
            .position_token_account(ata_pos)
            .token_owner_account_a(ata_a)
            .token_owner_account_b(ata_b)
            .token_vault_a(whirl.token_vault_a)
            .token_vault_b(whirl.token_vault_b)
            .tick_array_lower(ta_low)
            .tick_array_upper(ta_up)
            .liquidity_amount(liq_u128)
            .token_max_a(max_a)
            .token_max_b(max_b)
            .instruction(),
    );

    /* ───── simulate → send ───────────────────────────────────── */
    simulate_ok(&rpc, &ixs, &wallet)?;                // теперь 6017 НЕ возникает
    send_tx(&rpc, &ixs, &[&payer, &pos_mint])?;

    fs::write("last_position.txt", pos_pda.to_string())?;
    info!("✅ позиция создана: {pos_pda}");
    Ok(pos_pda)
}

/* ════════════════════ ADD LIQUIDITY ════════════════════ */

pub async fn add_liquidity(pool: &PoolConfig, usd: f64) -> Result<()> {
    let payer = read_keypair_file(KEYPAIR_FILENAME)
    .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
    let rpc   = best_rpc();
    let wallet = payer.pubkey();

    let pos_pda: Pubkey = if let Some(p) = pool.position_address {
        Pubkey::from_str(p)?
    } else {
        Pubkey::from_str(fs::read_to_string("last_position.txt")?.trim())?
    };

    let pos: Position   = Position::from_bytes(&rpc.get_account(&pos_pda)?.data)?;
    let whirl_addr      = Pubkey::from_str(pool.pool_address)?;
    let whirl: Whirlpool= Whirlpool::from_bytes(&rpc.get_account(&whirl_addr)?.data)?;

    let price = fetch_pool_position_info(pool).await?.current_price;
    let (liq, max_a, max_b) = {
        let sp = whirl.sqrt_price as f64 / Q64;
        let sl = (1.0001_f64).powi(pos.tick_lower_index / 2);
        let su = (1.0001_f64).powi(pos.tick_upper_index / 2);
        select_liq(usd, price, pool.decimal_A.into(), pool.decimal_B.into(), sp, sl, su)
    };
    debug!("add_liquidity liq={liq}  maxA={max_a}  maxB={max_b}");

    let ata_a = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_A)?);
    let ata_b = get_associated_token_address(&wallet, &Pubkey::from_str(pool.mint_B)?);
    if pool.mint_A == WSOL && ata_balance(&rpc, &ata_a) < max_a {
        wrap_if_needed(&rpc, &payer, ata_a, max_a - ata_balance(&rpc, &ata_a))?;
    }
    if pool.mint_B == WSOL && ata_balance(&rpc, &ata_b) < max_b {
        wrap_if_needed(&rpc, &payer, ata_b, max_b - ata_balance(&rpc, &ata_b))?;
    }

    let spacing = whirl.tick_spacing as i32;
    let span    = spacing * TA_SIZE;
    let base    = align_tick(pos.tick_lower_index, span);
    let (ta_l, _) = get_tick_array_address(&whirl_addr, base)?;
    let (ta_u, _) = get_tick_array_address(&whirl_addr, base + span)?;

    let mut ixs: Vec<Instruction> =
        vec![ComputeBudgetInstruction::set_compute_unit_limit(200_000)];
    if rpc.get_account(&ta_l).is_err() {
        debug!("TA lower missing – init");
        ixs.push(
            InitializeTickArrayBuilder::new()
                .whirlpool(whirl_addr).funder(wallet)
                .tick_array(ta_l).start_tick_index(base)
                .system_program(system_program::ID).instruction(),
        );
    }
    if rpc.get_account(&ta_u).is_err() {
        debug!("TA upper missing – init");
        ixs.push(
            InitializeTickArrayBuilder::new()
                .whirlpool(whirl_addr).funder(wallet)
                .tick_array(ta_u).start_tick_index(base + span)
                .system_program(system_program::ID).instruction(),
        );
    }

    ixs.push(
        IncreaseLiquidityBuilder::new()
            .whirlpool(whirl_addr).token_program(TOKEN_PID)
            .position_authority(wallet).position(pos_pda)
            .position_token_account(get_associated_token_address(&wallet, &pos.position_mint))
            .token_owner_account_a(ata_a).token_owner_account_b(ata_b)
            .token_vault_a(whirl.token_vault_a).token_vault_b(whirl.token_vault_b)
            .tick_array_lower(ta_l).tick_array_upper(ta_u)
            .liquidity_amount(liq).token_max_a(max_a).token_max_b(max_b)
            .instruction(),
    );

    simulate_ok(&rpc, &ixs, &wallet)?;
    send_tx(&rpc, &ixs, &[&payer])?;
    info!("✅ add_liquidity +{usd} USDC");
    Ok(())
}

