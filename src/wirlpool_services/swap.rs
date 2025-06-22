//! Swap через агрегатор Jupiter (mainnet-beta).
//! С учётом исправления тела запроса к /swap.

use std::str::FromStr;
use crate::params::{USDC,WSOL,WBTC,RAY,WETH,USDT,KEYPAIR_FILENAME, RPC_URL};
use anyhow::{anyhow, bail, Result};
use serde_json::{self, Value};
use solana_client::{
    rpc_client::RpcClient,
};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
};
use spl_associated_token_account::get_associated_token_address;
use crate::wirlpool_services::net::http_client;

pub const MIN_SWAP_ATOMS: u64  = 10_000;   // ≈ 0.00001 token

pub async fn execute_swap_tokens(
    sell_mint: &str,
    buy_mint:  &str,
    amount_in: f64,
) -> Result<SwapResult> {
    use solana_sdk::{transaction::VersionedTransaction, signer::Signer};
    use base64::{engine::general_purpose::STANDARD as b64, Engine as _};

    fn mint_info(m: &str) -> Result<(Pubkey,u8)> {
        Ok(match m {
            WSOL => (Pubkey::from_str(WSOL)?, 9),
            USDC => (Pubkey::from_str(USDC)?, 6),
            RAY  => (Pubkey::from_str(RAY )?, 6),
            WETH => (Pubkey::from_str(WETH)?, 8),
            WBTC => (Pubkey::from_str(WBTC)?, 8),
            USDT => (Pubkey::from_str(USDT)?, 6),
            _    => bail!("mint {m} не поддерживается")
        })
    }
    let (in_mint , in_dec ) = mint_info(sell_mint)?;
    let (out_mint, out_dec) = mint_info(buy_mint)?;

    // ─── 1. amount_in → atoms ───────────────────────────────────────────
    let amount_atoms = ((amount_in * 10f64.powi(in_dec as i32)).ceil()) as u64;


    // ─── 2. сеть / кошелёк ──────────────────────────────────────────────
    let payer : Keypair      = read_keypair_file(KEYPAIR_FILENAME)
    .map_err(|e| anyhow!("read_keypair_file failed: {}", e))?;
    let wallet: Pubkey       = payer.pubkey();
    let rpc    = RpcClient::new(RPC_URL.to_string());
    let http   = http_client();

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

#[inline]
fn to_atoms(x: f64, dec: u8) -> u64 {
    let mul = 10f64.powi(dec as i32);
    let v   = (x * mul).ceil();
    if v >= (u64::MAX as f64) { u64::MAX } else { v as u64 }
}

#[derive(Debug)]
pub struct SwapResult {
    pub balance_sell: f64,
    pub balance_buy:  f64,
}

