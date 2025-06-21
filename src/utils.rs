use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use solana_sdk::instruction::Instruction;
use std::sync::atomic::AtomicUsize;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use crate::params;
use std::sync::atomic::Ordering;
use tokio::time::Duration;
use solana_sdk::account::Account;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
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