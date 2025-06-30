// ─────────────────────────────────────────────────────────────────────────────
//  Открытие позиции в Raydium CLMM с проверкой наличия средств
//  (аналог open_with_funds_check_universal из Orca Whirlpool)
// ─────────────────────────────────────────────────────────────────────────────

use std::str::FromStr;

use solana_sdk::{
    instruction::Instruction,
    signature::{Keypair, Signer},
    system_program,
    transaction::Transaction,
};
use anyhow::Context;
use crate::database::triggers;
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_client::nonblocking::rpc_client::RpcClient;
use anchor_lang::AccountDeserialize;
use anchor_lang::{InstructionData, ToAccountMetas};
use anyhow::{anyhow, Result};
use raydium_amm_v3::states::POSITION_SEED;
use raydium_amm_v3::{
    accounts, instruction,
    states::{
        personal_position::PersonalPositionState,
        pool::PoolState,
    },
    ID as RAYDIUM_CLMM_PROGRAM_ID,
};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use spl_token;

use crate::{
    params::*,
    types::{OpenPositionResult, PoolConfig},
    utils::utils,
};

// Размер одного tick-array в Raydium (константа программы)
const TICK_ARRAY_SIZE: i32 = 60;
const OVR: f64 = 1.05;

/// Округляем tick до ближайшего корректного значения с учётом spacing.
fn align_tick(tick: i32, spacing: u16, ceil: bool) -> i32 {
    let spacing = spacing as i32;
    if ceil {
        ((tick + spacing - 1) / spacing) * spacing
    } else {
        (tick / spacing) * spacing
    }
}

/// Возвращает (tick_lower, tick_upper) выровненные по spacing.
fn nearest_valid_ticks_clmm(
    price_lo: f64,
    price_hi: f64,
    spacing: u16,
    dec0: u8,
    dec1: u8,
) -> (i32, i32) {
    use raydium_amm_v3::libraries::tick_math::{get_tick_at_sqrt_price, get_sqrt_price_at_tick};

    fn price_to_sqrt_x64(price: f64, dec0: u8, dec1: u8) -> u128 {
        // price = token1 / token0 (с учётом десятичных)
        let adj_price = price * 10f64.powi(dec0 as i32 - dec1 as i32);
        let sqrt_p = adj_price.sqrt();
        let val = sqrt_p * (1u128 << 64) as f64;
        val.round() as u128
    }

    let sqrt_lo = price_to_sqrt_x64(price_lo, dec0, dec1);
    let sqrt_hi = price_to_sqrt_x64(price_hi, dec0, dec1);

    let tick_lo = get_tick_at_sqrt_price(sqrt_lo).unwrap();
    let tick_hi = get_tick_at_sqrt_price(sqrt_hi).unwrap();

    (
        align_tick(tick_lo, spacing, false),
        align_tick(tick_hi, spacing, true),
    )
}

/// Преобразует sqrt_price_x64 программы → привычную цену token0 в token1.
fn sqrt_price_to_price(sqrt_price_x64: u128, dec0: u8, dec1: u8) -> f64 {
    let q64 = 2_f64.powi(64);
    let ratio = (sqrt_price_x64 as f64) / q64;
    let price = ratio * ratio; // (√P)^2 = P
    price * 10f64.powi(dec1 as i32 - dec0 as i32)
}

// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn open_with_funds_check_clmm(
    price_low: f64,
    price_high: f64,
    initial_amount_quote: f64, // token1 (quote-asset) в человеческих единицах
    pool: PoolConfig,
    slippage_bps: u16,
    _number: usize,            // сохранили для полной совместимости
) -> Result<OpenPositionResult> {
    // ───────── 1. RPC / Wallet / SDK ───────────────────────────────────────
    let rpc           = utils::init_rpc();
    let _wallet_guard = WALLET_MUTEX.lock().await;
    let wallet        = utils::load_wallet()?;
    let wallet_pk     = wallet.pubkey();

    // ───────── 2. Считываем PoolState ──────────────────────────────────────
    let pool_pk  = Pubkey::from_str(&pool.pool_address)?;
    let acc_data = rpc.get_account(&pool_pk).await?.data;
    let pool_state: PoolState =
        PoolState::try_deserialize(&mut acc_data.as_ref()).map_err(|e| anyhow!("{e}"))?;

    let dec0      = pool_state.mint_decimals_0;       // token0
    let dec1      = pool_state.mint_decimals_1;       // token1
    let spacing   = pool_state.tick_spacing;
    let sqrt_cur  = pool_state.sqrt_price_x64;
    let price_cur = sqrt_price_to_price(sqrt_cur, dec0, dec1);

    // ───────── 3. Валидные тики, выровненные цены ─────────────────────────
    let (tick_l, tick_u) = nearest_valid_ticks_clmm(
        price_low,
        price_high,
        spacing,
        dec0,
        dec1,
    );

    let price_low_aligned  = sqrt_price_to_price(
        raydium_amm_v3::libraries::tick_math::get_sqrt_price_at_tick(tick_l).unwrap(),
        dec0,
        dec1,
    );
    let price_high_aligned = sqrt_price_to_price(
        raydium_amm_v3::libraries::tick_math::get_sqrt_price_at_tick(tick_u).unwrap(),
        dec0,
        dec1,
    );

    // ───────── 4. Депозиты в атомах ───────────────────────────────────────
    let dep_token1_atoms = (initial_amount_quote * 10f64.powi(dec1 as i32)) as u64;
    let price0_in_1      = price_cur;
    let dep_token0_atoms = ((initial_amount_quote / price0_in_1) * 10f64.powi(dec0 as i32)) as u64;

    // ───────── 5. Выбор liquidity / base_flag ─────────────────────────────
    use raydium_amm_v3::libraries::{liquidity_math, tick_math};

    // sqrt границы диапазона
    let sqrt_l = tick_math::get_sqrt_price_at_tick(tick_l).unwrap();
    let sqrt_u = tick_math::get_sqrt_price_at_tick(tick_u).unwrap();

    // Сторона (base_flag) и ликвидность
    let (liquidity, amount_0_max, amount_1_max, base_flag) = if price_low_aligned > price0_in_1 {
        // диапазон выше рынка → нужен только token0
        (0u128, dep_token0_atoms.max(1), 1, Some(true))
    } else if price_high_aligned < price0_in_1 {
        // диапазон ниже рынка → нужен только token1
        (0u128, 1, dep_token1_atoms.max(1), Some(false))
    } else {
        // диапазон пересекает рынок → считаем ликвидность, кладём оба токена
        let liq_by_0 = liquidity_math::get_liquidity_from_single_amount_0(
            sqrt_cur,
            sqrt_l,
            sqrt_u,
            dep_token0_atoms,
        );
        let liq_by_1 = liquidity_math::get_liquidity_from_single_amount_1(
            sqrt_cur,
            sqrt_l,
            sqrt_u,
            dep_token1_atoms,
        );
        let liquidity = liq_by_0.min(liq_by_1).max(1);

        // токены, которые **могут** быть потрачены (сложим 5 % запас)
        let add_ovr = |x: u64| x + x / 20;
        (liquidity, add_ovr(dep_token0_atoms), add_ovr(dep_token1_atoms), None)
    };

    // ───────── 6. PDA для служебных аккаунтов ────────────────────────────
    let position_nft = Keypair::new();
    let position_nft_ata =
        get_associated_token_address(&wallet_pk, &position_nft.pubkey());

    let (personal_position_pk, _) = Pubkey::find_program_address(
        &[
            b"position",
            position_nft.pubkey().as_ref(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    let (protocol_position_pk, _) = Pubkey::find_program_address(
        &[
            b"position",
            pool_pk.as_ref(),
            &tick_l.to_be_bytes(),
            &tick_u.to_be_bytes(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    let calc_start = |tick: i32| {
        let step = spacing as i32 * TICK_ARRAY_SIZE;
        ((tick as f64) / step as f64).floor() as i32 * step
    };
    let tick_array_lower_start = calc_start(tick_l);
    let tick_array_upper_start = calc_start(tick_u);

    let (tick_array_lower_pk, _) = Pubkey::find_program_address(
        &[
            b"tick_array",
            pool_pk.as_ref(),
            &tick_array_lower_start.to_be_bytes(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );
    let (tick_array_upper_pk, _) = Pubkey::find_program_address(
        &[
            b"tick_array",
            pool_pk.as_ref(),
            &tick_array_upper_start.to_be_bytes(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    // ───────── 7. Сборка инструкции OpenPositionV2 ────────────────────────
    let ix_data = instruction::OpenPositionV2 {
        liquidity,
        amount_0_max,
        amount_1_max,
        tick_lower_index: tick_l,
        tick_upper_index: tick_u,
        tick_array_lower_start_index: tick_array_lower_start,
        tick_array_upper_start_index: tick_array_upper_start,
        with_metadata: false,
        base_flag,
    };

    let ix_accounts = accounts::OpenPositionV2 {
        payer:                    wallet_pk,
        position_nft_owner:       wallet_pk,
        position_nft_mint:        position_nft.pubkey(),
        position_nft_account:     position_nft_ata,
        metadata_account:         Pubkey::new_unique(), // можно zero; не используем metadata
        pool_state:               pool_pk,
        protocol_position:        protocol_position_pk,
        tick_array_lower:         tick_array_lower_pk,
        tick_array_upper:         tick_array_upper_pk,
        personal_position:        personal_position_pk,
        token_account_0:          Pubkey::from_str(&pool.mint_a)?,
        token_account_1:          Pubkey::from_str(&pool.mint_b)?,
        token_vault_0:            pool_state.token_vault_0,
        token_vault_1:            pool_state.token_vault_1,
        rent:                     anchor_lang::solana_program::sysvar::rent::ID,
        system_program:           system_program::ID,
        token_program:            spl_token::ID,
        associated_token_program: spl_associated_token_account::ID,
        metadata_program:         mpl_token_metadata::ID,
        token_program_2022:       spl_token_2022::ID,
        vault_0_mint:             pool_state.token_mint_0,
        vault_1_mint:             pool_state.token_mint_1,
    };

    let ix = Instruction {
        program_id: RAYDIUM_CLMM_PROGRAM_ID,
        accounts:   ix_accounts.to_account_metas(None),
        data:       ix_data.data(),
    };

    // ───────── 8. Отправка транзакции ─────────────────────────────────────
    let mut tx = Transaction::new_with_payer(&[ix.clone()], Some(&wallet_pk));
    let latest = rpc.get_latest_blockhash().await?;
    tx.sign(&[&wallet, &position_nft], latest);

    let instructions = vec![ix];
    let signers: Vec<&Keypair> = vec![&wallet, &position_nft];
    
    utils::send_and_confirm(rpc.clone(), instructions, &signers).await?;

    // ───────── 9. Результат ───────────────────────────────────────────────
    Ok(OpenPositionResult {
        position_mint: position_nft.pubkey(),
        amount_wsol:   (amount_0_max as f64) / 10f64.powi(dec0 as i32),
        amount_usdc:   (amount_1_max as f64) / 10f64.powi(dec1 as i32),
    })
}


/// Возвращает список персональных позиций Raydium-CLMM,
/// принадлежащих текущему кошельку.
///
/// * `pool` — необязательный Pubkey пула; если задан, в результат
///            попадут только позиции этого пула.
///
/// Выход: вектор пар `(Pubkey-позиции, PersonalPositionState)`.
pub async fn list_positions_for_owner_clmm(
    pool: Option<Pubkey>,
) -> Result<Vec<(Pubkey, PersonalPositionState)>> {
    use solana_account_decoder::UiAccountEncoding;
    use solana_client::{
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        rpc_filter::{Memcmp, RpcFilterType},
    };
    use std::sync::Arc;

    // 1. Блокируем mutex, берём RPC и паблик-ключ владельца
    let _wallet_guard = WALLET_MUTEX.lock().await;
    let rpc: Arc<RpcClient> = utils::init_rpc();
    let wallet   = utils::load_wallet()?;
    let owner_pk = wallet.pubkey();

    // 2. Конструируем фильтры: обязательный — владелец, опциональный — пул
    // offset-ы вычислены из структуры PersonalPositionState:
    // 0-й байт — bump[1], далее owner (32), position (32), pool_state (32)
    const OWNER_OFFSET: usize = 1;
    const POOL_OFFSET:  usize = 65; // 1 + 32 + 32

    let mut filters = vec![
        RpcFilterType::Memcmp(Memcmp::new(
            OWNER_OFFSET,
            MemcmpEncodedBytes::Bytes(owner_pk.to_bytes().to_vec()),
        )),
        RpcFilterType::DataSize(PersonalPositionState::LEN as u64),
    ];
    
    if let Some(pool_pk) = pool {
        filters.push(RpcFilterType::Memcmp(Memcmp::new(
            POOL_OFFSET,
            MemcmpEncodedBytes::Bytes(pool_pk.to_bytes().to_vec()),
        )));
    }
    // 3. Запрашиваем аккаунты программы Raydium-CLMM
    let cfg = RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        ..Default::default()
    };
    let accounts = rpc
        .get_program_accounts_with_config(&RAYDIUM_CLMM_PROGRAM_ID, cfg)
        .await
        .map_err(|e| anyhow!("get_program_accounts_with_config failed: {}", e))?;

    // 4. Десериализуем PersonalPositionState и собираем результат
    let mut out = Vec::with_capacity(accounts.len());

    for (pk, acc) in accounts {
        // безопасно: размер уже проверен DataSize-фильтром
        let mut data_slice: &[u8] = &acc.data;
        let state = match PersonalPositionState::try_deserialize(&mut data_slice) {
            Ok(s) => s,
            Err(_) => continue, // пропускаем повреждённые аккаунты
        };
        out.push((pk, state));
    }

    Ok(out)
}


/// Закрывает персональную позицию Raydium-CLMM.
///
/// * `position_pk` – PDA personal_position (seed = POSITION_SEED + nft_mint);
/// * `min_amount0` / `min_amount1` – минимально допустимые суммы вывода (слиппедж).
pub async fn close_clmm_position(
    position_pk: Pubkey,
    min_amount0: u64,
    min_amount1: u64,
) -> anyhow::Result<()> {
    use anchor_lang::AccountDeserialize;
    use raydium_amm_v3::{
        accounts, instruction,
        states::{personal_position::PersonalPositionState, pool::PoolState},
        ID as RAYDIUM_CLMM_PROGRAM_ID,
    };
    use solana_sdk::{
        instruction::Instruction,
        signature::{Keypair, Signer},
        system_program,
    };
    use spl_associated_token_account::{
        get_associated_token_address, instruction::create_associated_token_account,
    };
    use spl_token;

    // ───────── 0. RPC / кошелёк ────────────────────────────────────────────
    let rpc       = utils::init_rpc();
    let wallet    = utils::load_wallet()?;
    let wallet_pk = wallet.pubkey();

    // ───────── 1. PersonalPosition & PoolState ─────────────────────────────
    let pos_acc   = rpc.get_account(&position_pk).await?;
    let mut slice = pos_acc.data.as_slice();
    let pos_state = PersonalPositionState::try_deserialize(&mut slice)
        .map_err(|e| anyhow!("decode PersonalPositionState: {e}"))?;

    let pool_pk   = pos_state.pool_id;
    let pool_acc  = rpc.get_account(&pool_pk).await?;
    let mut ps    = pool_acc.data.as_slice();
    let pool_state: PoolState = PoolState::try_deserialize(&mut ps)
        .map_err(|e| anyhow!("decode PoolState: {e}"))?;

    // ───────── 2. PDA / ATA ────────────────────────────────────────────────
    // protocol_position = seed [POSITION_SEED, pool, tick_l, tick_u]
    let (protocol_position_pk, _) = Pubkey::find_program_address(
        &[
            POSITION_SEED.as_bytes(),
            pool_pk.as_ref(),
            &pos_state.tick_lower_index.to_be_bytes(),
            &pos_state.tick_upper_index.to_be_bytes(),
        ],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    // tick-array PDA (для DecreaseLiquidity)
    const TICK_ARRAY_SIZE: i32 = 60;
    let array_start = |t: i32| {
        let step = pool_state.tick_spacing as i32 * TICK_ARRAY_SIZE;
        (t / step) * step
    };
    let lower_start = array_start(pos_state.tick_lower_index);
    let upper_start = array_start(pos_state.tick_upper_index);

    let (tick_array_lower_pk, _) = Pubkey::find_program_address(
        &[b"tick_array", pool_pk.as_ref(), &lower_start.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );
    let (tick_array_upper_pk, _) = Pubkey::find_program_address(
        &[b"tick_array", pool_pk.as_ref(), &upper_start.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM_ID,
    );

    // NFT mint и ATA владельца
    let nft_mint    = pos_state.nft_mint;
    let nft_account = get_associated_token_address(&wallet_pk, &nft_mint);

    // recipient ATA для токен-0 / токен-1
    let ata0 = get_associated_token_address(&wallet_pk, &pool_state.token_mint_0);
    let ata1 = get_associated_token_address(&wallet_pk, &pool_state.token_mint_1);

    // создаём недостающие ATA (nft_account/ata0/ata1)
    let mut instructions: Vec<Instruction> = Vec::new();
    for (ata, mint) in [
        (nft_account, nft_mint),
        (ata0, pool_state.token_mint_0),
        (ata1, pool_state.token_mint_1),
    ] {
        if rpc.get_account(&ata).await.is_err() {
            instructions.push(create_associated_token_account(
                &wallet_pk,
                &wallet_pk,
                &mint,
                &spl_token::id(),
            ));
        }
    }

    // ───────── 3-A. DecreaseLiquidity (если есть ликвидность) ──────────────
    if pos_state.liquidity > 0 {
        let dec_ix = Instruction {
            program_id: RAYDIUM_CLMM_PROGRAM_ID,
            accounts: accounts::DecreaseLiquidity {
                nft_owner:                 wallet_pk,
                nft_account,                                   // ATA c NFT-позиции
                personal_position:         position_pk,
                pool_state:                pool_pk,
                protocol_position:         protocol_position_pk,
                token_vault_0:             pool_state.token_vault_0,
                token_vault_1:             pool_state.token_vault_1,
                tick_array_lower:          tick_array_lower_pk,
                tick_array_upper:          tick_array_upper_pk,
                recipient_token_account_0: ata0,
                recipient_token_account_1: ata1,
                token_program:             spl_token::ID,
            }
            .to_account_metas(None),
            data: instruction::DecreaseLiquidity {
                liquidity:    pos_state.liquidity,
                amount_0_min: min_amount0,
                amount_1_min: min_amount1,
            }
            .data(),
        };
        instructions.push(dec_ix);
    }

    // ───────── 3-B. ClosePosition ──────────────────────────────────────────
    let close_ix = Instruction {
        program_id: RAYDIUM_CLMM_PROGRAM_ID,
        accounts: accounts::ClosePosition {
            nft_owner:          wallet_pk,
            position_nft_mint:  nft_mint,
            position_nft_account: nft_account,
            personal_position:  position_pk,
            system_program:     system_program::ID,
            token_program:      spl_token::ID, // подходит и для token-2022
        }
        .to_account_metas(None),
        data: instruction::ClosePosition {}.data(),
    };
    instructions.push(close_ix);

    // ───────── 4. Отправляем транзакцию ────────────────────────────────────
    utils::send_and_confirm(rpc, instructions, &[&wallet]).await
}



/// Закрывает *все* открытые позиции текущего кошелька.
/// * `pool` – необязательный Pubkey пула-фильтр (None → все пулы).
pub async fn close_all_positions_clmm(pool: Option<Pubkey>) -> anyhow::Result<()> {
    use tokio::time::{sleep, Duration};

    // 0. «Флажок» и список позиций
    triggers::closing_switcher(true, None).await?;
    let positions = list_positions_for_owner_clmm(pool)
        .await
        .context("fetch positions")?;
    if positions.is_empty() {
        log::debug!("No Raydium-CLMM positions to close.");
        triggers::closing_switcher(false, None).await?;
        return Ok(());
    }

    // 1-й проход ──────────────────────────────────────────────────────────────
    let mut failed: Vec<Pubkey> = Vec::new();
    for (i, (pk, _)) in positions.iter().enumerate() {
        log::debug!("Closing {}/{} position={pk}", i + 1, positions.len());
        if let Err(e) = close_clmm_position(*pk, 0, 0).await {
            log::error!("  ❌ first-pass failed: {e}");
            failed.push(*pk);
        } else {
            log::debug!("  ✅ closed");
        }
        sleep(Duration::from_millis(500)).await;
    }

    // всё ок?
    if failed.is_empty() {
        log::debug!("🎉 All positions closed in first pass.");
        triggers::closing_switcher(false, None).await?;
        return Ok(());
    }

    // 2-й проход (повторяем только те, что не закрылись) ─────────────────────
    log::warn!("Retrying {} failed positions …", failed.len());
    for (j, pk) in failed.iter().enumerate() {
        log::debug!("Retry {}/{} position={pk}", j + 1, failed.len());
        if let Err(e) = close_clmm_position(*pk, 0, 0).await {
            log::error!("  ❌ second-pass failed: {e}");
        } else {
            log::debug!("  ✅ closed on retry");
        }
        sleep(Duration::from_millis(500)).await;
    }

    log::debug!("Done attempts to close all Raydium-CLMM positions.");
    triggers::closing_switcher(false, None).await?;
    Ok(())
}
