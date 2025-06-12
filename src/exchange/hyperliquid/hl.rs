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
    /// agent-кошелёк, которым подписываются сделки
    wallet: Arc<LocalWallet>,
    /// адрес торгового аккаунта, где лежат позиции и USDC
    trading_address: H160,
    info: InfoClient,
    pub exchange: ExchangeClient,
    meta: Option<Vec<AssetMeta>>,
}

/* =============================== impl HL ================================= */

impl HL {
    /* --------------------------- инициализация ------------------------- */

    pub async fn new(secret_hex: &str, trading_addr: Option<H160>) -> Result<Self> {
        /* 1. agent-wallet ------------------------------------------------ */
        let wallet_raw: LocalWallet = secret_hex.parse()?;
        let wallet = Arc::new(wallet_raw);

        /* 2. адрес, на котором фактически ведётся торговля -------------- */
        let trading_address = trading_addr
            .ok_or_else(|| anyhow!("HL_TRADING_ADDRESS env-var is not set"))?;

        /* 3. end-points -------------------------------------------------- */
        let base_url = BaseUrl::Mainnet;

        dbgln!("Init InfoClient");
        let info = InfoClient::new(None, Some(base_url)).await?;

        dbgln!("Init ExchangeClient (vault_address = None)");
        /*  Важно: в ExchangeClient поле `vault_address` предназначено
            ТОЛЬКО для Vault-счётов. Для обычного trading-аккаунта нужно
            передавать `None`, иначе запросы начнут падать. */
        let exchange = ExchangeClient::new(
            None,
            (*wallet).clone(),
            Some(base_url),
            None,
            None,          // ← vault_address = None
        )
        .await?;

        Ok(Self {
            wallet,
            trading_address,
            info,
            exchange,
            meta: None,
        })
    }

    /// Читаем ключи из `.env`:
    /// * `HLSECRET_1` – приватный ключ agent-кошелька  
    /// * `HL_TRADING_ADDRESS` – hex-адрес торгового аккаунта
    pub async fn new_from_env() -> Result<Self> {
        dotenv::dotenv().ok();

        let secret = env::var("HLSECRET_1")
            .map_err(|_| anyhow!("env HLSECRET_1 not set"))?;

        let trading = env::var("HL_TRADING_ADDRESS")
            .map_err(|_| anyhow!("env HL_TRADING_ADDRESS not set"))?
            .parse::<H160>()
            .map_err(|_| anyhow!("invalid HL_TRADING_ADDRESS hex"))?;

        Self::new(&secret, Some(trading)).await
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
            let r = self
                .info
                .user_state(self.trading_address)
                .await
                .map_err(|e| anyhow!(e))?;
            dbgln!("← user_state = {:?}", r);
            Ok(r)
        })
        .await?;
        Ok(st.cross_margin_summary.account_value.parse()?)
    }


    pub async fn get_position(&self, symbol: &str) -> Result<Option<Position>> {
        dbgln!("→ user_state() [position]");
        let st = retry_async(|| async {
            let r = self
                .info
                .user_state(self.trading_address)
                .await
                .map_err(|e| anyhow!(e))?;
            dbgln!("← user_state = {:?}", r);
            Ok(r)
        })
        .await?;

        for p in st.asset_positions {
            if p.position.coin.eq_ignore_ascii_case(Self::coin(symbol)) {
                let size: f64 = p.position.szi.parse()?;
                return Ok(Some(Position {
                    position_value: p.position.position_value.parse()?,
                    unrealized_pnl: p.position.unrealized_pnl.parse()?,
                    size: size.abs(),
                    entry_px: p
                        .position
                        .entry_px
                        .as_deref()
                        .unwrap_or("0")
                        .parse()?,
                    side: if size > 0.0 { 1 } else { 2 },
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
        let coin   = Self::coin(symbol);
        let px_dec = 2;                 // Hyperliquid tick-size = 0.01
        let sz_dec = self.instrument_info(symbol).await?.sz_decimals;

        /* 1. цены SL и trigger, округляем к 0.01 */
        let raw_sl = if side.eq_ignore_ascii_case("Buy") {
            entry_px * (1.0 - sl_perc)
        } else {
            entry_px * (1.0 + sl_perc)
        };
        let trigger_px = round_to(raw_sl, px_dec);
        /* limit_px **обязан** = trigger_px, иначе “invalid price” */
        let limit_px   = trigger_px;

        /* 2. размер, округлённый по sz_decimals */
        let sz = round_to(position_sz, sz_dec);

        /* 3. заявка */
        let order = ClientOrderRequest {
            asset: coin.into(),
            is_buy: !side.eq_ignore_ascii_case("Buy"),   // противоположная сторона
            reduce_only: true,
            limit_px,
            sz,
            cloid: None,
            order_type: ClientOrder::Trigger(ClientTrigger {
                trigger_px,
                is_market: true,
                tpsl: "sl".into(),
            }),
        };

        dbgln!("→ ExchangeClient.order() = {:?}", order);
        self.exchange
            .order(order, Some(self.wallet.as_ref()))
            .await
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
        // короткие ссылки/копии
        let ex     = &self.exchange;
        let info   = &self.info;
        let addr   = self.trading_address;
        let coin_s = coin.to_string();
        let size_f = size;
        let is_buy_f = is_buy;
        let cloid_f  = cloid;

        retry_async(move || {
            let coin   = coin_s.clone();
            async move {
                if reduce_only {
                    /* 1. убираем все reduce-only ордера (SL/TP) */
                    let open = info.open_orders(addr).await
                        .map_err(|e| anyhow!(e))?;
                    for o in open {
                        if o.coin.eq_ignore_ascii_case(&coin) {
                            let req = ClientCancelRequest {
                                asset: o.coin.clone(),
                                oid: o.oid,
                            };
                            ex.cancel(req, None).await.map_err(|e| anyhow!(e))?;
                        }
                    }

                    /* 2. обычный market_open обратной стороны, он «съест» лонг */
                    let p = MarketOrderParams {
                        asset: &coin,
                        is_buy: !is_buy_f,       // <-- противоположная сторона!
                        sz: size_f,
                        px: None,
                        slippage: Some(MARKET_SLIPPAGE),
                        cloid: Some(cloid_f),
                        wallet: None,
                    };
                    dbgln!("→ market_open(close) {:?}", p);
                    ex.market_open(p).await.map_err(|e| anyhow!(e))
                } else {
                    /* открытие новой позиции */
                    let p = MarketOrderParams {
                        asset: &coin,
                        is_buy: is_buy_f,
                        sz: size_f,
                        px: None,
                        slippage: Some(MARKET_SLIPPAGE),
                        cloid: Some(cloid_f),
                        wallet: None,
                    };
                    dbgln!("→ market_open {:?}", p);
                    ex.market_open(p).await.map_err(|e| anyhow!(e))
                }
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
        let ord = self.info.open_orders(self.trading_address).await?;
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

    async fn cancel_open_orders(&self, coin: &str) -> Result<()> {
        dbgln!("→ InfoClient.open_orders()");
        let list = self
            .info
            .open_orders(self.trading_address)
            .await
            .map_err(|e| anyhow!(e))?;
        dbgln!("← open_orders = {:?}", list);

        for o in list {
            if o.coin.eq_ignore_ascii_case(coin) {
                let req = ClientCancelRequest {
                    asset: o.coin.clone(),
                    oid: o.oid,
                };
                dbgln!("→ cancel {:?}", req);
                // обязательно подпись agent-wallet
                self.exchange
                    .cancel(req, Some(self.wallet.as_ref()))
                    .await
                    .map_err(|e| anyhow!(e))?;
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

fn round_dec(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn round_to(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}