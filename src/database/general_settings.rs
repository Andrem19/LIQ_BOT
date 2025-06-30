// src/database/general_settings.rs
use crate::database::db::DB;
use crate::params::{
    PCT_LIST_1, PCT_LIST_2, WEIGHTS_NUMBER, COMPRESS,
    AMOUNT, POOL_NUMBER, PCT_NUMBER, INFO_INTERVAL,
};
use sqlx::{Row, sqlite::SqlitePoolOptions, Sqlite, Pool};

/// Глобальные настройки (единственная строка в таблице, id = 1)
#[derive(Debug, Clone)]
pub struct GeneralSettings {
    pub pct_list_1:    [f64; 4],
    pub pct_list_2:    [f64; 4],
    pub amount:        f64,
    pub pool_number:   u16,
    pub pct_number:    u16,
    pub info_interval: u16,
    pub weights_number: u16,
    pub compress: bool,
}

/// Инициализация модуля — создаём таблицу `general_settings`.
pub async fn init_general_settings_module() -> sqlx::Result<()> {
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS general_settings (
            id               INTEGER PRIMARY KEY NOT NULL CHECK(id = 1),
            pct11            REAL    NOT NULL,
            pct12            REAL    NOT NULL,
            pct13            REAL    NOT NULL,
            pct14            REAL    NOT NULL,
            pct21            REAL    NOT NULL,
            pct22            REAL    NOT NULL,
            pct23            REAL    NOT NULL,
            pct24            REAL    NOT NULL,
            weights_number   INTEGER NOT NULL DEFAULT 0,                    -- ★
            amount           REAL    NOT NULL,
            pool_number      INTEGER NOT NULL,
            pct_number       INTEGER NOT NULL,
            info_interval    INTEGER NOT NULL,
            compress         INTEGER NOT NULL CHECK(compress IN (0,1)) DEFAULT 0
        );
    "#)
    .execute(&*DB)
    .await?;
    Ok(())
}


/// Вставить или обновить единственную запись настроек (id = 1).
pub async fn upsert_general_settings(cfg: &GeneralSettings) -> sqlx::Result<()> {
    sqlx::query(r#"
        INSERT INTO general_settings (
            id,
            pct11, pct12, pct13, pct14,
            pct21, pct22, pct23, pct24,
            weights_number,                                           -- ★
            amount,
            pool_number,
            pct_number,
            info_interval,
            compress
        ) VALUES (
            1,
            ?,?,?,?,    -- pct11–pct14  (4)
            ?,?,?,?,    -- pct21–pct24  (4)
            ?,          -- weights_number (1) ★
            ?,          -- amount
            ?,          -- pool_number
            ?,          -- pct_number
            ?,          -- info_interval
            ?           -- compress
        )
        ON CONFLICT(id) DO UPDATE SET
            pct11            = excluded.pct11,
            pct12            = excluded.pct12,
            pct13            = excluded.pct13,
            pct14            = excluded.pct14,
            pct21            = excluded.pct21,
            pct22            = excluded.pct22,
            pct23            = excluded.pct23,
            pct24            = excluded.pct24,
            weights_number   = excluded.weights_number,               -- ★
            amount           = excluded.amount,
            pool_number      = excluded.pool_number,
            pct_number       = excluded.pct_number,
            info_interval    = excluded.info_interval,
            compress         = excluded.compress;
    "#)
    .bind(cfg.pct_list_1[0])
    .bind(cfg.pct_list_1[1])
    .bind(cfg.pct_list_1[2])
    .bind(cfg.pct_list_1[3])
    .bind(cfg.pct_list_2[0])
    .bind(cfg.pct_list_2[1])
    .bind(cfg.pct_list_2[2])
    .bind(cfg.pct_list_2[3])
    .bind(cfg.weights_number as i32)                                // ★ bind weights_number
    .bind(cfg.amount)
    .bind(cfg.pool_number as i32)
    .bind(cfg.pct_number as i32)
    .bind(cfg.info_interval as i32)
    .bind(cfg.compress as i32)
    .execute(&*DB)
    .await?;
    Ok(())
}


/// Получить единственную запись настроек (id = 1)
pub async fn get_general_settings() -> sqlx::Result<Option<GeneralSettings>> {
    if let Some(row) = sqlx::query("SELECT * FROM general_settings WHERE id = 1")
        .fetch_optional(&*DB)
        .await?
    {
        Ok(Some(GeneralSettings {
            pct_list_1: [
                row.try_get("pct11")?, row.try_get("pct12")?,
                row.try_get("pct13")?, row.try_get("pct14")?,
            ],
            pct_list_2: [
                row.try_get("pct21")?, row.try_get("pct22")?,
                row.try_get("pct23")?, row.try_get("pct24")?,
            ],
            weights_number: row.try_get::<i32, _>("weights_number")? as u16,  // ★
            amount:          row.try_get("amount")?,
            pool_number:     row.try_get::<i32, _>("pool_number")?   as u16,
            pct_number:      row.try_get::<i32, _>("pct_number")?    as u16,
            info_interval:   row.try_get::<i32, _>("info_interval")? as u16,
            compress:        row.try_get::<i32, _>("compress")? != 0,
        }))
    } else {
        Ok(None)
    }
}


/// Обновить pct_list_1 (4 элемента)
pub async fn update_pct_list_1(new: [f64; 4]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE general_settings SET \
            pct11 = ?1, pct12 = ?2, pct13 = ?3, pct14 = ?4 \
         WHERE id = 1"
    )
    .bind(new[0]).bind(new[1]).bind(new[2]).bind(new[3])
    .execute(&*DB).await?;
    Ok(())
}

/// Обновить pct_list_2 (4 элемента)
pub async fn update_pct_list_2(new: [f64; 4]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE general_settings SET \
            pct21 = ?1, pct22 = ?2, pct23 = ?3, pct24 = ?4 \
         WHERE id = 1"
    )
    .bind(new[0]).bind(new[1]).bind(new[2]).bind(new[3])
    .execute(&*DB).await?;
    Ok(())
}

/// Обновить amount
pub async fn update_amount(new: f64) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET amount = ?1 WHERE id = 1")
        .bind(new)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить pool_number
pub async fn update_pool_number(new: u16) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET pool_number = ?1 WHERE id = 1")
        .bind(new as i32)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить pct_number
pub async fn update_pct_number(new: u16) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET pct_number = ?1 WHERE id = 1")
        .bind(new as i32)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить info_interval
pub async fn update_info_interval(new: u16) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET info_interval = ?1 WHERE id = 1")
        .bind(new as i32)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Инициализация из констант в params
pub async fn init_settings_from_params() -> sqlx::Result<()> {
    let cfg = GeneralSettings {
        pct_list_1:    PCT_LIST_1,
        pct_list_2:    PCT_LIST_2,
        amount:        AMOUNT,
        pool_number:   POOL_NUMBER,
        pct_number:    PCT_NUMBER,
        info_interval: INFO_INTERVAL,
        weights_number: WEIGHTS_NUMBER,
        compress: COMPRESS
    };
    upsert_general_settings(&cfg).await
}

/// Обновить weights_number
pub async fn update_weights_number(new: u16) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET weights_number = ?1 WHERE id = 1")
        .bind(new as i32)
        .execute(&*DB)
        .await?;
    Ok(())
}

/// Обновить compress
pub async fn update_compress(new: bool) -> sqlx::Result<()> {
    sqlx::query("UPDATE general_settings SET compress = ?1 WHERE id = 1")
        .bind(new as i32)
        .execute(&*DB)
        .await?;
    Ok(())
}