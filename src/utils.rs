use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use solana_sdk::instruction::Instruction;
use std::sync::atomic::AtomicUsize;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use crate::params;
use std::sync::atomic::Ordering;
use tokio::time::Duration;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::account::Account;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
use crate::params::{WSOL, USDC};
use crate::wirlpool_services::swap;
use std::str::FromStr;
use orca_tx_sender::Signer;
use orca_tx_sender::ComputeBudgetInstruction;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::message::Message;
use orca_tx_sender::CommitmentConfig;

pub static RPC_ROTATOR: Lazy<RpcRotator> = Lazy::new(RpcRotator::new);

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
        for _ in 0..40 {
            if let Some(status) = rpc.get_signature_status(&sig).await.map_err(op("get_signature_status"))? {
                status.map_err(|e| anyhow!("transaction failed: {:?}", e))?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(anyhow!("Timeout: tx {} not confirmed within 40s", sig))
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