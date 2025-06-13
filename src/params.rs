use std::time::Duration;

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут
pub const RANGE_HALF: f64 = 0.003;

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub program:               &'static str, // всегда whirLbM…  на mainnet, но оставляем для devnet
    pub name:                  &'static str,
    pub pool_address:          &'static str,
    pub position_address:      Option<&'static str>, // None  → позиции ещё нет
    pub mint_a:                &'static str,
    pub mint_b:                &'static str,
    pub decimal_a:             u16,
    pub decimal_b:             u16,
    pub initial_amount_usdc:   f64,
}

pub const POOLS: &[PoolConfig] = &[
    PoolConfig {
        program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        name: "WSOL/USDC",
        pool_address: "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e",
        position_address: None,
        mint_a: "So11111111111111111111111111111111111111112",
        mint_b: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        decimal_a: 9,
        decimal_b: 6,
        initial_amount_usdc: 40.0,
    }
];
