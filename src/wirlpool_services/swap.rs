//! Swap через агрегатор Jupiter (mainnet-beta).
//! С учётом исправления тела запроса к /swap.

use tokio::time::sleep;
use std::str::FromStr;
use crate::params::{USDC,WSOL,WBTC,RAY,WETH,USDT,KEYPAIR_FILENAME, RPC_URL};
use anyhow::{anyhow, bail, Result};
use serde_json::{self, Value};
use tokio::time::Duration;
use orca_tx_sender::Signer;
use solana_client::{
    rpc_client::RpcClient,
};

use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use solana_sdk::transaction::Transaction;
use std::env;
use crate::utils;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
};
use solana_sdk::transaction::VersionedTransaction;
use spl_associated_token_account::get_associated_token_address;
use crate::wirlpool_services::net::http_client;

pub const MIN_SWAP_ATOMS: u64  = 10_000;   // ≈ 0.00001 token

const RETRYABLE: &[&str] = &[
    "custom program error: 0x6",     // Orca-CLOB tiny-price
    "custom program error: 0x9ca",   // Raydium RequireGte
    "custom program error: 0x1786",  // Whirlpool InvalidTimestamp  ← NEW
];
const LIQ_ERRORS: &[&str] = &[
    "custom program error: 0x9ca",   // Raydium require_gte
    "custom program error: 0x1786",  // Whirlpool InvalidTimestamp
    "InsufficientLiquidity",
    "LiquidityExceeded",
];
const MAX_RETRY: u16 = 3;

pub async fn execute_swap_tokens(
    sell_mint: &str,
    buy_mint:  &str,
    amount_in: f64,
) -> Result<SwapResult> {
    use solana_sdk::{transaction::VersionedTransaction, signer::Signer};
    use base64::{engine::general_purpose::STANDARD as b64, Engine as _};

    let (in_mint , in_dec ) = mint_pub_dec(sell_mint)?;
    let (out_mint, out_dec) = mint_pub_dec(buy_mint)?;

    // ─── 1. amount_in → atoms ───────────────────────────────────────────
    let amount_atoms = ((amount_in * 10f64.powi(in_dec as i32)).ceil()) as u64;
    

    // ─── 2. сеть / кошелёк ──────────────────────────────────────────────
    let payer : Keypair      = read_keypair_file(KEYPAIR_FILENAME)
    .map_err(|e| anyhow!("read_keypair_file failed: {}", e))?;
    let wallet: Pubkey       = payer.pubkey();

    let rpc_url = env::var("HELIUS_HTTP")?;

    let rpc    = RpcClient::new(rpc_url.to_string());
    let http   = http_client();


    let wsol_pubkey = Pubkey::from_str(&WSOL)?;
    ensure_ata_sync(&rpc, &payer, &wallet, &wsol_pubkey)?;


    let get_bal = |mint: &Pubkey, dec: u8| -> Result<f64> {
        if mint.to_string() == WSOL {
            Ok(rpc.get_balance(&wallet)? as f64 / 1e9)
        } else {
            let ata = get_associated_token_address(&wallet, mint);
            let ui  = rpc.get_token_account_balance(&ata).ok();
            Ok(ui
                .and_then(|b| b.amount.parse::<u64>().ok())
                .map(|a| a as f64 / 10f64.powi(dec as i32))
                .unwrap_or(0.0))
        }
    };

    if amount_atoms < MIN_SWAP_ATOMS {
        return Ok(SwapResult {
            balance_sell: get_bal(&in_mint , in_dec )?,
            balance_buy : get_bal(&out_mint, out_dec)?,
        });
    }


    // ─── 3. две попытки:  50 bps  → 150 bps ─────────────────────────────
    let mut retry = 0;
    for slippage_bps in [40_u16, 120_u16, 500_u16] {
        const GOOD_DEXES:  [&str; 3] = ["Raydium", "Orca", "Meteora"];
        let url = format!(
            "https://quote-api.jup.ag/v6/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}&onlyDirectRoutes=false",
            in_mint, out_mint, amount_atoms, slippage_bps
            // , GOOD_DEXES.join(",")
        );
        let quote: serde_json::Value = http.get(&url).send().await?.json().await?;

        // 3.2 build swap (v0)
        let swap_req = serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": wallet.to_string(),
            "wrapAndUnwrapSol": true,
            "asLegacyTransaction": false
        });
        let swap_json: serde_json::Value = http
            .post("https://quote-api.jup.ag/v6/swap")
            .json(&swap_req)
            .send()
            .await?
            .json()
            .await?;

        let Some(tx_b64) = swap_json["swapTransaction"].as_str() else {
            if slippage_bps == 150 { bail!("swapTransaction missing") } else { continue };
        };

        // 3.3 подпись
        let mut vtx: VersionedTransaction = bincode::deserialize(&b64.decode(tx_b64)?)?;
        let msg = vtx.message.serialize();
        let sig = payer.sign_message(&msg);
        if vtx.signatures.is_empty() {
            vtx.signatures.push(sig);
        } else {
            vtx.signatures[0] = sig;
        }

        // 3.4 отправка
        match rpc.send_and_confirm_transaction(&vtx) {
            Ok(sig) => {
                println!("Swap OK: {sig}");
                sleep(Duration::from_millis(500)).await;
                let bal_in  = get_bal(&in_mint , in_dec )?;
                let bal_out = get_bal(&out_mint, out_dec)?;
                return Ok(SwapResult { balance_sell: bal_in, balance_buy: bal_out });
            }
            Err(e) if RETRYABLE.iter().any(|tag| e.to_string().contains(tag)) && retry < MAX_RETRY => {
                tokio::time::sleep(Duration::from_millis(400)).await;   // ждём следующий слот
                retry += 1;
                continue;       // получаем новый quote и пытаемся снова
            }
            Err(e) if LIQ_ERRORS.iter().any(|tag| e.to_string().contains(tag)) => {
                let price_usdc = utils::get_sol_price_usd(sell_mint, false).await?;
                let usd_total  = amount_in * price_usdc;
                let parts = match usd_total {
                    x if x <  50.0   => 1,
                    x if x < 600.0   => 2,
                    x if x < 2_000.0 => 3,
                    _                => 4,
                };
                let chunk = amount_in / parts as f64;
                for _ in 0..parts {
                    swap_once_wrap(sell_mint, buy_mint, chunk).await?;
                    //   мини-пауза, чтобы следующий слот гарант-но отличался
                    sleep(Duration::from_millis(300)).await;
                }
                let bal_in  = get_bal(&in_mint , in_dec )?;
                let bal_out = get_bal(&out_mint, out_dec)?;
                return Ok(SwapResult { balance_sell: bal_in, balance_buy: bal_out });

            }

            // «виртуальная» ошибка Jupiter про несуществующий ATA
            Err(e) if e.to_string().contains("could not find account") => {
                println!("Jupiter virtual-ATA error (игнорируем): {e}");
                let bal_in  = get_bal(&in_mint , in_dec )?;
                let bal_out = get_bal(&out_mint, out_dec)?;
                return Ok(SwapResult { balance_sell: bal_in, balance_buy: bal_out });
            }

            Err(e) if retry < 2 => {
                println!("Retry swap ({} bp) because: {e}", slippage_bps);
                retry += 1;
                continue;
            }

            // любая другая ошибка
            Err(e) => return Err(anyhow!("send tx: {e}")),
        }
    }

    unreachable!("loop above always returns / errors")
}

#[derive(Debug)]
pub struct SwapResult {
    pub balance_sell: f64,
    pub balance_buy:  f64,
}


async fn swap_once_wrap(sell_mint: &str, buy_mint: &str, amount: f64) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD as b64, Engine as _};

    let (in_pub,  in_dec)  = mint_pub_dec(sell_mint)?;
    let (out_pub, _out_de) = mint_pub_dec(buy_mint)?;
    let payer              = utils::utils::load_wallet()?;
    let amount_atoms       = ((amount * 10f64.powi(in_dec as i32)).ceil()) as u64;
    if amount_atoms < MIN_SWAP_ATOMS { bail!("слишком маленькая сумма для свопа") }

    // 1) Quote ----------------------------------------------------------------
    let url = format!(
        "https://quote-api.jup.ag/v6/quote?inputMint={}&outputMint={}&amount={}",
        in_pub, out_pub, amount_atoms
    );
    let quote: serde_json::Value = http_client().get(&url).send().await?.json().await?;

    // 2) SwapTx ----------------------------------------------------------------
    let swap_req = serde_json::json!({
        "quoteResponse": quote,
        "userPublicKey": payer.pubkey().to_string(),
        "wrapAndUnwrapSol": true,
        "asLegacyTransaction": false
    });
    let swap_json: serde_json::Value = http_client()
        .post("https://quote-api.jup.ag/v6/swap")
        .json(&swap_req)
        .send()
        .await?
        .json()
        .await?;
    let tx_b64 = swap_json["swapTransaction"]
        .as_str()
        .ok_or_else(|| anyhow!("swapTransaction missing"))?;
    let mut vtx: VersionedTransaction = bincode::deserialize(&b64.decode(tx_b64)?)?;
    let msg  = vtx.message.serialize();
    let sig  = payer.sign_message(&msg);
    if vtx.signatures.is_empty() { vtx.signatures.push(sig) } else { vtx.signatures[0] = sig }

    // 3) Send ------------------------------------------------------------------
    RpcClient::new(RPC_URL.to_string())
        .send_and_confirm_transaction(&vtx)
        .map_err(|e| anyhow!("swap_once: {e}"))?;
    Ok(())
}

fn mint_pub_dec(symbol: &str) -> Result<(Pubkey,u8)> {
    Ok(match symbol {
        WSOL => (Pubkey::from_str(WSOL)?, 9),
        USDC => (Pubkey::from_str(USDC)?, 6),
        RAY  => (Pubkey::from_str(RAY )?, 6),
        WETH => (Pubkey::from_str(WETH)?, 8),
        WBTC => (Pubkey::from_str(WBTC)?, 8),
        USDT => (Pubkey::from_str(USDT)?, 6),
        _    => bail!("mint {symbol} не поддерживается"),
    })
}

fn ensure_ata_sync(
    rpc: &RpcClient,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
) -> anyhow::Result<()> {
    let ata = get_associated_token_address(owner, mint);
    if rpc.get_account(&ata).is_err() {
        // создаём idempotent-версию ATA — безопасно, если уже существует
        let ix = create_associated_token_account_idempotent(
            &payer.pubkey(), // funding_address (payer)
            owner,           // wallet_address (owner)
            mint,            // mint
            &spl_token::id(),// **программа ассоц. токен аккаунта**
        );
        let recent = rpc.get_latest_blockhash()
            .map_err(|e| anyhow::anyhow!("get_latest_blockhash failed: {}", e))?;
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer],
            recent,
        );
        rpc.send_and_confirm_transaction(&tx)
            .map_err(|e| anyhow::anyhow!("create ATA failed: {}", e))?;
    }
    Ok(())
}