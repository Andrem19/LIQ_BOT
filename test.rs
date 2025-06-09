use std::str::FromStr;
use anchor_client::{Client, Cluster};
use raydium_amm_v3::{ID as CLMM_PROGRAM_ID, state::PositionState};
use solana_sdk::{pubkey::Pubkey, signature::{read_keypair_file, Keypair}};

fn get_position_pda(
    owner: &Pubkey,
    pool: &Pubkey,
    lower_tick: i32,
    upper_tick: i32,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            b"position",
            owner.as_ref(),
            pool.as_ref(),
            &lower_tick.to_le_bytes(),
            &upper_tick.to_le_bytes(),
        ],
        &CLMM_PROGRAM_ID,
    )
}

fn main() -> anyhow::Result<()> {
    // 1) Настройка клиента
    let keypair = read_keypair_file("~/.config/solana/mainnet-id.json")?;
    let client = Client::new_with_options(
        Cluster::Mainnet,
        keypair.clone(),
        anchor_client::solana_client::rpc_config::RpcClientConfig::default(),
    );
    let program = client.program(CLMM_PROGRAM_ID);

    // 2) Параметры вашей позиции
    let pool_pubkey = Pubkey::from_str("3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv")?;
    let lower_tick = /* ваш lower_tick */ -387_000;
    let upper_tick = /* ваш upper_tick */ -383_000;

    // 3) Вычисляем PDA и загружаем состояние
    let (position_pda, _) = get_position_pda(&keypair.pubkey(), &pool_pubkey, lower_tick, upper_tick);
    let position: PositionState = program.account(position_pda)?;

    // 4) Читаем незабранные комиссии
    let pending_wsol = position.fee_owed_a as f64 / 1e9;
    let pending_usdc = position.fee_owed_b as f64 / 1e6;

    println!(
        "Pending yield: {:.6} WSOL, {:.6} USDC",
        pending_wsol, pending_usdc
    );
    Ok(())
}
