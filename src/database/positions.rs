// db/positions.rs
use once_cell::sync::Lazy;
use sqlx::{Pool, Row, Sqlite, sqlite::SqlitePoolOptions};
use crate::database::db::DB;
use chrono::{DateTime, Utc};
use anyhow::Result;
use crate::types::{PoolConfig, LiqPosition, Role};

/// Роли позиции


/// Инициализация: создаём таблицу с единственной строкой (PK id=1)
pub async fn init_positions_module() -> sqlx::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pool_configs (
            id                        INTEGER PRIMARY KEY NOT NULL CHECK(id=1),
            amount                    REAL NOT NULL,
            program                   TEXT NOT NULL,
            name                      TEXT NOT NULL,
            pool_address              TEXT NOT NULL,
            mint_a                    TEXT NOT NULL,
            mint_b                    TEXT NOT NULL,
            decimal_a                 INTEGER NOT NULL,
            decimal_b                 INTEGER NOT NULL,

            position_role_1           TEXT,
            position_address_1        TEXT,
            position_nft_1            TEXT,
            upper_price_1             REAL,
            lower_price_1             REAL,
            commission_collected_1    REAL NOT NULL DEFAULT 0,

            position_role_2           TEXT,
            position_address_2        TEXT,
            position_nft_2            TEXT,
            upper_price_2             REAL,
            lower_price_2             REAL,
            commission_collected_2    REAL NOT NULL DEFAULT 0,

            position_role_3           TEXT,
            position_address_3        TEXT,
            position_nft_3            TEXT,
            upper_price_3             REAL,
            lower_price_3             REAL,
            commission_collected_3    REAL NOT NULL DEFAULT 0,

            date_opened               TEXT NOT NULL,
            is_closed                 INTEGER NOT NULL DEFAULT 0,
            total_value_open          REAL NOT NULL,
            total_value_current       REAL NOT NULL,
            wallet_balance            REAL NOT NULL
        );
        "#
    )
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Вставить или обновить (id=1) запись пула
pub async fn upsert_pool_config(
    amount: f64,
    program: &str,
    name: &str,
    pool_address: &str,
    pos1: Option<LiqPosition>,
    pos2: Option<LiqPosition>,
    pos3: Option<LiqPosition>,
    mint_a: &str,
    mint_b: &str,
    decimal_a: u16,
    decimal_b: u16,
    date_opened: DateTime<Utc>,
    is_closed: bool,
    commission1: f64,
    commission2: f64,
    commission3: f64,
    total_value_open: f64,
    total_value_current: f64,
    wallet_balance: f64,            // ← новый параметр
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO pool_configs (
            id, amount, program, name, pool_address,
            mint_a, mint_b, decimal_a, decimal_b,
            position_role_1, position_address_1, position_nft_1, upper_price_1, lower_price_1, commission_collected_1,
            position_role_2, position_address_2, position_nft_2, upper_price_2, lower_price_2, commission_collected_2,
            position_role_3, position_address_3, position_nft_3, upper_price_3, lower_price_3, commission_collected_3,
            date_opened, is_closed, total_value_open, total_value_current, wallet_balance
        ) VALUES (
            1, ?1, ?2, ?3, ?4,
            ?5, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20,
            ?21, ?22, ?23, ?24, ?25, ?26,
            ?27, ?28, ?29, ?30, ?31
        )
        ON CONFLICT(id) DO UPDATE SET
            amount                   = excluded.amount,
            program                  = excluded.program,
            name                     = excluded.name,
            pool_address             = excluded.pool_address,
            mint_a                   = excluded.mint_a,
            mint_b                   = excluded.mint_b,
            decimal_a                = excluded.decimal_a,
            decimal_b                = excluded.decimal_b,
            position_role_1          = excluded.position_role_1,
            position_address_1       = excluded.position_address_1,
            position_nft_1           = excluded.position_nft_1,
            upper_price_1            = excluded.upper_price_1,
            lower_price_1            = excluded.lower_price_1,
            commission_collected_1   = excluded.commission_collected_1,
            position_role_2          = excluded.position_role_2,
            position_address_2       = excluded.position_address_2,
            position_nft_2           = excluded.position_nft_2,
            upper_price_2            = excluded.upper_price_2,
            lower_price_2            = excluded.lower_price_2,
            commission_collected_2   = excluded.commission_collected_2,
            position_role_3          = excluded.position_role_3,
            position_address_3       = excluded.position_address_3,
            position_nft_3           = excluded.position_nft_3,
            upper_price_3            = excluded.upper_price_3,
            lower_price_3            = excluded.lower_price_3,
            commission_collected_3   = excluded.commission_collected_3,
            date_opened              = excluded.date_opened,
            is_closed                = excluded.is_closed,
            total_value_open         = excluded.total_value_open,
            total_value_current      = excluded.total_value_current,
            wallet_balance           = excluded.wallet_balance
        ;
        "#
    )
    .bind(amount)
    .bind(program)
    .bind(name)
    .bind(pool_address)
    .bind(mint_a)
    .bind(mint_b)
    .bind(decimal_a as i32)
    .bind(decimal_b as i32)
    // pos1
    .bind(pos1.as_ref().map(|p| p.role.as_str()))
    .bind(pos1.as_ref().and_then(|p| p.position_address.clone()))
    .bind(pos1.as_ref().and_then(|p| p.position_nft.clone()))
    .bind(pos1.as_ref().map(|p| p.upper_price))
    .bind(pos1.as_ref().map(|p| p.lower_price))
    .bind(commission1)
    // pos2
    .bind(pos2.as_ref().map(|p| p.role.as_str()))
    .bind(pos2.as_ref().and_then(|p| p.position_address.clone()))
    .bind(pos2.as_ref().and_then(|p| p.position_nft.clone()))
    .bind(pos2.as_ref().map(|p| p.upper_price))
    .bind(pos2.as_ref().map(|p| p.lower_price))
    .bind(commission2)
    // pos3
    .bind(pos3.as_ref().map(|p| p.role.as_str()))
    .bind(pos3.as_ref().and_then(|p| p.position_address.clone()))
    .bind(pos3.as_ref().and_then(|p| p.position_nft.clone()))
    .bind(pos3.as_ref().map(|p| p.upper_price))
    .bind(pos3.as_ref().map(|p| p.lower_price))
    .bind(commission3)
    // timestamps & flags
    .bind(date_opened.to_rfc3339())
    .bind(if is_closed { 1 } else { 0 })
    .bind(total_value_open)
    .bind(total_value_current)
    .bind(wallet_balance)             // ← новое биндинг
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Вернуть запись (id = 1), теперь с wallet_balance
pub async fn get_pool_config() -> sqlx::Result<Option<PoolConfig>> {
    if let Some(row) = sqlx::query("SELECT * FROM pool_configs WHERE id = 1")
        .fetch_optional(&*DB)
        .await?
    {
        // Вспомогательные лямбды
        let get_str = |col: &str| -> Option<String> {
            row.try_get::<Option<String>, _>(col).ok().flatten()
        };
        let get_f64 = |col: &str| -> Option<f64> {
            row.try_get::<Option<f64>, _>(col).ok().flatten()
        };
        let mk = |i: u8| -> Option<LiqPosition> {
            let role_s = get_str(&format!("position_role_{}", i))?;
            let role  = Role::from_str(&role_s)?;
            Some(LiqPosition {
                role,
                position_address: get_str(&format!("position_address_{}", i)),
                position_nft:     get_str(&format!("position_nft_{}", i)),
                upper_price:      get_f64(&format!("upper_price_{}", i)).unwrap_or(0.0),
                lower_price:      get_f64(&format!("lower_price_{}", i)).unwrap_or(0.0),
            })
        };

        // Парсим date_opened
        let dt_str: String = row.try_get("date_opened")?;
        let date_opened = DateTime::parse_from_rfc3339(&dt_str)
            .map_err(|e| sqlx::Error::Protocol(format!("Invalid date_opened: {}", e)))?
            .with_timezone(&Utc);

        let cfg = PoolConfig {
            amount:                 row.try_get("amount")?,
            program:                row.try_get("program")?,
            name:                   row.try_get("name")?,
            pool_address:           row.try_get("pool_address")?,
            mint_a:                 row.try_get("mint_a")?,
            mint_b:                 row.try_get("mint_b")?,
            decimal_a:              row.try_get::<i32,_>("decimal_a")? as u16,
            decimal_b:              row.try_get::<i32,_>("decimal_b")? as u16,
            position_1:             mk(1),
            position_2:             mk(2),
            position_3:             mk(3),
            date_opened,
            is_closed:              row.try_get::<i32,_>("is_closed")? != 0,
            commission_collected_1: row.try_get("commission_collected_1")?,
            commission_collected_2: row.try_get("commission_collected_2")?,
            commission_collected_3: row.try_get("commission_collected_3")?,
            total_value_open:       row.try_get("total_value_open")?,
            total_value_current:    row.try_get("total_value_current")?,
            wallet_balance:         row.try_get("wallet_balance")?, // ← новое поле
        };
        Ok(Some(cfg))
    } else {
        Ok(None)
    }
}

pub async fn record_position_metrics(
    cfg: &PoolConfig,
    commission1: f64,
    commission2: f64,
    commission3: f64,
    total_value_current: f64,
    wallet_balance: f64
) -> sqlx::Result<()> {
    let now = Utc::now();

    // 1) Смотрим, что в БД
    let existing = get_pool_config().await?;

    // 2) Если нет записи или запись помечена is_closed=true — первый запуск
    let is_first_run = existing
        .as_ref()
        .map(|r| r.is_closed)
        .unwrap_or(true);
    println!("is_first_run: {}", &is_first_run);
    if is_first_run {
        // ─── ПЕРВЫЙ запуск ─────────────────────────────────────────────────
        // вот здесь мы явно передаём все три cfg.position_N.clone()
        // и тем самым записываем их upper_price / lower_price
        upsert_pool_config(
            cfg.amount,
            &cfg.program,
            &cfg.name,
            &cfg.pool_address,
            cfg.position_1.clone(),           // role, address, upper_price, lower_price
            cfg.position_2.clone(),
            cfg.position_3.clone(),
            &cfg.mint_a,
            &cfg.mint_b,
            cfg.decimal_a,
            cfg.decimal_b,
            now,                              // date_opened
            false,                            // is_closed = false
            commission1,                      // commission_collected_1
            commission2,
            commission3,
            total_value_current,              // total_value_open
            total_value_current,              // total_value_current
            wallet_balance
        )
        .await?;
    } else {
        // ─── НЕ первый запуск ────────────────────────────────────────────────
        // обновляем только цифры: комиссии и текущее TVL
        update_commission(1, commission1).await?;
        update_commission(2, commission2).await?;
        update_commission(3, commission3).await?;
        update_total_value_current(total_value_current).await?;
    }

    Ok(())
}


/// Удалить единственную запись (id = 1), вернуть true если была
pub async fn delete_pool_config() -> sqlx::Result<bool> {
    let res = sqlx::query("DELETE FROM pool_configs WHERE id = 1")
        .execute(&*DB)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Пометить как закрытую (id = 1)
pub async fn close_pool_config() -> sqlx::Result<()> {
    sqlx::query("UPDATE pool_configs SET is_closed = 1 WHERE id = 1")
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить собранную комиссию в позиции i=1..=3
pub async fn update_commission(
    position_index: u8,
    commission: f64,
) -> sqlx::Result<()> {
    let col = match position_index {
        1 => "commission_collected_1",
        2 => "commission_collected_2",
        3 => "commission_collected_3",
        _ => return Err(sqlx::Error::Protocol("Invalid position_index".into())),
    };
    let sql = format!("UPDATE pool_configs SET {} = ?1 WHERE id = 1", col);
    sqlx::query(&sql)
        .bind(commission)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить текущее значение (id = 1)
pub async fn update_total_value_current(
    new_value: f64
) -> sqlx::Result<()> {
    sqlx::query("UPDATE pool_configs SET total_value_current = ?1 WHERE id = 1")
        .bind(new_value)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Записать новое значение wallet_balance
pub async fn update_wallet_balance(new_balance: f64) -> sqlx::Result<()> {
    sqlx::query("UPDATE pool_configs SET wallet_balance = ?1 WHERE id = 1")
        .bind(new_balance)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Получить текущее значение wallet_balance
pub async fn get_wallet_balance() -> sqlx::Result<Option<f64>> {
    let row = sqlx::query("SELECT wallet_balance FROM pool_configs WHERE id = 1")
        .fetch_optional(&*DB)
        .await?;
    Ok(row
        .and_then(|r| r.try_get::<f64, _>("wallet_balance").ok()))
}

/// Обновить address и/или nft для позиции index = 1..=3.
/// Если какой-то из параметров = `None`, он не меняется.
pub async fn update_position_fields(
    position_index: u8,
    new_address: Option<&str>,
    new_nft: Option<&str>,
) -> sqlx::Result<()> {
    // проверяем индекс
    if !(1..=3).contains(&position_index) {
        return Err(sqlx::Error::Protocol(
            "Invalid position_index, must be 1, 2 or 3".into(),
        ));
    }

    // собираем фрагменты SET
    let mut sets = Vec::new();
    if new_address.is_some() {
        sets.push(format!("position_address_{} = ?", position_index));
    }
    if new_nft.is_some() {
        sets.push(format!("position_nft_{}     = ?", position_index));
    }

    // если нечего менять — выходим
    if sets.is_empty() {
        return Ok(());
    }

    // финальный SQL
    let sql = format!(
        "UPDATE pool_configs SET {} WHERE id = 1",
        sets.join(", ")
    );

    // готовим запрос
    let mut q = sqlx::query(&sql);

    // биндим аргументы в том же порядке, что и в `sets`
    if let Some(addr) = new_address {
        q = q.bind(addr);
    }
    if let Some(nft) = new_nft {
        q = q.bind(nft);
    }

    // выполняем
    q.execute(&*DB).await?;
    Ok(())
}

pub async fn find_position_index_by_nft(position_mint: &str) -> Result<Option<u8>> {
    // Выбираем из единственной строки три столбца с mint’ами
    let row_opt = sqlx::query(
        "SELECT position_nft_1, position_nft_2, position_nft_3 \
         FROM pool_configs WHERE id = 1"
    )
    .fetch_optional(&*DB)
    .await?;

    if let Some(row) = row_opt {
        let nft1: Option<String> = row.try_get("position_nft_1")?;
        let nft2: Option<String> = row.try_get("position_nft_2")?;
        let nft3: Option<String> = row.try_get("position_nft_3")?;

        if nft1.as_deref() == Some(position_mint) {
            return Ok(Some(1));
        }
        if nft2.as_deref() == Some(position_mint) {
            return Ok(Some(2));
        }
        if nft3.as_deref() == Some(position_mint) {
            return Ok(Some(3));
        }
    }
    // либо записи вообще нет, либо ни один не совпал
    Ok(None)
}
