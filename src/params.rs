use std::time::Duration;
use crate::types::PoolConfig;
use crate::types::{LiqPosition, Role};

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут
pub const RANGE_HALF: f64 = 0.003;
pub const PCT_LIST: [f64; 4] = [0.004, 0.005, 0.01, 0.015];
pub const WEIGHTS: [f64; 3] = [37.5, 25.0, 37.5];
pub const TOTAL_USDC: f64 = 1000.0;


pub const POOL: PoolConfig = PoolConfig {
    amount: 1000.0,
    program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    name: "WSOL/USDC",
    pool_address: "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e",
    position_1: Some(LiqPosition {
        role: Role::Middle,
        position_address: Some("uRgZAE5R8DEpG1jUahWoRTPiGZfuJ1g39VN4JVdWExh"),
        position_nft: Some("Da2APhiBKYJCcJfBPLZj7MTXh933uQa51WuuQNQt1PxR"),
        upper_price: 152.15,
        lower_price: 148.42
    }),
    position_2: Some(LiqPosition {
        role: Role::Middle,
        position_address: Some("uRgZAE5R8DEpG1jUahWoRTPiGZfuJ1g39VN4JVdWExh"),
        position_nft: Some("Da2APhiBKYJCcJfBPLZj7MTXh933uQa51WuuQNQt1PxR"),
        upper_price: 148.42,
        lower_price: 146.20
    }),
    position_3: Some(LiqPosition {
        role: Role::Middle,
        position_address: Some("uRgZAE5R8DEpG1jUahWoRTPiGZfuJ1g39VN4JVdWExh"),
        position_nft: Some("Da2APhiBKYJCcJfBPLZj7MTXh933uQa51WuuQNQt1PxR"),
        upper_price: 146.20,
        lower_price: 144.01
    }),
    mint_a: "So11111111111111111111111111111111111111112",
    mint_b: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    decimal_a: 9,
    decimal_b: 6,
};

