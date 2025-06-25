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
use crate::wirlpool_services::net::http_client;
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
use crate::wirlpool_services::swap;
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
    // 1) Загружаем кошелёк и RPC
    let payer: Keypair = utils::load_wallet()
        .map_err(|e| anyhow!("failed to load wallet: {}", e))?;
    let wallet: Pubkey = payer.pubkey();
    let rpc = utils::init_rpc();

    // 2) Узнаём текущий баланс mint-а
    let balance = get_token_balance(&rpc, &wallet, mint, dec).await?;
    if balance <= keep_amount {
        return Ok(format!(
            "🔔 {} balance is {:.6}, which is ≤ keep_amount {:.6}. No swap.",
            mint, balance, keep_amount
        ));
    }

    // 3) Считаем, сколько нужно свопнуть
    let to_swap = balance - keep_amount;

    // 4) Пытаемся сделать своп → USDC
    match swap::execute_swap_tokens(mint, USDC, to_swap).await {
        Ok(res) => {
            // res.balance_sell — остаток sell-token после свопа
            Ok(format!(
                "🔁 Swapped {:.6} {} → USDC.\n\
                 Remaining {} balance: {:.6}\n\
                 USDC received (approx): {:.6}",
                to_swap,
                mint,
                mint,
                res.balance_sell,
                res.balance_buy
            ))
        }
        Err(e) => Err(anyhow!("swap failed for {}: {}", mint, e)),
    }
}



pub fn calc_bound_prices_struct(base_price: f64, pct_list: &[f64]) -> Vec<PriceBound> {
    assert!(pct_list.len() == 4, "Нужно ровно 4 процента: [верх_внутр, низ_внутр, верх_экстр, низ_экстр]");

    // Верхняя внутренняя граница (немного выше рынка)
    let upper_inner = base_price * (1.0 + pct_list[0]);
    // Нижняя внутренняя граница (немного ниже рынка)
    let lower_inner = base_price * (1.0 - pct_list[1]);
    // Верхняя экстремальная (ещё выше)
    let upper_outer = upper_inner * (1.0 + pct_list[2]);
    // Нижняя экстремальная (ещё ниже)
    let lower_outer = lower_inner * (1.0 - pct_list[3]);

    vec![
        PriceBound { bound_type: BoundType::UpperOuter, value: upper_outer },
        PriceBound { bound_type: BoundType::UpperInner, value: upper_inner },
        PriceBound { bound_type: BoundType::LowerInner, value: lower_inner },
        PriceBound { bound_type: BoundType::LowerOuter, value: lower_outer },
    ]
}



pub fn calc_range_allocation_struct(
    price: f64,
    bounds: &[PriceBound],
    weights: &[f64; 3],
    total_usdc: f64,
) -> Vec<RangeAlloc> {
    // ─── 1. Выбираем нужные границы ────────────────────────────────────
    let get = |t: BoundType| bounds.iter().find(|b| b.bound_type == t).unwrap().value;
    let upper_outer = get(BoundType::UpperOuter);
    let upper_inner = get(BoundType::UpperInner);
    let lower_inner = get(BoundType::LowerInner);
    let lower_outer = get(BoundType::LowerOuter);

    // ─── 2. Верхний диапазон (Role::Up) ─────────────────────────────────
    let up_weight = total_usdc * weights[0] / 100.0;
    let up_sol    = up_weight / price;
    let upper = RangeAlloc {
        role: Role::Up,                           // ✱ ИЗМЕНЕНО
        range_idx: 0,
        usdc_amount: 0.0,
        sol_amount: up_sol,
        usdc_equivalent: up_sol * price,
        upper_price: upper_outer,
        lower_price: upper_inner,
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
        upper_price: lower_inner,
        lower_price: lower_outer,
    };

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