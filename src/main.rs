// src/main.rs

use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use anchor_client::{Client, Cluster};
use raydium_amm_v3::{
    ID as CLMM_PROGRAM_ID,
    instruction::{increase_liquidity, decrease_liquidity, collect_fees},
    util::tick_math::{price_to_tick, tick_to_price},
    state::PoolState,
};
use solana_account_decoder::UiAccountData;
use solana_client::{
    rpc_client::RpcClient,
    rpc_filter::RpcTokenAccountsFilter,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{read_keypair_file, Keypair},
    pubkey::Pubkey,
    transaction::Transaction,
};
use tokio::time::interval;

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Инициализация клиента и ключа
    let keypair_path = dirs::home_dir().unwrap()
        .join(".config/solana/mainnet-id.json");
    let keypair: Keypair = read_keypair_file(&keypair_path)?;
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let rpc_client = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let client = Client::new_with_options(
        Cluster::Custom(rpc_url.to_string(), rpc_url.to_string()),
        keypair.clone(),
        CommitmentConfig::confirmed(),
    );
    let program = client.program(CLMM_PROGRAM_ID);

    // 2) Адрес CLMM-пула WSOL/USDC
    let pool_pubkey = Pubkey::from_str("3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv")?;

    // 3) Настройки диапазона: ±0.25% (в сумме 0.5%)
    const RANGE_HALF: f64 = 0.0025;

    // 4) Переменные для состояния позиции
    let mut has_position = false;
    let mut lower_tick: i32 = 0;
    let mut upper_tick: i32 = 0;

    // 5) Запускаем цикл раз в 30 сек
    let mut ticker = interval(Duration::from_secs(30));
    loop {
        ticker.tick().await;

        // 6) Читаем состояние пула
        let pool_state: PoolState = program.account(pool_pubkey)?;
        let current_price: f64 = pool_state.price();      // цена USDC за WSOL
        let tick_spacing: u16 = pool_state.tick_spacing as u16;

        // 7) Рассчитываем новые границы
        let lower_price = current_price * (1.0 - RANGE_HALF);
        let upper_price = current_price * (1.0 + RANGE_HALF);
        let new_lower_tick = price_to_tick(lower_price, tick_spacing)?;
        let new_upper_tick = price_to_tick(upper_price, tick_spacing)?;

        // 8) Если у нас ещё нет позиции — делаем initial deposit
        if !has_position {
            let amount_usdc: u128 = 400_u64 * 1_000_000_u128; // 400 USDC (6 dec)
            let amount_wsol: u128 = ((400.0 / current_price) * 1e9) as u128;

            let increase_ix = increase_liquidity(
                &CLMM_PROGRAM_ID,
                &pool_pubkey,
                &program.payer(),
                new_lower_tick,
                new_upper_tick,
                amount_usdc,
                amount_wsol,
            );

            let mut tx = Transaction::new_with_payer(
                &[increase_ix],
                Some(&program.payer()),
            );
            let (recent_hash, _) = rpc_client.get_latest_blockhash()?;
            tx.sign(&[&keypair], recent_hash);
            let sig = rpc_client
                .send_and_confirm_transaction(&tx)?;
            println!("Initial liquidity deposited at price {:.6}, tx={}", current_price, sig);

            // Запоминаем позицию
            lower_tick = new_lower_tick;
            upper_tick = new_upper_tick;
            has_position = true;
            continue;
        }

        // 9) Проверяем: вышла ли цена за старый диапазон?
        let old_lower_price = tick_to_price(lower_tick, tick_spacing)?;
        let old_upper_price = tick_to_price(upper_tick, tick_spacing)?;
        if current_price < old_lower_price || current_price > old_upper_price {
            println!("Price {:.6} out of [{:.6} – {:.6}], rebalancing…",
                     current_price, old_lower_price, old_upper_price);

            // 10) Снимаем старую позицию и собираем комиссии
            let decrease_ix = decrease_liquidity(
                &CLMM_PROGRAM_ID,
                &pool_pubkey,
                &program.payer(),
                lower_tick,
                upper_tick,
                u128::MAX,
            );
            let collect_ix = collect_fees(
                &CLMM_PROGRAM_ID,
                &pool_pubkey,
                &program.payer(),
            );

            // 11) Считаем текущие остатки USDC и WSOL
            let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSS3ZQSijzCetra7T1Y9NPpF7Rmgv8dZ")?;
            let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
            let balance_usdc = get_token_balance(&rpc_client, &keypair.pubkey(), &usdc_mint)?;
            let balance_wsol = get_token_balance(&rpc_client, &keypair.pubkey(), &wsol_mint)?;

            // Общая стоимость в USDC (*1e6 для ламинатов)
            let total_value = (balance_usdc as f64)
                + (balance_wsol as f64) * current_price * (1e6 / 1e9);

            // Целевая половина (в лампортах USDC и в вастах WSOL)
            let target_usdc = (total_value / 2.0).round() as u128;
            let target_wsol = ((total_value / 2.0) / current_price * 1e9).round() as u128;

            // 12) Делаем swap, чтобы получить ровно target_usdc и target_wsol
            // Здесь используем CLMM-swap инструкцию:
            // Если у нас USDC слишком много — меняем USDC→WSOL, иначе WSOL→USDC.
            let swap_ix = if (balance_usdc as u128) > target_usdc {
                // USDC→WSOL
                // В качестве примера: весь излишек меняем
                let amount_in = (balance_usdc as u128) - target_usdc;
                raydium_amm_v3::instruction::swap(
                    &CLMM_PROGRAM_ID,
                    &pool_pubkey,
                    &program.payer(),
                    amount_in,
                    0, // не ставим лимит
                )
            } else {
                // WSOL→USDC
                let amount_in = (balance_wsol as u128) - target_wsol;
                raydium_amm_v3::instruction::swap(
                    &CLMM_PROGRAM_ID,
                    &pool_pubkey,
                    &program.payer(),
                    amount_in,
                    0,
                )
            };

            // 13) Вносим обновлённую позицию
            let increase_ix = increase_liquidity(
                &CLMM_PROGRAM_ID,
                &pool_pubkey,
                &program.payer(),
                new_lower_tick,
                new_upper_tick,
                target_usdc,
                target_wsol,
            );

            // 14) Собираем все инструкции и шлём
            let mut tx = Transaction::new_with_payer(
                &[decrease_ix, collect_ix, swap_ix, increase_ix],
                Some(&program.payer()),
            );
            let (recent_hash, _) = rpc_client.get_latest_blockhash()?;
            tx.sign(&[&keypair], recent_hash);
            let sig = rpc_client
                .send_and_confirm_transaction(&tx)?;
            println!("Rebalanced at price {:.6}, tx={}", current_price, sig);

            // Обновляем состояние
            lower_tick = new_lower_tick;
            upper_tick = new_upper_tick;
        }
        // иначе — price внутри диапазона, ждём следующей итерации
    }
}

/// Возвращает баланс SPL-токена (в минимальных единицах, UInt64)
fn get_token_balance(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    use solana_account_decoder::UiAccountEncoding;
    use solana_client::rpc_config::RpcAccountInfoConfig;

    let resp = rpc.get_token_accounts_by_owner(
        owner,
        RpcTokenAccountsFilter::Mint(*mint),
    )?;
    if resp.is_empty() {
        return Ok(0);
    }
    // Берём первый associated-аккаунт
    let data = &resp[0].1.data;
    if let UiAccountData::Json(parsed) = data {
        let ui_amount = parsed.token_amount.ui_amount.unwrap_or(0.0);
        let decimals = parsed.token_amount.decimals;
        let lamports = (ui_amount * 10u64.pow(decimals as u32) as f64).round();
        Ok(lamports as u64)
    } else {
        Ok(0)
    }
}
