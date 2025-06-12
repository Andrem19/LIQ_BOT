//! hl.rs — перенос Python-класса HL на Rust
//! crate: hyperliquid_rust_sdk = "0.6.0"

use anyhow::{anyhow, Result};
use ethers::{
    signers::{LocalWallet, Signer},
    types::H160,
};
use hyperliquid_rust_sdk::{
    /* helpers-reexports */
    BaseUrl, truncate_float,
    /* info + meta */
    InfoClient, AssetMeta,
    /* exchange-reexports */
    ExchangeClient,
    ClientCancelRequest,
    ExchangeDataStatus, ExchangeResponseStatus,
    ClientOrder, ClientOrderRequest,
    MarketCloseParams, MarketOrderParams, ClientTrigger,
};
use std::{
    collections::HashMap,
    env,
    future::Future,
    sync::Arc,
    time::Duration,
};
use tokio::time::sleep;
use uuid::Uuid;

/* -------------------------------------------------------------------------- */
/*                                 конфигурация                               */
/* -------------------------------------------------------------------------- */

const MAX_RETRY:              u8  = 3;
const RETRY_DELAY_SECS:       u64 = 2;
const FILL_POLL_RETRIES:      u16 = 120;           // 60 с опроса
const FILL_POLL_DELAY_MILLIS: u64 = 500;
const MARKET_SLIPPAGE:        f64 = 0.03;          // 3 %

/* макрос-логгер: активен при HL_DEBUG */
macro_rules! dbgln { ($($arg:tt)*) => {
    if std::env::var("HL_DEBUG").is_ok() {
        println!($($arg)*);
    }
};}

/* универсальный retry с логированием ошибок */
async fn retry_async<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut n = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if n < MAX_RETRY => {
                dbgln!("retry_async: attempt {} failed: {e}", n + 1);
                n += 1;
                sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/* -------------------------------------------------------------------------- */
/*                                   клиент                                   */
/* -------------------------------------------------------------------------- */

pub struct HL {
    wallet:   Arc<LocalWallet>,
    address:  H160,           // таргет-суб-аккаунт
    info:     InfoClient,
    pub exchange: ExchangeClient,
    meta:     Option<Vec<AssetMeta>>,
}

/* =============================== impl HL ================================= */

impl HL {
    /* --------------------------- инициализация ------------------------- */

    pub async fn new(secret_hex: &str, sub_account: Option<H160>) -> Result<Self> {
        let wallet_raw: LocalWallet = secret_hex.parse()?;
        let wallet      = Arc::new(wallet_raw);
        let default_acc = wallet.address();
        let address     = sub_account.unwrap_or(default_acc);
        let base_url    = BaseUrl::Mainnet;

        dbgln!("Init InfoClient");
        let info = InfoClient::new(None, Some(base_url)).await?;
        dbgln!("Init ExchangeClient (account={:?})", sub_account);
        let exchange = ExchangeClient::new(
            None, (*wallet).clone(), Some(base_url), None, sub_account
        ).await?;
        
        Ok(Self { wallet, address, info, exchange, meta: None })
    }

    pub async fn new_from_env() -> Result<Self> {
        dotenv::dotenv().ok();
        let secret = env::var("HLSECRET_1").map_err(|_| anyhow!("env HLSECRET_1 not set"))?;
        let sub = None;//env::var("HL_SUBACCOUNT_1").ok()
            // .map(|s| s.parse::<H160>())
            // .transpose()?;
        Self::new(&secret, sub).await
    }

    /* ------------------------- кэш Meta (universe) ---------------------- */

    async fn meta(&mut self) -> Result<&Vec<AssetMeta>> {
        if self.meta.is_none() {
            dbgln!("→ InfoClient.meta()");
            let m = retry_async(|| async {
                let r = self.info.meta().await.map_err(|e| anyhow!(e))?;
                dbgln!("← meta = {:?}", r);
                Ok(r)
            }).await?;
            self.meta = Some(m.universe.clone());
        }
        Ok(self.meta.as_ref().unwrap())
    }

    #[inline] fn coin(sym: &str) -> &str { sym.trim_end_matches("USDT") }

    /* ---------------------------- API-методы --------------------------- */

    pub async fn get_balance(&self) -> Result<f64> {
        dbgln!("→ InfoClient.user_state() [balance]");
        let st = retry_async(|| async {
            let r = self.info.user_state(self.address).await.map_err(|e| anyhow!(e))?;
            dbgln!("← user_state = {:?}", r);
            Ok(r)
        })
        .await?;
        Ok(st.cross_margin_summary.account_value.parse()?)
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Option<Position>> {
        dbgln!("→ user_state() [position]");
        let st = retry_async(|| async {
            let r = self.info.user_state(self.address).await.map_err(|e| anyhow!(e))?;
            dbgln!("← user_state = {:?}", r); Ok(r)
        }).await?;
        for p in st.asset_positions {
            if p.position.coin.eq_ignore_ascii_case(Self::coin(symbol)) {
                let size: f64 = p.position.szi.parse()?;
                return Ok(Some(Position {
                    position_value: p.position.position_value.parse()?,
                    unrealized_pnl: p.position.unrealized_pnl.parse()?,
                    size:           size.abs(),
                    entry_px:       p.position.entry_px.as_deref().unwrap_or("0").parse()?,
                    side:           if size > 0.0 { 1 } else { 2 },
                }));
            }
        }
        Ok(None)
    }

    pub async fn instrument_info(&mut self, symbol: &str) -> Result<AssetMeta> {
        let coin = Self::coin(symbol);
        self.meta().await?
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(coin))
            .cloned()
            .ok_or_else(|| anyhow!("unknown asset {symbol}"))
    }
    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let coin = Self::coin(symbol);
        dbgln!("→ update_leverage({}, {})", coin, leverage);
        retry_async(|| async {
            let r = self.exchange
                .update_leverage(leverage, coin, false, None)    // ✔
                .await.map_err(|e| anyhow!(e))?;
            dbgln!("← update_leverage = {:?}", r);
            Ok(())
        }).await
    }
    /* ----------------------------- сделки ------------------------------ */

    pub async fn open_market_order(
        &mut self, symbol: &str, side: &str,
        amount_usdt: f64, reduce_only: bool, amount_coins: f64,
    ) -> Result<(Uuid, f64)> {
        let coin   = Self::coin(symbol);
        let cloid  = Uuid::new_v4();
        let is_buy = side.eq_ignore_ascii_case("Buy");

        let last_px = self.get_last_price(symbol).await?;
        let size = if reduce_only {
            amount_coins
        } else {
            truncate_float(amount_usdt / last_px,
                           self.instrument_info(symbol).await?.sz_decimals,
                           false)
        };
        if size == 0.0 { return Err(anyhow!("size=0")); }
        if !reduce_only { self.set_leverage(symbol, 8).await?; }

        let mut resp = self.send_market(size, is_buy, coin, cloid, reduce_only).await?;
        if let Some(px) = parse_avg_px(&resp)? { return Ok((cloid, px)); }
        if let Some(px) = self.poll_for_fill(symbol, size, is_buy).await? { return Ok((cloid, px)); }

        dbgln!("no fill → cancel & retry");
        self.cancel_if_stuck(coin).await?;
        resp = self.send_market(size, is_buy, coin, cloid, reduce_only).await?;
        if let Some(px) = parse_avg_px(&resp)? { return Ok((cloid, px)); }
        if let Some(px) = self.poll_for_fill(symbol, size, is_buy).await? { return Ok((cloid, px)); }

        Err(anyhow!("order {cloid} stuck"))
    }

    pub async fn open_sl(
        &mut self,
        symbol: &str,
        side: &str,
        position_sz: f64,
        entry_px: f64,
        sl_perc: f64,
    ) -> Result<()> {
        let coin = Self::coin(symbol);
        let decimals = self.instrument_info(symbol).await?.sz_decimals as i32;
        let sl_px = if side.eq_ignore_ascii_case("Buy") {
            entry_px * (1.0 - sl_perc)
        } else {
            entry_px * (1.0 + sl_perc)
        };
        let trigger_px = if side.eq_ignore_ascii_case("Sell") { sl_px * 0.999 } else { sl_px * 1.001 };
        let rounded_sz = (position_sz * 10f64.powi(decimals)).round() / 10f64.powi(decimals);

        let order = ClientOrderRequest {
            asset: coin.into(),
            is_buy: !side.eq_ignore_ascii_case("Buy"),
            reduce_only: true,
            limit_px: sl_px,
            sz: rounded_sz,
            cloid: None,
            order_type: ClientOrder::Trigger(ClientTrigger {
                trigger_px,
                is_market: true,
                tpsl: "sl".into(),
            }),
        };
        dbgln!("→ ExchangeClient.order() = {:?}", order);
        self.exchange.order(order, None).await
            .map(|r| dbgln!("← order resp = {:?}", r))
            .map_err(|e| anyhow!(e))
    }

    /* --------------------------- helpers ------------------------------- */

    async fn send_market(
        &self,
        size: f64,
        is_buy: bool,
        coin: &str,
        cloid: Uuid,
        reduce_only: bool,
    ) -> Result<ExchangeResponseStatus> {
        retry_async(|| async {
            if reduce_only {
                let p = MarketCloseParams {
                    asset: coin,
                    sz: Some(size),
                    px: None,
                    slippage: Some(MARKET_SLIPPAGE),
                    cloid: Some(cloid),
                    wallet: None,                       // ✔
                };
                dbgln!("→ market_close {:?}", p);
                self.exchange.market_close(p).await.map_err(|e| anyhow!(e))
            } else {
                let p = MarketOrderParams {
                    asset: coin,
                    is_buy,
                    sz: size,
                    px: None,
                    slippage: Some(MARKET_SLIPPAGE),
                    cloid: Some(cloid),
                    wallet: None,                       // ✔
                };
                dbgln!("→ market_open {:?}", p);
                self.exchange.market_open(p).await.map_err(|e| anyhow!(e))
            }
        })
        .await
        .map(|r| { dbgln!("← market resp = {:?}", r); r })
    }

    async fn poll_for_fill(&self, symbol: &str, size: f64, is_buy: bool) -> Result<Option<f64>> {
        let side_target = if is_buy { 1 } else { 2 };
        for i in 0..FILL_POLL_RETRIES {
            if let Some(pos) = self.get_position(symbol).await? {
                if pos.side == side_target && pos.size >= size * 0.999 {
                    dbgln!("poll filled after {}ms → {:?}", i*FILL_POLL_DELAY_MILLIS as u16, pos);
                    return Ok(Some(pos.entry_px));
                }
            }
            sleep(Duration::from_millis(FILL_POLL_DELAY_MILLIS)).await;
        }
        Ok(None)
    }

    async fn cancel_if_stuck(&self, coin: &str) -> Result<()> {
        dbgln!("→ InfoClient.open_orders()");
        let ord = self.info.open_orders(self.address).await?;
        dbgln!("← open_orders = {:?}", ord);
        for o in ord {
            if o.coin.eq_ignore_ascii_case(coin) {
                let req = ClientCancelRequest { asset: o.coin.clone(), oid: o.oid };
                dbgln!("→ cancel {:?}", req);
                let _ = self.exchange.cancel(req, None).await;
            }
        }
        Ok(())
    }

    pub async fn get_last_price(&self, symbol: &str) -> Result<f64> {
        let coin = Self::coin(symbol);
        dbgln!("→ InfoClient.all_mids()");
        let mids: HashMap<String,String> = self.info.all_mids().await?;
        dbgln!("← all_mids = {:?}", mids);
        Ok(mids.get(coin).ok_or_else(|| anyhow!("no mid"))?.parse()?)
    }
}

/* ------------------------------ utils ----------------------------------- */

fn parse_avg_px(resp: &ExchangeResponseStatus) -> Result<Option<f64>> {
    if let ExchangeResponseStatus::Ok(rsp) = resp {
        if let Some(data) = &rsp.data {
            for s in &data.statuses {
                if let ExchangeDataStatus::Filled(f) = s {
                    return Ok(Some(f.avg_px.parse()?));
                }
            }
        }
    }
    Ok(None)
}

/* ------------------------------ DTO ------------------------------------- */

#[derive(Debug, Clone)]
pub struct Position {
    pub position_value : f64,
    pub unrealized_pnl : f64,
    pub size           : f64,
    pub entry_px       : f64,
    pub side           : u8,   // 1 = long, 2 = short
}
