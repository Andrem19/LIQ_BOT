
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