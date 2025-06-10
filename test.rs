// test.rs

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use orca_whirlpools_client::{
    {Position, Whirlpool},
    UpdateFeesAndRewardsBuilder,
    get_tick_array_address,
};
use std::str::FromStr;
use solana_sdk::program_pack::Pack;
// Orca Whirlpools всегда использует массивы из 88 тиков
const TICK_ARRAY_SIZE: i32 = 88;

#[tokio::main]
async fn main() -> Result<()> {
    // 0) Загрузить keypair
    let keypair = read_keypair_file("/home/jupiter/.config/solana/mainnet-id.json")
        .map_err(|e| anyhow::anyhow!("Не удалось прочитать ключи: {}", e))?;
    let owner = keypair.pubkey();

    // 1) RPC-клиент
    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

    // 2) Адреса whirlpool и позиции
    let whirlpool_addr = Pubkey::from_str("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE")?;
    let position_addr = Pubkey::from_str("9NgfNLFRQzMuhow7D4ddTCKkVyAvvPLJbaXQPoM1u4x3")?;

    // 3) Считать Whirlpool, чтобы получить tick_spacing
    let pool_acc   = rpc.get_account(&whirlpool_addr)?;
    let whirlpool: Whirlpool = Whirlpool::from_bytes(&pool_acc.data)?;

    // 4) Считать Position, чтобы узнать свои tick_lower/upper
    let pos_acc = rpc.get_account(&position_addr)?;
    let position: Position = Position::from_bytes(&pos_acc.data)?;
    let lower_tick = position.tick_lower_index;
    let upper_tick = position.tick_upper_index;

    // 5) Вычислить границу массива, в котором лежат наши тики
    let span = whirlpool.tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start_lower = lower_tick.div_euclid(span) * span;
    let start_upper = upper_tick.div_euclid(span) * span;

    // 6) Получить PDA массивов тиков
    let (tick_array_lower, _) = get_tick_array_address(&whirlpool_addr, start_lower)?;
    let (tick_array_upper, _) = get_tick_array_address(&whirlpool_addr, start_upper)?;

    // 7) Собрать инструкцию обновления fees & rewards
    let ix = UpdateFeesAndRewardsBuilder::new()
        .whirlpool(whirlpool_addr)
        .position(position_addr)
        .tick_array_lower(tick_array_lower)
        .tick_array_upper(tick_array_upper)
        .instruction();

    // 8) Отправить транзакцию
    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&owner),
        &[&keypair],
        recent_blockhash,
    );
    rpc.send_and_confirm_transaction(&tx)?;

    let dec_a = 9 as u32;
    let dec_b = 6 as u32;



    // 9) Повторно считать Position и вывести pending yield
    let pos_acc = rpc.get_account(&position_addr)?;
    let position: Position = Position::from_bytes(&pos_acc.data)?;

    let raw_fee_a = position.fee_owed_a;
    let raw_fee_b = position.fee_owed_b;

    // конвертируем в дробные токены
    let pending_a = raw_fee_a as f64 / 10f64.powi(dec_a as i32);
    let pending_b = raw_fee_b as f64 / 10f64.powi(dec_b as i32);

    // выводим
    println!("--- Pending Yield (actual tokens) ---");
    println!("Token A ({} decimals): {:.9}", dec_a, pending_a);
    println!("Token B ({} decimals): {:.6}", dec_b, pending_b);

    Ok(())
}
