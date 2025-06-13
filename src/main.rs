// // src/main.rs

// pub mod telegram;
// pub mod params;
// pub mod get_info;
// pub mod exchange;
// pub mod types;
// pub mod swap;
// pub mod net;
// pub mod open_position;
// pub mod compare;

// use anyhow::Result;
// use params::POOLS;
// use get_info::fetch_pool_position_info;
// use swap::execute_swap;
// use types::PoolPositionInfo;

// #[tokio::main]
// async fn main() -> Result<()> {
//     // 1) Получаем информацию по позиции
//     let pool_cfg = &POOLS[0];
//     // compare::compare_pools().await?;
//     // if pool_cfg.position_address.is_some() {
//     //     let _info: PoolPositionInfo = fetch_pool_position_info(pool_cfg).await?;
//     // } else {
//     //     println!("⚠️  Позиция ещё не открыта — пропускаем получение информации.");
//     // }

//     // 2) Совершаем своп: продаём 50 USDC (mint_B) → покупаем WSOL (mint_A)
//     // let result = swap::execute_swap(pool_cfg, pool_cfg.mint_B, pool_cfg.mint_A, 10.0).await?;
//     // println!(
//     //     "После свопа — USDC: {:.6}, WSOL: {:.9}",
//     //     result.balance_sell,
//     //     result.balance_buy
//     // );

//     Ok(())
// }
// src/main.rs

// pub mod telegram;
// pub mod params;
// pub mod get_info;
// pub mod exchange;
// pub mod types;
// pub mod swap;
// pub mod net;
// pub mod open_position;

// use anyhow::Result;
// use std::time::Duration;
// use tokio::time::sleep;
// use crate::exchange::hyperliquid::hl::HL;
// use dotenv::dotenv;

// #[tokio::main]
// async fn main() -> Result<()> {
//     dotenv().ok();
//     // 1. Инициализация HL-клиента из переменных окружения.
//     // Перед запуском убедитесь, что в окружении установлены:
//     //   HLSECRET_1 — ваш приватный ключ в hex
//     //   (опционально) HL_SUBACCOUNT — адрес саб-аккаунта в hex
//     let mut hl = HL::new_from_env().await?;

//     // Торгуемая пара
//     let symbol = "SOLUSDT";

//     // 2. Узнаём текущую цену
//     let price = hl.get_last_price(symbol).await?;
//     println!("Current price for {}: {}", symbol, price);

//     // 3. Открываем длинную позицию на 100 USDT
//     //    reduce_only = false, amount_coins не используется в режиме открытия
//     let (open_cloid, entry_price) = hl
//         .open_market_order(symbol, "Sell", 40.0, false, 0.0)
//         .await?;
//     println!("Opened long position: cloid = {}, entry_price = {}", open_cloid, entry_price);
//     println!("Wait 10 sec");
//     sleep(Duration::from_secs(10)).await;
//     // 4. Берём размер позиции из API, чтобы выставить стоп-лосс
//     let position = hl
//         .get_position(symbol)
//         .await?
//         .expect("Position not found after opening");
//     println!("Position details: {:?}", position);

//     // 5. Ставим стоп-лосс 1% от цены входа
//     hl.open_sl(symbol, "Sell", position.size, position.entry_px, 0.01)
//         .await?;
//     println!("Stop-loss set at 1%");

//     // 6. Ждём одну минуту
//     sleep(Duration::from_secs(20)).await;

//     // 7. Узнаём текущий нереализованный PnL
//     let position_after = hl
//         .get_position(symbol)
//         .await?
//         .expect("Position disappeared");
//     println!(
//         "Unrealized PnL after 1 minute: {}",
//         position_after.unrealized_pnl
//     );

//     // 8. Закрываем позицию по маркету (reduce_only = true, передаём размер)
//     let (close_cloid, close_price) = hl
//         .open_market_order(symbol, "Sell", 0.0, true, position_after.size)
//         .await?;
//     println!(
//         "Closed position: cloid = {}, avg_exit_price = {}",
//         close_cloid, close_price
//     );

//     Ok(())
// }
// src/main.rs

// mod params;
// mod open_position;
// mod get_info;
// mod swap;
// mod types;
// mod net;
// mod wsol_return;
// mod open_pos;

// use anyhow::Result;
// use dotenv::dotenv;
// use open_position::{open_position, add_liquidity};
// use params::{POOLS, RANGE_HALF};

// #[tokio::main]
// async fn main() -> Result<()> {
//     dotenv().ok();
//     tracing_subscriber::fmt::init();

//     let pool = &POOLS[0];

//     // 1. Открываем позицию и сразу кладём начальную ликвидность
//     open_position(pool, RANGE_HALF, RANGE_HALF).await?;

//     // // 2. При необходимости докидываем ещё
//     // add_liquidity(pool, pool.initial_amount_usdc / 2.0).await?;

//     Ok(())
// }
// open_position.rs -----------------------------------------------------------

mod params;

use anyhow::{anyhow, Result};
use std::{sync::Arc, str::FromStr, time::Duration};

use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    hash::Hash,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    program_pack::Pack,
    signature::{Keypair, Signature, Signer, read_keypair_file},
    system_instruction,
    transaction::Transaction,
};
use spl_token::state::Mint;

use orca_whirlpools_client::Whirlpool;
use orca_whirlpools::{
    open_position_instructions,
    set_whirlpools_config_address,
    IncreaseLiquidityParam, WhirlpoolsConfigInput,
};
use orca_whirlpools_core::{sqrt_price_to_price, U128};

/// ───── параметры, которые вы чаще всего меняете ─────
const RPC_URL: &str           = "https://api.mainnet-beta.solana.com";
const WALLET_PATH: &str       = params::KEYPAIR_FILENAME;          // ваш keypair
const WHIRLPOOL: &str         = "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e"; // WSOL/USDC
const PCT_RANGE: f64          = 0.01;          // ±1 %
const DEPOSIT_SOL: u64        = 10_000_000;    // 0.01 SOL
const SLIPPAGE_BPS: u16       = 5_000;         // 50 %
/// ──────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    /* 1. RPC-клиент и конфиг Whirlpool-SDK */
    set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
    .map_err(|e| anyhow!("set_whirlpools_config_address: {}", e))?;
    let rpc = Arc::new(RpcClient::new_with_commitment(
        RPC_URL.to_string(),
        CommitmentConfig::confirmed(),
    ));

    /* 2. Wallet */
    let wallet = read_keypair_file(WALLET_PATH)
        .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
    let wallet_pk = wallet.pubkey();

    /* 3. Читаем аккаунт пула и decimals токенов */
    let whirl_pk = Pubkey::from_str(WHIRLPOOL)?;
    let whirl_acct = rpc.get_account(&whirl_pk).await?;
    let whirl = Whirlpool::from_bytes(&whirl_acct.data)?;
    let dec_a = Mint::unpack(&rpc.get_account(&whirl.token_mint_a).await?.data)?.decimals;
    let dec_b = Mint::unpack(&rpc.get_account(&whirl.token_mint_b).await?.data)?.decimals;

    /* 4. Границы цен */
    let price_now  = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);
    let price_low  = price_now * (1.0 - PCT_RANGE);
    let price_high = price_now * (1.0 + PCT_RANGE);

    /* 5. Запрашиваем инструкции SDK (TickArray+Open+Increase) */
    let quote = open_position_instructions(
        &rpc,
        whirl_pk,
        price_low,
        price_high,
        IncreaseLiquidityParam::TokenA(DEPOSIT_SOL),
        Some(SLIPPAGE_BPS),
        Some(wallet_pk),
    )
    .await
    .map_err(|e| anyhow!("open_position_instructions: {}", e))?;

    /* 6. Собираем все keypair-подписанты */
    let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + quote.additional_signers.len());
    signers.push(&wallet);
    signers.extend(quote.additional_signers.iter());

    /* 7. Добавляем Compute-Budget (400 000 CU) перед набором SDK */
    let mut ixs: Vec<Instruction> = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(400_000),
    ];
    ixs.extend(quote.instructions);

    /* 8. Формируем и подписываем Transaction */
    let recent: Hash = rpc.get_latest_blockhash().await?;
    let message = Message::new(&ixs, Some(&wallet_pk));
    let mut tx = Transaction::new_unsigned(message);
    tx.try_sign(&signers, recent)?;

    /* 9. Отправляем raw-tx и ждём подтверждения */
    let sig: Signature = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentConfig::processed().commitment),
                ..RpcSendTransactionConfig::default()
            },
        )
        .await?;

    println!("⏳ tx sent {sig} — waiting for confirmation…");

    // простой поллинг: 40 сек × 1 сек
    for _ in 0..40 {
        if let Some(status_res) = rpc.get_signature_status(&sig).await? {
            // транзакция найдена в стейтусах — разбираем результат
            match status_res {
                Ok(()) => {
                    println!("✅ позиция создана! tx = {}", sig);
                    return Ok(());
                }
                Err(err) => {
                    return Err(anyhow!("Transaction failed: {:?}", err));
                }
            }
        }
        // ещё не в стейтусах — ждём секунду
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!("Timeout: transaction {sig} not confirmed within 40 s"))
}


// pub mod params;
// pub mod wirlpool;
// use orca_whirlpools::{
//     close_position_instructions, set_whirlpools_config_address, WhirlpoolsConfigInput,
// };

// use solana_client::nonblocking::rpc_client::RpcClient;
// use solana_sdk::pubkey::Pubkey;
// use std::str::FromStr;
// use solana_sdk::signature::Signer;
// use orca_tx_sender::{
//     build_and_send_transaction,
//     set_rpc, get_rpc_client
// };
// use solana_sdk::commitment_config::CommitmentLevel;
// use solana_sdk::{
//     signature::{read_keypair_file, Keypair},

// };

// use anyhow::anyhow;

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     set_rpc("https://api.mainnet-beta.solana.com").await?;
//     set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet).unwrap();
//     let wallet = read_keypair_file(params::KEYPAIR_FILENAME)
//     .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
//     let rpc = get_rpc_client()?;

//     let position_mint_address =
//     Pubkey::from_str("EcK84YxsFTDPtTce72YATQhZbZiwfjxVxgRr381UcDAz").unwrap();

//     let result = close_position_instructions(
//         &rpc,
//         position_mint_address,
//         Some(100),
//         Some(wallet.pubkey()),
//     )
//     .await?;

//     // Собираем список подписантов конкретного типа &Keypair
//     let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + result.additional_signers.len());
//     signers.push(&wallet);
//     for kp in &result.additional_signers {
//         signers.push(kp);
//     }

//     println!("Quote token max B: {:?}", result.quote.token_est_b);
//     println!("Fees Quote: {:?}", result.fees_quote);
//     println!("Rewards Quote: {:?}", result.rewards_quote);
//     println!("Number of Instructions: {}", result.instructions.len());
//     println!("Signers count: {}", signers.len());

//     let signature = build_and_send_transaction(
//         result.instructions,
//         &signers,                             // <-- теперь &[&Keypair]
//         Some(CommitmentLevel::Confirmed),
//         None,                                 // No address lookup tables
//     )
//     .await?;

//     println!("Close position transaction sent: {}", signature);
//     Ok(())
// }