use solana_sdk::pubkey::Pubkey;

use serde::{Deserialize, Serialize};

/// Снимок состояния пула, который читает `reporter()`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolRuntimeInfo {
    pub name:        String,   // “SOL/USDC” или “RAY/SOL”
    pub last_price:  f64,      // спотовая цена в quoted-формате (см. ниже)
    pub fees_usd:    f64,      // накопленные комиссии (в USD экв.)
}

impl Default for PoolRuntimeInfo {
    fn default() -> Self {
        Self { name: String::new(), last_price: 0.0, fees_usd: 0.0 }
    }
}

#[derive(Debug)]
pub struct PoolPositionInfo {
    pub pending_a: f64,
    pub pending_b: f64,
    pub pending_a_usd: f64,
    pub sum: f64,
    pub amount_a: f64,
    pub amount_b: f64,
    pub value_a: f64,
    pub value_b: f64,
    pub pct_a: f64,
    pub pct_b: f64,
    pub current_price: f64,
    pub lower_price: f64,
    pub upper_price: f64,
    pub pct_down: f64,
    pub pct_up: f64,
}

#[derive(Clone, Debug)]
pub enum Role {
    Middle,
    Up,
    Down
}

#[derive(Clone, Debug)]
pub struct LiqPosition {
    pub role: Role,
    pub position_address: Option<&'static str>,
    pub position_nft: Option<&'static str>,
    pub upper_price: f64,
    pub lower_price: f64
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub amount: f64,
    pub program:               &'static str,
    pub name:                  &'static str,
    pub pool_address:          &'static str,
    pub position_1:      Option<LiqPosition>,
    pub position_2:      Option<LiqPosition>,
    pub position_3:      Option<LiqPosition>,
    pub mint_a:                &'static str,
    pub mint_b:                &'static str,
    pub decimal_a:             u16,
    pub decimal_b:             u16,
    pub sol_init: f64
}


#[derive(Debug, Clone)]
pub struct RangeAlloc {
    pub range_idx: usize,
    pub range_type: &'static str,
    pub usdc_amount: f64,
    pub sol_amount: f64,
    pub usdc_equivalent: f64,
    pub upper_price: f64,
    pub lower_price: f64,
}

#[derive(Debug, Clone)]
pub struct PriceBound {
    pub bound_type: BoundType,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundType {
    UpperOuter,   // верхняя экстремальная (выше рынка)
    UpperInner,   // верхняя внутренняя (немного выше рынка)
    LowerInner,   // нижняя внутренняя (немного ниже рынка)
    LowerOuter,   // нижняя экстремальная (ещё ниже рынка)
}

#[derive(Clone, Debug)]
pub enum WorkerCommand {
    /// Start a new cycle with this pool configuration.
    On(PoolConfig),
    /// Immediately force-close any open position and go idle.
    Off,
}

#[derive(Debug)]
pub struct OpenPositionResult {
    pub position_mint: Pubkey,
    /// Объём WSOL (в единицах токена, не в атомах)
    pub amount_wsol: f64,
    /// Объём USDC (в единицах токена, не в атомах)
    pub amount_usdc: f64,
}