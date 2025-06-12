use std::time::Duration;

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут
pub const RANGE_HALF: f64 = 0.0025;

#[derive(Clone)]
pub struct PoolConfig {
    pub program: &'static str,
    pub name: &'static str,
    pub pool_address: &'static str,
    /// Т.к. позиция может быть не открыта, теперь Option
    pub position_address: Option<&'static str>,
    pub mint_A: &'static str,
    pub mint_B: &'static str,
    pub decimal_A: u16,
    pub decimal_B: u16,
    pub initial_amount_usdc: f64,
}

pub const POOLS: &[PoolConfig] = &[
    PoolConfig {
        program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        name: "WSOL/USDC",
        pool_address: "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e",
        position_address: Some("8TskD7A128Gwj864V2kHaqk4XngJhGdMbn3s2d6xR6nM"),
        mint_A: "So11111111111111111111111111111111111111112",
        mint_B: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        decimal_A: 9,
        decimal_B: 6,
        initial_amount_usdc: 100.0,
    },
    PoolConfig {
        program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        name: "WSOL/USDC",
        pool_address: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
        position_address: Some("8TskD7A128Gwj864V2kHaqk4XngJhGdMbn3s2d6xR6nM"),
        mint_A: "So11111111111111111111111111111111111111112",
        mint_B: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        decimal_A: 9,
        decimal_B: 6,
        initial_amount_usdc: 100.0,
    },
    PoolConfig {
        program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        name: "WSOL/USDC",
        pool_address: "4HppGTweoGQ8ZZ6UcCgwJKfi5mJD9Dqwy6htCpnbfBLW",
        position_address: Some("8TskD7A128Gwj864V2kHaqk4XngJhGdMbn3s2d6xR6nM"),
        mint_A: "So11111111111111111111111111111111111111112",
        mint_B: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        decimal_A: 9,
        decimal_B: 6,
        initial_amount_usdc: 100.0,
    },
];
