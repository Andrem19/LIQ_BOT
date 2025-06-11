//! hl.rs — перенос Python-класса HL на Rust
//! crate: hyperliquid_rust_sdk = "0.6.0"

use anyhow::{anyhow, Result};
use ethers::signers::{LocalWallet, Signer};   // <-- address()
use ethers::types::H160;
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
/*                       небольшой универсальный retry                       */
/* -------------------------------------------------------------------------- */
const MAX_RETRY: u8 = 3;
const RETRY_DELAY_SECS: u64 = 2;

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
                n += 1;
                sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/* -------------------------------------------------------------------------- */
/*                               основной клиент                              */
/* -------------------------------------------------------------------------- */

pub struct HL {
    wallet:   Arc<LocalWallet>,
    address:  H160,
    info:     InfoClient,
    exchange: ExchangeClient,
    meta:     Option<Vec<AssetMeta>>,
}

impl HL {
    /* ------------------------- инициализация ---------------------------- */

    pub async fn new(secret_hex: &str, sub_account: Option<H160>) -> Result<Self> {
        let wallet_raw: LocalWallet = secret_hex.parse()?;
        let address = sub_account.unwrap_or(wallet_raw.address());
        let wallet  = Arc::new(wallet_raw);

        let base_url = BaseUrl::Mainnet;

        let info = InfoClient::new(None, Some(base_url)).await?;
        let exchange = ExchangeClient::new(
            None,                 // reqwest::Client
            (*wallet).clone(),
            Some(base_url),
            None,                 // meta
            None,                 // vault address
        )
        .await?;

        Ok(Self { wallet, address, info, exchange, meta: None })
    }

    /// ENV-shortcut — HL_SECRET и опционально HL_SUBACCOUNT
    pub async fn new_from_env() -> Result<Self> {
        let secret = env::var("HLSECRET_1").map_err(|_| anyhow!("env HL_SECRET not set"))?;
        let sub = env::var("HL_SUBACCOUNT").ok()
            .map(|s| s.parse::<H160>())
            .transpose()?;
        Self::new(&secret, sub).await
    }

    /* ----------------------- внутренний кеш meta ----------------------- */

    async fn meta(&mut self) -> Result<&Vec<AssetMeta>> {
        if self.meta.is_none() {
            let m = retry_async(|| async { self.info.meta().await.map_err(|e| anyhow!(e)) }).await?;
            self.meta = Some(m.universe);
        }
        Ok(self.meta.as_ref().unwrap())
    }

    fn coin(sym: &str) -> &str {
        sym.trim_end_matches("USDT")
    }

    /* ----------------------------- API-методы -------------------------- */

    pub async fn get_balance(&self) -> Result<f64> {
        let st = retry_async(|| async { self.info.user_state(self.address).await.map_err(|e| anyhow!(e)) }).await?;
        st.cross_margin_summary
            .account_value
            .parse()
            .map_err(|e| anyhow!("parse balance: {e}"))
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Option<Position>> {
        let coin = Self::coin(symbol);
        let st = retry_async(|| async { self.info.user_state(self.address).await.map_err(|e| anyhow!(e)) }).await?;

        for p in st.asset_positions {
            if p.position.coin.eq_ignore_ascii_case(coin) {
                let size: f64 = p.position.szi.parse()?;
                let entry_px: f64 = p.position.entry_px.as_deref().unwrap_or("0").parse()?;
                return Ok(Some(Position {
                    position_value: p.position.position_value.parse()?,
                    unrealized_pnl: p.position.unrealized_pnl.parse()?,
                    size: size.abs(),
                    entry_px,
                    side: if size > 0.0 { 1 } else { 2 },
                }));
            }
        }
        Ok(None)
    }

    pub async fn is_contract_exist(&mut self, symbol: &str) -> Result<(bool, Vec<String>)> {
        let list: Vec<String> = self.meta().await?
            .iter()
            .map(|m| format!("{}USDT", m.name))
            .collect();
        Ok((list.contains(&symbol.to_string()), list))
    }

    pub async fn instrument_info(&mut self, symbol: &str) -> Result<AssetMeta> {
        let coin = Self::coin(symbol);
        self.meta()
            .await?
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(coin))
            .cloned()
            .ok_or_else(|| anyhow!("unknown asset {symbol}"))
    }

    /// Обновляем плечо (isolated). Возврат — `Ok(())`.
    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let coin = Self::coin(symbol);
        retry_async(|| async {
            self.exchange
                .update_leverage(leverage, coin, false, None)
                .await
                .map(|_| ())                // игнорируем ExchangeResponseStatus
                .map_err(|e| anyhow!(e))
        })
        .await
    }

    pub async fn any_positions(&self) -> Result<Vec<(String, String, f64)>> {
        let st = retry_async(|| async { self.info.user_state(self.address).await.map_err(|e| anyhow!(e)) }).await?;
        Ok(st.asset_positions
            .into_iter()
            .filter_map(|p| {
                let sz: f64 = p.position.szi.parse().ok()?;
                if sz == 0.0 {
                    None
                } else {
                    let sd = if sz > 0.0 { "Buy" } else { "Sell" };
                    Some((format!("{}USDT", p.position.coin), sd.into(), sz.abs()))
                }
            })
            .collect())
    }

    pub async fn get_last_price(&self, symbol: &str) -> Result<f64> {
        let coin = Self::coin(symbol);
        let mids: HashMap<String, String> =
            retry_async(|| async { self.info.all_mids().await.map_err(|e| anyhow!(e)) }).await?;
        mids.get(coin)
            .ok_or_else(|| anyhow!("no mid for {coin}"))?
            .parse()
            .map_err(|e| anyhow!("parse price: {e}"))
    }

    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<()> {
        let coin = Self::coin(symbol);
        let orders = retry_async(|| async { self.info.open_orders(self.address).await.map_err(|e| anyhow!(e)) }).await?;
        for o in orders {
            if o.coin.eq_ignore_ascii_case(coin) {
                retry_async(|| {
                    let req = ClientCancelRequest { asset: o.coin.clone(), oid: o.oid };
                    async move { self.exchange.cancel(req, None).await.map(|_| ()).map_err(|e| anyhow!(e)) }
                })
                .await?;
            }
        }
        Ok(())
    }

    /* ----------- market / close — возвращает (cloid, avg_px) ------------ */

        /// Маркет-ордер / закрытие позиции. Возвращает `(cloid, avg_price)`.
        pub async fn open_market_order(
            &mut self,
            symbol: &str,
            side: &str,
            amount_usdt: f64,
            reduce_only: bool,
            amount_coins: f64,
        ) -> Result<(Uuid, f64)> {
            // 1) Подготовка
            let coin   = Self::coin(symbol);
            let cloid  = Uuid::new_v4();
            let is_buy = side.eq_ignore_ascii_case("Buy");
    
            // 2) Цена и размер
            let last_px = self.get_last_price(symbol).await?;
            let meta    = self.instrument_info(symbol).await?;
            let size = if reduce_only {
                amount_coins
            } else {
                truncate_float(amount_usdt / last_px, meta.sz_decimals, false)
            };
    
            // 3) Плечо
            self.set_leverage(symbol, 20).await?;
    
            // 4) Ручной retry-цикл с немедленным `.await` в ветках
            let resp: ExchangeResponseStatus = {
                let mut attempt = 0;
                loop {
                    // Собираем параметры внутри цикла, чтобы каждый раз была свежая структура
                    let result = if reduce_only {
                        let params = MarketCloseParams {
                            asset:    coin,
                            sz:       Some(size),
                            px:       None,
                            slippage: Some(0.01),
                            cloid:    Some(cloid),
                            wallet:   None,
                        };
                        // сразу ждём результат
                        self.exchange.market_close(params).await
                    } else {
                        let params = MarketOrderParams {
                            asset:    coin,
                            is_buy,
                            sz:       size,
                            px:       None,
                            slippage: Some(0.01),
                            cloid:    Some(cloid),
                            wallet:   None,
                        };
                        self.exchange.market_open(params).await
                    };
    
                    match result {
                        Ok(r) => break r,                          // успешный ответ
                        Err(e) if attempt < MAX_RETRY => {         // повторяем
                            attempt += 1;
                            sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                        }
                        Err(e) => return Err(anyhow!(e)),          // окончательный провал
                    }
                }
            };
    
            // 5) Извлекаем среднюю цену из заполненных статусов
            if let ExchangeResponseStatus::Ok(rsp) = resp {
                if let Some(data) = rsp.data {
                    for status in data.statuses {
                        if let ExchangeDataStatus::Filled(filled) = status {
                            let avg_px: f64 = filled.avg_px.parse()?;
                            return Ok((cloid, avg_px));
                        }
                    }
                }
            }
    
            // 6) Если не было fill — возвращаем last_px
            Ok((cloid, last_px))
        }
    
    

    /* ------------------------------- SL --------------------------------- */

    pub async fn open_sl(
        &mut self,
        symbol: &str,
        side: &str,
        position_sz: f64,
        entry_px: f64,
        sl_perc: f64,
    ) -> Result<()> {
        let coin   = Self::coin(symbol);
        let decimals = self.instrument_info(symbol).await?.sz_decimals as i32;

        let sl_px = if side.eq_ignore_ascii_case("Buy") {
            entry_px * (1.0 - sl_perc)
        } else {
            entry_px * (1.0 + sl_perc)
        };
        let trigger_px = if side.eq_ignore_ascii_case("Sell") {
            sl_px * (1.0 - 0.001)
        } else {
            sl_px * (1.0 + 0.001)
        };

        let rounded_sz =
            (position_sz * 10f64.powi(decimals)).round() / 10f64.powi(decimals);

        let order = ClientOrderRequest {
            asset: coin.to_string(),
            is_buy: !side.eq_ignore_ascii_case("Buy"),   // противоположная сторона
            reduce_only: true,
            limit_px: sl_px,
            sz: rounded_sz,
            cloid: None,
            order_type: ClientOrder::Trigger(ClientTrigger {
                trigger_px: trigger_px,
                is_market: true,
                tpsl: "sl".into(),
            }),
        };

        self.exchange
            .order(order, None)
            .await
            .map(|_| ())
            .map_err(|e| anyhow!(e))
    }
}

/* -------------------------------------------------------------------------- */
/*                          DTO позиции (как в Python)                        */
/* -------------------------------------------------------------------------- */

#[derive(Debug, Clone)]
pub struct Position {
    pub position_value: f64,
    pub unrealized_pnl: f64,
    pub size:           f64,
    pub entry_px:       f64,
    /// 1 — long, 2 — short
    pub side:           u8,
}
