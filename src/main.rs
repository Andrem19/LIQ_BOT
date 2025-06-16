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


// pub mod params;
// pub mod exchange;
// pub mod types;
// pub mod telegram_service;
// pub mod wirlpool_services;


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
//     sleep(Duration::from_secs(1)).await;
//     hl.open_tp(symbol, "Sell", position.size, position.entry_px, 0.01)
//     .await?;
//     println!("Take-profit set at 1%");

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

// mod params;
// mod wirlpool;
// use anyhow::{anyhow, Result};
// use std::{sync::Arc, str::FromStr, time::Duration};

// use solana_client::{
//     nonblocking::rpc_client::RpcClient,
//     rpc_config::RpcSendTransactionConfig,
// };
// use solana_sdk::{
//     commitment_config::CommitmentConfig,
//     hash::Hash,
//     instruction::Instruction,
//     message::Message,
//     pubkey::Pubkey,
//     program_pack::Pack,
//     signature::{Keypair, Signature, Signer, read_keypair_file},
//     system_instruction,
//     transaction::Transaction,
// };
// use spl_token::state::Mint;

// use orca_whirlpools_client::Whirlpool;
// use orca_whirlpools::{
//     open_position_instructions,
//     set_whirlpools_config_address,
//     IncreaseLiquidityParam, WhirlpoolsConfigInput,
// };
// use orca_whirlpools_core::{sqrt_price_to_price, U128};

// /// ───── параметры, которые вы чаще всего меняете ─────
// const RPC_URL: &str           = "https://api.mainnet-beta.solana.com";
// const WALLET_PATH: &str       = params::KEYPAIR_FILENAME;          // ваш keypair
// const WHIRLPOOL: &str         = "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e"; // WSOL/USDC
// const PCT_RANGE: f64          = 0.01;          // ±1 %
// const DEPOSIT_SOL: u64        = 10_000_000;    // 0.01 SOL
// const SLIPPAGE_BPS: u16       = 5_000;         // 50 %
// /// ──────────────────────────────────────────────────────

// #[tokio::main]
// async fn main() -> Result<()> {
//     /* 1. RPC-клиент и конфиг Whirlpool-SDK */
//     set_whirlpools_config_address(WhirlpoolsConfigInput::SolanaMainnet)
//     .map_err(|e| anyhow!("set_whirlpools_config_address: {}", e))?;
//     let rpc = Arc::new(RpcClient::new_with_commitment(
//         RPC_URL.to_string(),
//         CommitmentConfig::confirmed(),
//     ));

//     /* 2. Wallet */
//     let wallet = read_keypair_file(WALLET_PATH)
//         .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
//     let wallet_pk = wallet.pubkey();

//     /* 3. Читаем аккаунт пула и decimals токенов */
//     let whirl_pk = Pubkey::from_str(WHIRLPOOL)?;
//     let whirl_acct = rpc.get_account(&whirl_pk).await?;
//     let whirl = Whirlpool::from_bytes(&whirl_acct.data)?;
//     let dec_a = Mint::unpack(&rpc.get_account(&whirl.token_mint_a).await?.data)?.decimals;
//     let dec_b = Mint::unpack(&rpc.get_account(&whirl.token_mint_b).await?.data)?.decimals;

//     /* 4. Границы цен */
//     let price_now  = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);
//     let price_low  = price_now * (1.0 - PCT_RANGE);
//     let price_high = price_now * (1.0 + PCT_RANGE);

//     /* 5. Запрашиваем инструкции SDK (TickArray+Open+Increase) */
//     let quote = open_position_instructions(
//         &rpc,
//         whirl_pk,
//         price_low,
//         price_high,
//         IncreaseLiquidityParam::TokenA(DEPOSIT_SOL),
//         Some(SLIPPAGE_BPS),
//         Some(wallet_pk),
//     )
//     .await
//     .map_err(|e| anyhow!("open_position_instructions: {}", e))?;

//     /* 6. Собираем все keypair-подписанты */
//     let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + quote.additional_signers.len());
//     signers.push(&wallet);
//     signers.extend(quote.additional_signers.iter());

//     /* 7. Добавляем Compute-Budget (400 000 CU) перед набором SDK */
//     let mut ixs: Vec<Instruction> = vec![
//         solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(400_000),
//     ];
//     ixs.extend(quote.instructions);

//     /* 8. Формируем и подписываем Transaction */
//     let recent: Hash = rpc.get_latest_blockhash().await?;
//     let message = Message::new(&ixs, Some(&wallet_pk));
//     let mut tx = Transaction::new_unsigned(message);
//     tx.try_sign(&signers, recent)?;

//     /* 9. Отправляем raw-tx и ждём подтверждения */
//     let sig: Signature = rpc
//         .send_transaction_with_config(
//             &tx,
//             RpcSendTransactionConfig {
//                 skip_preflight: false,
//                 preflight_commitment: Some(CommitmentConfig::processed().commitment),
//                 ..RpcSendTransactionConfig::default()
//             },
//         )
//         .await?;

//     println!("⏳ tx sent {sig} — waiting for confirmation…");

//     // простой поллинг: 40 сек × 1 сек
//     for _ in 0..40 {
//         if let Some(status_res) = rpc.get_signature_status(&sig).await? {
//             // транзакция найдена в стейтусах — разбираем результат
//             match status_res {
//                 Ok(()) => {
//                     println!("✅ позиция создана! tx = {}", sig);
//                     return Ok(());
//                 }
//                 Err(err) => {
//                     return Err(anyhow!("Transaction failed: {:?}", err));
//                 }
//             }
//         }
//         // ещё не в стейтусах — ждём секунду
//         tokio::time::sleep(Duration::from_secs(1)).await;
//     }
//     Err(anyhow!("Timeout: transaction {sig} not confirmed within 40 s"))
// }


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
// src/main.rs
// src/main.rs

// pub mod types;
// pub mod params;
// pub mod wirlpool_services;
// pub mod telegram_service;
// pub mod exchange;

// use anyhow::{anyhow, Result};
// use std::{env, str::FromStr, sync::Arc};
// use solana_client::nonblocking::rpc_client::RpcClient;
// use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
// use spl_token::state::Mint;
// use spl_token::solana_program::program_pack::Pack;
// use orca_whirlpools_client::Whirlpool;
// use orca_whirlpools_core::{U128, sqrt_price_to_price};

// use crate::wirlpool_services::wirlpool::{open_with_funds_check, open_whirlpool_position, close_whirlpool_position, harvest_whirlpool_position, summarize_harvest_fees, HarvestSummary};
// use crate::params::POOL;
// use crate::types::PoolConfig;

// /// Ширина диапазона ±1%
// const PCT_RANGE: f64 = 0.01;

// /// Инициализирует RPC-клиент из .env-переменных
// fn init_rpc() -> Arc<RpcClient> {
//     let url = env::var("HELIUS_HTTP")
//         .or_else(|_| env::var("QUICKNODE_HTTP"))
//         .or_else(|_| env::var("ANKR_HTTP"))
//         .or_else(|_| env::var("CHAINSTACK_HTTP"))
//         .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
//     Arc::new(RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()))
// }

// /// Асинхронно получает текущую цену и диапазон [low, high] для пула
// async fn get_price_bounds(
//     rpc: &Arc<RpcClient>,
//     pool: &PoolConfig,
// ) -> Result<(f64, f64), Box<dyn std::error::Error>> {
//     let whirl_pk = Pubkey::from_str(pool.pool_address)?;
//     let acct = rpc.get_account(&whirl_pk).await?;
//     let whirl = Whirlpool::from_bytes(&acct.data)?;
//     // читаем decimals A и B
//     let mint_a_acct = rpc.get_account(&whirl.token_mint_a).await?;
//     let dec_a = Mint::unpack(&mint_a_acct.data)?.decimals;
//     let mint_b_acct = rpc.get_account(&whirl.token_mint_b).await?;
//     let dec_b = Mint::unpack(&mint_b_acct.data)?.decimals;
//     // вычисляем цену и диапазон
//     let mut price_now = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);
//     // price_now = 143.00;
//     Ok((price_now * (1.0 - PCT_RANGE), price_now * (1.0 + PCT_RANGE)))
// }

// /// Точка входа. Раскомментируйте нужный тест.
// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     let pool = POOL.clone();
//     test_open().await?;
//     // test_harvest(pool.clone(), "Cdafq3j7Bo2Zy3MGCqgRAAVHRtSramMfBbFPsvaEMdB5").await?;
//     // test_close("GqoZ9UQ89S5auCgnovkgV5ywemyCVaJnHWEhnuPgqYco").await?;
//     Ok(())
// }

// /// Тест открытия позиции
// async fn test_open() -> Result<(), Box<dyn std::error::Error>> {
//     let rpc = init_rpc();
//     let pool = POOL.clone();
//     println!("=== Testing OPEN on pool {} ({}) ===", pool.name, pool.pool_address);

//     let (price_low, price_high) = get_price_bounds(&rpc, &pool).await?;
//     println!("Price bounds: [{:.6}, {:.6}]", price_low, price_high);

//     let position_mint = open_with_funds_check(price_low, price_high, 30.0, pool)
//         .await
//         .map_err(|e| format!("open_whirlpool_position failed: {}", e))?;
//     println!("✅ Opened position, NFT mint = {}", position_mint);
//     Ok(())
// }

// /// Тест сбора комиссий (harvest)
// async fn test_harvest(pool: PoolConfig, mint_str: &str) -> Result<()> {
//     let position_mint = Pubkey::from_str(mint_str)?;
//     println!(
//         "=== Testing HARVEST on position {} for pool {} ({}) ===",
//         position_mint, pool.name, pool.pool_address
//     );

//     // 1) Получаем сырые минимальные единицы
//     let fees = harvest_whirlpool_position(position_mint)
//         .await
//         .map_err(|e| anyhow::anyhow!("harvest_whirlpool_position failed: {}", e))?;

//     // 2) Конвертируем в читаемые суммы и считаем USD-стоимость
//     let summary: HarvestSummary = summarize_harvest_fees(&pool, &fees)
//         .await
//         .map_err(|e| anyhow::anyhow!("summarize_harvest_fees failed: {}", e))?;

//     // 3) Печатаем результаты
//     println!("✅ Harvested fees:");
//     println!(
//         "   • {:.6} {} (raw = {})",
//         summary.amount_a, pool.mint_a, fees.fee_owed_a
//     );
//     println!(
//         "   • {:.6} {} (raw = {})",
//         summary.amount_b, pool.mint_b, fees.fee_owed_b
//     );
//     println!(
//         "   • Price of {} → {} = {:.6}",
//         pool.mint_a, pool.mint_b, summary.price_a_in_usd
//     );
//     println!(
//         "   • Total value = {:.6} {}",
//         summary.total_usd, pool.mint_b
//     );

//     Ok(())
// }


// /// Тест закрытия позиции
// async fn test_close(mint_str: &str) -> Result<(), Box<dyn std::error::Error>> {
//     let position_mint = Pubkey::from_str(mint_str)?;
//     println!("=== Testing CLOSE on position {} ===", position_mint);

//     close_whirlpool_position(position_mint)
//         .await
//         .map_err(|e| format!("close_whirlpool_position failed: {}", e))?;
//     println!("✅ Closed position {}", position_mint);
//     Ok(())
// }
// src/main.rs (или отдельный файл launcher.rs)


// mod types;
// pub mod exchange;
// mod params;
// mod wirlpool_services;
// mod telegram_service;
// mod orca_logic;

// use std::{thread, time::Duration};
// use chrono::Local;
// use telegram_service::tl_engine::{start, ServiceCommand};
// use dotenv::dotenv;

// #[tokio::main]
// async fn main() {
//     // 0. Опционально: подгружаем .env (TELEGRAM_API, CHAT_ID, HLSECRET_1, HL_TRADING_ADDRESS и т.д.)
//     dotenv().ok();

//     println!(
//         "[{}][INFO] Запуск LIQ_BOT: инициализация Telegram engine...",
//         Local::now().format("%Y-%m-%d %H:%M:%S")
//     );

//     // 1. Стартуем движок Telegram: он внутри сделает
//     //    - init Commander (Arc<Commander>)
//     //    - register_commands(...)
//     //    - spawn thread, который в цикле читает getUpdates и вызывает exec_command()
//     let (tx, _commander) = start();

//     println!(
//         "[{}][INFO] Telegram engine успешно запущен. Команды зарегистрированы.",
//         Local::now().format("%Y-%m-%d %H:%M:%S")
//     );

//     // 2. Шлём стартовое уведомление в чат
//     let startup_msg = format!(
//         "✅ LIQ_BOT успешно запущен в {}",
//         Local::now().format("%Y-%m-%d %H:%M:%S")
//     );
//     if tx.send(ServiceCommand::SendMessage(startup_msg)).is_ok() {
//         println!(
//             "[{}][DEBUG] Стартовое сообщение отправлено в Telegram.",
//             Local::now().format("%Y-%m-%d %H:%M:%S")
//         );
//     } else {
//         eprintln!(
//             "[{}][ERROR] Не удалось отправить стартовое сообщение в Telegram.",
//             Local::now().format("%Y-%m-%d %H:%M:%S")
//         );
//     }

//     // 3. Основной «таймерный» цикл приложения
//     //    Если вам не нужна дополнительная логика, можно просто спать.
//     println!(
//         "[{}][INFO] LIQ_BOT полностью запущен. Ожидание команд в Telegram…",
//         Local::now().format("%Y-%m-%d %H:%M:%S")
//     );
//     loop {
//         thread::sleep(Duration::from_secs(60));
//         println!(
//             "[{}][DEBUG] LIQ_BOT alive…",
//             Local::now().format("%Y-%m-%d %H:%M:%S")
//         );
//     }
// }

// pub mod params;
// pub mod exchange;
// pub mod types;
// pub mod orca_logic;
// pub mod telegram_service;
// pub mod wirlpool_services;
// use tokio::time::sleep;
// use std::{str::FromStr, time::Duration};
// use types::OpenPositionResult;
// use orca_whirlpools_client::DecodedAccount;
// use orca_whirlpools::PositionOrBundle;

// use anyhow::anyhow;
// use solana_client::nonblocking::rpc_client::RpcClient;
// use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
// use spl_token::state::Mint;
// use spl_token::solana_program::program_pack::Pack;
// use orca_whirlpools_core::{U128, sqrt_price_to_price};
// use orca_whirlpools_client::Whirlpool;
// use dotenv::dotenv;
// use env_logger::Env;
// use log::LevelFilter;

// use params::{PCT_LIST, WEIGHTS, TOTAL_USDC, POOL};
// use orca_logic::helpers::{calc_bound_prices_struct, calc_range_allocation_struct};
// use crate::wirlpool_services::wirlpool::open_with_funds_check;

// #[tokio::main]
// async fn main() -> anyhow::Result<()> {
//     dotenv().ok();

//     env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
//     .init();
//     // 1) Создаём асинхронный RPC‐клиент к Mainnet
//     let rpc = RpcClient::new_with_commitment(
//         "https://api.mainnet-beta.solana.com".to_string(),
//         CommitmentConfig::confirmed(),
//     );

//     // 2) Парсим Pubkey пула и читаем его on‐chain аккаунт
//     let whirl_pk = Pubkey::from_str(&POOL.pool_address)?;
//     let acct    = rpc.get_account(&whirl_pk).await?;
//     let whirl   = Whirlpool::from_bytes(&acct.data)?;

//     // 3) Получаем decimals для токенов A и B
//     let mint_a_acct = rpc.get_account(&whirl.token_mint_a).await?;
//     let da = Mint::unpack(&mint_a_acct.data)?.decimals as i32;
//     let mint_b_acct = rpc.get_account(&whirl.token_mint_b).await?;
//     let db = Mint::unpack(&mint_b_acct.data)?.decimals as i32;

//     // 4) Вычисляем текущую цену SOL→USDC из sqrt_price
//     let price = sqrt_price_to_price(U128::from(whirl.sqrt_price), da as u8, db as u8);
//     println!("Current pool price: {:.6}", price);

//     // 5) Строим границы и вычисляем аллокацию
//     let bounds = calc_bound_prices_struct(price, &PCT_LIST);
//     let allocs = calc_range_allocation_struct(price, &bounds, &WEIGHTS, TOTAL_USDC);

//     println!("{:?}", allocs);

//     // 6) Печатаем результат
//     // let upper = allocs[0].clone();
//     // let position_upper: OpenPositionResult = open_with_funds_check(upper.lower_price, upper.upper_price, upper.usdc_equivalent, POOL).await
//     // .map_err(op("open_with_funds_check 1"))?;

//     // sleep(Duration::from_millis(1500)).await;

//     // let middle = allocs[1].clone();
//     // let position_middle: OpenPositionResult = open_with_funds_check(middle.lower_price, middle.upper_price, middle.usdc_amount, POOL).await
//     // .map_err(op("open_with_funds_check 2"))?;

//     // sleep(Duration::from_millis(1500)).await;

//     // let lower = allocs[2].clone();
//     // let position_lower: OpenPositionResult = open_with_funds_check(lower.lower_price, lower.upper_price, lower.usdc_equivalent, POOL).await
//     // .map_err(op("open_with_funds_check 2"))?;


//     // 2) Все позиции в конкретном пуле POOL
//     let my_positions = wirlpool_services::wirlpool::list_positions_for_owner().await?;
//     for p in &my_positions {
//         match p {
//             PositionOrBundle::Position(hp) => {
//                 println!("Single position:");
//                 println!("  address: {}", hp.address);
//                 println!("  pool:    {}", hp.data.whirlpool);
//                 println!("  mint:    {}", hp.data.position_mint);
//                 println!("  liquidity: {}", hp.data.liquidity);
//                 println!("  ticks: [{} .. {}]",
//                     hp.data.tick_lower_index, hp.data.tick_upper_index);
//                 println!("  fees A/B owed: {}/{}",
//                     hp.data.fee_owed_a, hp.data.fee_owed_b);
//             }
//             PositionOrBundle::PositionBundle(hb) => {
//                 println!("Bundled position:");
//                 println!("  bundle account: {}", hb.address);
//                 println!("  inner positions:");
//                 for inner in &hb.positions {
//                     println!("    - {}", inner.address);
//                 }
//             }
//         }
//     }
//     // wirlpool_services::wirlpool::close_all_positions().await?;

//     Ok(())
// }

// fn op<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> anyhow::Error {
//     move |e| anyhow!("{} failed: {}", ctx, e)
// }


mod params;
mod types;
mod orca_logic;
mod telegram_service;
mod wirlpool_services;
mod exchange;
mod orchestrator;

use anyhow::Result;
use dotenv::dotenv;

use std::time::Duration;
use tokio::time::sleep;
use env_logger::Env;

use crate::telegram_service::tl_engine::ServiceCommand;


#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    // env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
    // .init();

    // 1. start Telegram service
    let (tx_telegram, _commander) = telegram_service::tl_engine::start();

    // 2. start Hyperliquid hedge worker
    let tx_hl = exchange::hl_engine::start()?;

    // 3. orchestrator: infinite cycle
    loop {
        if let Err(e) = orchestrator::orchestrator_cycle(&tx_telegram, &tx_hl).await {
            eprintln!("[ERROR] orchestrator cycle failed: {:?}", e);
            let _ = tx_telegram.send(ServiceCommand::SendMessage(
                format!("❌ Bot error: {:?}", e)
            ));
        }
        // small pause before restarting cycle
        sleep(Duration::from_secs(10)).await;
    }
}