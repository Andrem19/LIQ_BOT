use std::time::Duration;
use crate::types::PoolConfig;
use crate::types::{LiqPosition, Role};

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут
pub const RANGE_HALF: f64 = 0.003;
pub const PCT_LIST_1: [f64; 4] = [0.006, 0.006, 0.012, 0.012];
pub const PCT_LIST_2: [f64; 4] = [0.005, 0.005, 0.01, 0.01];
pub const ATR_BORDER: f64 = 1.8;
pub const WEIGHTS: [f64; 3] = [37.5, 25.0, 37.5];
pub const TOTAL_USDC: f64 = 900.0;


pub const POOL: PoolConfig = PoolConfig {
    amount: 900.0,
    program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    name: "WSOL/USDC",
    pool_address: "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e",
    position_1: None,
    position_2: None,
    position_3: None,
    mint_a: "So11111111111111111111111111111111111111112",
    mint_b: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    decimal_a: 9,
    decimal_b: 6,
    sol_init: 2.0
};

