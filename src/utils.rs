use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use solana_sdk::instruction::Instruction;
use std::sync::atomic::AtomicUsize;
use anyhow::{anyhow, Result};
use crate::types::{RangeAlloc, PriceBound, BoundType};
use serde_json::Value;
use std::time::{Instant};
use crate::params::WSOL as SOL_MINT;
use crate::types::Role;
use crate::dex_services::net::http_client;
use once_cell::sync::Lazy;
use crate::params;
use std::sync::atomic::Ordering;
use anyhow::Context;
use tokio::time::Duration;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::account::Account;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
use crate::types::WalletBalanceInfo;
use crate::params::{WSOL, USDC};
use crate::dex_services::swap;
use std::str::FromStr;
use orca_tx_sender::Signer;
use orca_tx_sender::ComputeBudgetInstruction;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::message::Message;
use orca_tx_sender::CommitmentConfig;

pub static RPC_ROTATOR: Lazy<RpcRotator> = Lazy::new(RpcRotator::new);
static PRICE_CACHE: Lazy<tokio::sync::RwLock<(f64, Instant)>> =
    Lazy::new(|| tokio::sync::RwLock::new((0.0, Instant::now() - Duration::from_secs(60))));

pub fn op<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> anyhow::Error {
    move |e| anyhow!("{} failed: {}", ctx, e)
}

pub mod utils {
    use super::*;

    pub fn init_rpc() -> Arc<RpcClient> {
        RPC_ROTATOR.client()
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
        for _ in 0..18 {
            if let Some(status) = rpc.get_signature_status(&sig).await.map_err(op("get_signature_status"))? {
                status.map_err(|e| anyhow!("transaction failed: {:?}", e))?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(anyhow!("Timeout: tx {} not confirmed within 18s", sig))
    }
}

pub struct RpcRotator {
    urls:    Vec<String>,
    current: AtomicUsize,
}

impl RpcRotator {
    pub fn new() -> Self {
        let urls = [
            "HELIUS_HTTP",
            "QUICKNODE_HTTP",
            "ANKR_HTTP",
            "CHAINSTACK_HTTP",
        ]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .collect::<Vec<_>>();

        assert!(
            !urls.is_empty(),
            "ни одна из переменных RPC-енд-поинтов не задана",
        );

        RpcRotator {
            urls,
            current: AtomicUsize::new(0),
        }
    }

    pub fn client(&self) -> Arc<RpcClient> {
        let idx = self.current.load(Ordering::Relaxed);
        Arc::new(RpcClient::new_with_commitment(
            self.urls[idx].clone(),
            CommitmentConfig::confirmed(),
        ))
    }

    /// Сдвигаем указатель на следующий URL (на сетевой ошибке).
    pub fn rotate(&self) {
        let next = (self.current.load(Ordering::Relaxed) + 1) % self.urls.len();
        self.current.store(next, Ordering::Relaxed);
    }
}

fn is_net_error(e: &solana_client::client_error::ClientError) -> bool {
    let s = e.to_string();
    s.contains("dns error")          ||
    s.contains("timed out")          ||
    s.contains("connection closed")  ||
    s.contains("transport error")
}

pub async fn safe_get_account(
    rpc: &RpcClient,
    pk: &Pubkey,
) -> anyhow::Result<Account> {
    match rpc.get_account(pk).await {
        Ok(account) => Ok(account),
        Err(e) if is_net_error(&e) => {
            // на сетевой ошибке пробуем перейти на следующий RPC
            RPC_ROTATOR.rotate();
            Err(anyhow::anyhow!("RPC network error: {e}"))
        }
        Err(e) => Err(anyhow::anyhow!("RPC error: {e}")),
    }
}

pub async fn get_token_balance(
    rpc:    &RpcClient,
    wallet: &Pubkey,
    mint:   &str,
    dec:    u8,
) -> Result<f64> {
    if mint == WSOL {
        // SOL balance
        let lamports = rpc.get_balance(wallet).await?;
        Ok(lamports as f64 / 1e9)
    } else {
        // SPL-token balance
        let mint_pk = Pubkey::from_str(mint)?;
        let ata     = get_associated_token_address(wallet, &mint_pk);
        // Запрос возвращает Future<Result<UiTokenAmount, ClientError>>
        let ui = rpc
            .get_token_account_balance(&ata)
            .await
            .ok(); // если аккаунта нет — получим None

        let amount = ui
            .and_then(|b| b.amount.parse::<u64>().ok())
            .map(|atoms| atoms as f64 / 10f64.powi(dec as i32))
            .unwrap_or(0.0);

        Ok(amount)
    }
}

/// Возвращает вектор `(mint, balance)` только для тех токенов, у которых
/// баланс > 0 (учитывая их decimals).
pub async fn balances_for_mints(
    rpc:    &RpcClient,
    wallet: &Pubkey,
    mints:  &[(&'static str, u8)],
) -> Result<Vec<(&'static str, u8, f64)>> {
    let mut out = Vec::new();
    for (mint, dec) in mints {
        // ждём результат и сразу пробрасываем ошибку, если она есть
        let bal = get_token_balance(rpc, wallet, mint, *dec).await?;
        if bal > 0.0 {
            out.push((*mint, *dec, bal));
        }
    }
    Ok(out)
}

pub async fn sweep_dust_to_usdc(
    dust_mints: &[(&'static str, u8)],
) -> Result<String> {
    /* 1. сеть / кошелёк */
    let payer:  Keypair = utils::load_wallet()
        .map_err(|e| anyhow!("read_keypair_file failed: {}", e))?;
    let wallet: Pubkey  = payer.pubkey();
    let rpc   = utils::init_rpc();


    /* 2. узнаём балансы */
    let dust_balances = balances_for_mints(&rpc, &wallet, dust_mints).await?;

    if dust_balances.is_empty() {
        return Ok("✅ Пыль не найдена — балансы нулевые.".into());
    }

    /* 3. для каждого токена пытаемся сделать своп → USDC */
    let mut report = String::new();
    for (mint, dec, bal) in dust_balances {
        match swap::execute_swap_tokens(mint, USDC, bal).await {
            Ok(res) => {
                report.push_str(&format!(
                    "🔁 {mint}: {:.6} → USDC | остаток: {:.6} {mint}\n",
                    bal,
                    res.balance_sell,
                ));
            }
            Err(err) => {
                report.push_str(&format!(
                    "⚠️  {mint}: не удалось свопнуть ({err})\n",
                ));
            }
        }
    }

    Ok(if report.is_empty() {
        "✅ Свопы завершены, но изменений нет.".into()
    } else {
        report
    })
}

pub async fn swap_excess_to_usdc(
    mint: &str,
    dec:  u8,
    keep_amount: f64,
) -> Result<String> {
    use anyhow::{anyhow, Context};
    use tokio::time::{sleep, Duration};
    use solana_sdk::{signer::keypair::Keypair, pubkey::Pubkey};

    const MAX_ATTEMPTS   : u8  = 3;     // сколько раз пробуем своп
    const FEE_BUFFER_SOL : f64 = 0.10;  // базовый буфер SOL (увеличивается с каждой попыткой)

    // ─── инициализация ───────────────────────────────────────────────
    let payer : Keypair = utils::load_wallet()
        .map_err(|e| anyhow!("failed to load wallet: {}", e))?;
    let wallet: Pubkey  = payer.pubkey();
    let rpc             = utils::init_rpc();

    let mut last_err: Option<anyhow::Error> = None;

    // ─── цикл ретраев ────────────────────────────────────────────────
    for attempt in 1..=MAX_ATTEMPTS {
        log::debug!("swap_excess_to_usdc; attempt {}/{}", attempt, MAX_ATTEMPTS);

        // 1) актуальный баланс токена
        let balance = match get_token_balance(&rpc, &wallet, mint, dec).await {
            Ok(bal) => bal,
            Err(e)  => {
                last_err = Some(anyhow!("failed to get balance for {}: {}", mint, e));
                if attempt < MAX_ATTEMPTS {
                    log::info!(
                        "Attempt {}/{}: balance error: {}. Retry in 1 s …",
                        attempt, MAX_ATTEMPTS, e
                    );
                    sleep(Duration::from_secs(1)).await;
                    continue;
                } else {
                    break;
                }
            }
        };

        // 2) динамический буфер: 0.10 SOL, затем 0.20 SOL, затем 0.30 SOL …
        let dyn_buffer = if mint == WSOL {
            FEE_BUFFER_SOL * attempt as f64
        } else {
            0.0
        };

        // 3) своп не нужен или нечего продавать
        if balance <= keep_amount + dyn_buffer {
            return Ok(format!(
                "🔔 {} balance {:.6} ≤ keep {:.6} + buffer {:.3}. Swap not required.",
                mint, balance, keep_amount, dyn_buffer
            ));
        }

        // 4) к продаже отдаём всё «лишнее», но оставляем буфер
        let to_swap = balance - keep_amount - dyn_buffer;

        // 5) пробуем выполнить своп
        match swap::execute_swap_tokens(mint, USDC, to_swap).await {
            Ok(res) => {
                return Ok(format!(
                    "🔁 Swapped {:.6} {} → USDC.\n\
                     Buffered {:.3} SOL for fees.\n\
                     Remaining {} balance: {:.6}\n\
                     USDC received (approx): {:.6}",
                    to_swap,
                    mint,
                    dyn_buffer,
                    mint,
                    res.balance_sell,
                    res.balance_buy
                ));
            }
            Err(e) => {
                last_err = Some(anyhow!("swap failed for {}: {}", mint, e));
                if attempt < MAX_ATTEMPTS {
                    log::info!(
                        "Attempt {}/{}: swap error: {}. Retry in 1 s …",
                        attempt, MAX_ATTEMPTS, e
                    );
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("swap_excess_to_usdc failed")))
}





pub fn calc_bound_prices_struct(base_price: f64, pct_list: &[f64], compress: bool) -> Vec<PriceBound> {
    assert!(pct_list.len() == 4, "Нужно ровно 4 процента: [верх_внутр, низ_внутр, верх_экстр, низ_экстр]");

    // Верхняя внутренняя граница (немного выше рынка)
    let upper_inner = base_price * (1.0 + pct_list[0]);
    // Нижняя внутренняя граница (немного ниже рынка)
    let lower_inner = base_price * (1.0 - pct_list[1]);
    // Верхняя экстремальная (ещё выше)
    let upper_outer = if compress {base_price * (1.0 + pct_list[2])} else { upper_inner * (1.0 + pct_list[2])};
    // Нижняя экстремальная (ещё ниже)
    let lower_outer = if compress { base_price * (1.0 - pct_list[3]) } else { lower_inner * (1.0 - pct_list[3]) };

    vec![
        PriceBound { bound_type: BoundType::UpperOuter, value: upper_outer },
        PriceBound { bound_type: BoundType::UpperInner, value: upper_inner },
        PriceBound { bound_type: BoundType::LowerInner, value: lower_inner },
        PriceBound { bound_type: BoundType::LowerOuter, value: lower_outer },
    ]
}

pub fn calc_bound_prices_struct_for_two(base_price: f64, pct_list: &[f64]) -> Vec<PriceBound> {
    assert!(pct_list.len() == 4, "Нужно ровно 4 процента: [верх_внутр, низ_внутр, верх_экстр, низ_экстр]");

    // Верхняя внутренняя граница (немного выше рынка)
    let upper_inner = base_price * (1.0 + pct_list[0]);
    // Нижняя внутренняя граница (немного ниже рынка)
    let lower_inner = base_price * (1.0 - pct_list[1]);
    // Верхняя экстремальная (ещё выше)
    let upper_outer = base_price * (1.0 + pct_list[2]);
    // Нижняя экстремальная (ещё ниже)
    let lower_outer = base_price * (1.0 - pct_list[3]);

    vec![
        PriceBound { bound_type: BoundType::UpperOuter, value: upper_outer },
        PriceBound { bound_type: BoundType::UpperInner, value: upper_inner },
        PriceBound { bound_type: BoundType::LowerInner, value: lower_inner },
        PriceBound { bound_type: BoundType::LowerOuter, value: lower_outer },
    ]
}



pub fn calc_range_allocation_struct_for_two(
    price: f64,
    bounds: &[PriceBound],
    weights: &Vec<f64>,
    total_usdc: f64,
) -> Vec<RangeAlloc> {
    println!("total_usdc: {}", total_usdc);

    // ─── 1. Берём границы по типу ────────────────────────────────────
    let get = |t: BoundType| bounds.iter().find(|b| b.bound_type == t).unwrap().value;
    let upper_outer = get(BoundType::UpperOuter);
    let upper_inner = get(BoundType::UpperInner);
    let lower_inner = get(BoundType::LowerInner);
    let lower_outer = get(BoundType::LowerOuter);

    // ─── 2. Малый центральный диапазон (MiddleSmall) ────────────────────────
    let small_weight = total_usdc * weights[0] / 100.0;
    // используем inner-границы
    let (sqrt_p, sqrt_l_small, sqrt_u_small) =
        (price.sqrt(), lower_inner.sqrt(), upper_inner.sqrt());
    let span_small = sqrt_u_small - sqrt_l_small;
    let usdc_val_small    = small_weight * (sqrt_u_small - sqrt_p) / span_small;
    let sol_val_usd_small = small_weight * (sqrt_p - sqrt_l_small) / span_small;
    // ПРАВКА: умножаем на price (SOL за 1 USDC)
    let sol_amount_small  = sol_val_usd_small * price;

    let small = RangeAlloc {
        role: Role::MiddleSmall,
        range_idx: 0,
        usdc_amount: small_weight - usdc_val_small,
        sol_amount:  sol_amount_small,
        usdc_equivalent: small_weight,
        upper_price: upper_inner,
        lower_price: lower_inner,
    };

    // ─── 3. Большой центральный диапазон (Middle) ─────────────────────────
    let mid_weight = total_usdc * weights[1] / 100.0;
    // теперь используем outer-границы
    let (sqrt_p, sqrt_l_large, sqrt_u_large) =
        (price.sqrt(), lower_outer.sqrt(), upper_outer.sqrt());
    let span_large = sqrt_u_large - sqrt_l_large;
    let usdc_val_large    = mid_weight * (sqrt_u_large - sqrt_p) / span_large;
    let sol_val_usd_large = mid_weight * (sqrt_p - sqrt_l_large) / span_large;
    // ТАК ЖЕ умножаем на price
    let sol_amount_large  = sol_val_usd_large * price;

    let large = RangeAlloc {
        role: Role::Middle,
        range_idx: 1,
        usdc_amount: mid_weight - usdc_val_large,
        sol_amount:  sol_amount_large,
        usdc_equivalent: mid_weight,
        upper_price: upper_outer,
        lower_price: lower_outer,
    };

    println!("middle_small: {}   middle: {}", small.usdc_equivalent, large.usdc_equivalent);
    vec![small, large]
}


pub fn calc_range_allocation_struct(
    price: f64,
    bounds: &[PriceBound],
    weights: &Vec<f64>,
    total_usdc: f64,
    compress: bool
) -> Vec<RangeAlloc> {
    println!("total_usdc: {}", total_usdc);
    // ─── 1. Выбираем нужные границы ────────────────────────────────────
    let get = |t: BoundType| bounds.iter().find(|b| b.bound_type == t).unwrap().value;
    let upper_outer = get(BoundType::UpperOuter);
    let upper_inner = get(BoundType::UpperInner);
    let lower_inner = get(BoundType::LowerInner);
    let lower_outer = get(BoundType::LowerOuter);

    // ─── 2. Верхний диапазон (Role::Up) ─────────────────────────────────
    let up_weight = total_usdc * weights[0] / 100.0;
    let up_sol    = up_weight / price;
    let upper: RangeAlloc = RangeAlloc {
        role: Role::Up,                           // ✱ ИЗМЕНЕНО
        range_idx: 0,
        usdc_amount: 0.0,
        sol_amount: up_sol,
        usdc_equivalent: up_sol * price,
        upper_price: upper_outer,
        lower_price: if compress { price * 1.0001 } else { upper_inner },
    };

    // ─── 3. Центральный диапазон (Role::Middle) ────────────────────────
    let mid_weight = total_usdc * weights[1] / 100.0;
    let (sqrt_p, sqrt_l, sqrt_u) = (price.sqrt(), lower_inner.sqrt(), upper_inner.sqrt());
    let span = sqrt_u - sqrt_l;
    let usdc_val    = mid_weight * (sqrt_u - sqrt_p) / span;
    let sol_val_usd = mid_weight * (sqrt_p - sqrt_l) / span;
    let mid_sol     = sol_val_usd / price;

    let middle = RangeAlloc {
        role: Role::Middle,                       // ✱ ИЗМЕНЕНО
        range_idx: 1,
        usdc_amount: mid_weight - usdc_val,
        sol_amount:  mid_sol,
        usdc_equivalent: mid_weight,
        upper_price: upper_inner,
        lower_price: lower_inner,
    };

    // ─── 4. Нижний диапазон (Role::Down) ───────────────────────────────
    let down_weight = total_usdc * weights[2] / 100.0;
    let lower = RangeAlloc {
        role: Role::Down,                         // ✱ ИЗМЕНЕНО
        range_idx: 2,
        usdc_amount: down_weight,
        sol_amount: 0.0,
        usdc_equivalent: down_weight,
        upper_price: if compress { price * 0.9999 } else {lower_inner},
        lower_price: lower_outer,
    };
    println!("upper: {} middle: {} lower: {}", upper.usdc_equivalent, middle.usdc_equivalent, lower.usdc_equivalent);
    vec![upper, middle, lower]      // порядок гарантирован
}

pub async fn get_sol_price_usd(mint: &str, fallback: bool) -> Result<f64> {
    // -------- 0. быстрый взгляд в кэш -----------------------------------
    {
        let rd = PRICE_CACHE.read().await;
        if rd.1.elapsed() < Duration::from_secs(30) && rd.0 > 0.0 {
            return Ok(rd.0);
        }
    }

    let client = http_client(); // ваша вспомогательная функция (reqwest::Client)

    // -------- 1. пробуем Jupiter lite-api --------------------------------
    let url = format!(
        "https://lite-api.jup.ag/price/v2?ids={mint}"
    );
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            let body = resp.text().await?;
            let v: Value = serde_json::from_str(&body)
                .map_err(|e| anyhow!("Jupiter price JSON parse error: {e}  body={body}"))?;
            if let Some(p_str) = v["data"][SOL_MINT]["price"].as_str() {
                if let Ok(price) = p_str.parse::<f64>() {
                    cache_price(price).await;
                    return Ok(price);
                }
            }
        }
    }
    if fallback {
        // -------- 2. fallback → CoinGecko ------------------------------------
        let cg_url = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";
        let resp = client.get(cg_url).send().await?;
        let body = resp.text().await?;
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("CoinGecko JSON parse error: {e}  body={body}"))?;
        if let Some(price) = v["solana"]["usd"].as_f64() {
            cache_price(price).await;
            return Ok(price);
        }
    }

    Err(anyhow!("cannot fetch SOL/USD price from Jupiter or CoinGecko"))
}

/// сохраняем значение в кэше
async fn cache_price(p: f64) {
    let mut wr = PRICE_CACHE.write().await;
    *wr = (p, Instant::now());
}

/// Асинхронно собирает баланс SOL и USDC и конвертирует в USD.
pub async fn fetch_wallet_balance_info() -> Result<WalletBalanceInfo> {
    // 1) RPC и кошелёк
    let rpc    = utils::init_rpc();
    let wallet = utils::load_wallet().context("Failed to load wallet")?;
    let pk     = wallet.pubkey();

    // 2) SOL баланс
    let lam = rpc
        .get_balance(&pk)
        .await
        .context("Error fetching SOL balance")?;
    let sol_balance = lam as f64 / 1e9;

    // 3) USDC баланс
    let mint_usdc = Pubkey::from_str(USDC)
        .context("Invalid USDC mint")?;
    let ata = get_associated_token_address(&pk, &mint_usdc);

    let usdc_balance = rpc
        .get_token_account_balance(&ata)
        .await
        .ok()
        .and_then(|resp| resp.amount.parse::<f64>().ok())
        .unwrap_or(0.0) / 1e6;

    // 4) Цена SOL → USD
    let sol_usd_price = get_sol_price_usd(WSOL, true)
        .await
        .context("Error fetching SOL/USD price")?;

    // 5) Конвертация в доллары
    let sol_in_usd   = sol_balance * sol_usd_price;
    let usdc_in_usd  = usdc_balance; // USDC ≈ $1
    let total_usd    = sol_in_usd + usdc_in_usd;

    Ok(WalletBalanceInfo {
        sol_balance,
        usdc_balance,
        sol_usd_price,
        sol_in_usd,
        usdc_in_usd,
        total_usd,
    })
}