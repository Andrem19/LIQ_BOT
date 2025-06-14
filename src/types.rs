
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
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub program:               &'static str, // всегда whirLbM…  на mainnet, но оставляем для devnet
    pub name:                  &'static str,
    pub pool_address:          &'static str,
    pub position_1:      Option<LiqPosition>, // None  → позиции ещё нет
    pub position_2:      Option<LiqPosition>, // None  → позиции ещё нет
    pub position_3:      Option<LiqPosition>, // None  → позиции ещё нет
    pub mint_a:                &'static str,
    pub mint_b:                &'static str,
    pub decimal_a:             u16,
    pub decimal_b:             u16,
    pub initial_amount_usdc:   f64,
}