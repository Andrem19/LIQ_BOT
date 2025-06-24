// src/database/general_settings.rs
use crate::database::db::DB;
use crate::params::{PCT_LIST_1, PCT_LIST_2, WEIGHTS, AMOUNT, POOL_NUMBER};
use sqlx::{Row, Sqlite, sqlite::SqlitePoolOptions};

/// Глобальные настройки (единственная строка в таблице, id = 1)
#[derive(Debug, Clone)]
pub struct GeneralSettings {
    pub pct_list_1: [f64; 4],
    pub pct_list_2: [f64; 4],
    pub weights:    [f64; 3],
    pub amount:     f64,
    pub pool_number: u16,
}

/// Инициализация модуля — создаём таблицу `general_settings`.
pub async fn init_general_settings_module() -> sqlx::Result<()> {
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS general_settings (
            id           INTEGER PRIMARY KEY NOT NULL CHECK(id = 1),
            pct11        REAL    NOT NULL,
            pct12        REAL    NOT NULL,
            pct13        REAL    NOT NULL,
            pct14        REAL    NOT NULL,
            pct21        REAL    NOT NULL,
            pct22        REAL    NOT NULL,
            pct23        REAL    NOT NULL,
            pct24        REAL    NOT NULL,
            weight1      REAL    NOT NULL,
            weight2      REAL    NOT NULL,
            weight3      REAL    NOT NULL,
            amount       REAL    NOT NULL,
            pool_number  INTEGER NOT NULL
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
            weight1, weight2, weight3,
            amount, pool_number
        ) VALUES (
            1, ?,?,?, ?,?,?,?, ?,?,?, ?,?
        )
        ON CONFLICT(id) DO UPDATE SET
            pct11       = excluded.pct11,
            pct12       = excluded.pct12,
            pct13       = excluded.pct13,
            pct14       = excluded.pct14,
            pct21       = excluded.pct21,
            pct22       = excluded.pct22,
            pct23       = excluded.pct23,
            pct24       = excluded.pct24,
            weight1     = excluded.weight1,
            weight2     = excluded.weight2,
            weight3     = excluded.weight3,
            amount      = excluded.amount,
            pool_number = excluded.pool_number;
    "#)
    .bind(cfg.pct_list_1[0])
    .bind(cfg.pct_list_1[1])
    .bind(cfg.pct_list_1[2])
    .bind(cfg.pct_list_1[3])
    .bind(cfg.pct_list_2[0])
    .bind(cfg.pct_list_2[1])
    .bind(cfg.pct_list_2[2])
    .bind(cfg.pct_list_2[3])
    .bind(cfg.weights[0])
    .bind(cfg.weights[1])
    .bind(cfg.weights[2])
    .bind(cfg.amount)
    .bind(cfg.pool_number as i32)
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
        let cfg = GeneralSettings {
            pct_list_1: [
                row.try_get("pct11")?, row.try_get("pct12")?,
                row.try_get("pct13")?, row.try_get("pct14")?,
            ],
            pct_list_2: [
                row.try_get("pct21")?, row.try_get("pct22")?,
                row.try_get("pct23")?, row.try_get("pct24")?,
            ],
            weights: [
                row.try_get("weight1")?, row.try_get("weight2")?,
                row.try_get("weight3")?,
            ],
            amount:      row.try_get("amount")?,
            pool_number: row.try_get::<i32,_>("pool_number")? as u16,
        };
        Ok(Some(cfg))
    } else {
        Ok(None)
    }
}

/// Обновить pct_list_1 (4 элемента)
pub async fn update_pct_list_1(new: [f64;4]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE general_settings SET \
            pct11 = ?1, pct12 = ?2, pct13 = ?3, pct14 = ?4 \
         WHERE id = 1"
    )
    .bind(new[0])
    .bind(new[1])
    .bind(new[2])
    .bind(new[3])
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Обновить pct_list_2 (4 элемента)
pub async fn update_pct_list_2(new: [f64;4]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE general_settings SET \
            pct21 = ?1, pct22 = ?2, pct23 = ?3, pct24 = ?4 \
         WHERE id = 1"
    )
    .bind(new[0])
    .bind(new[1])
    .bind(new[2])
    .bind(new[3])
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Обновить weights (3 элемента)
pub async fn update_weights(new: [f64;3]) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE general_settings SET \
            weight1 = ?1, weight2 = ?2, weight3 = ?3 \
         WHERE id = 1"
    )
    .bind(new[0])
    .bind(new[1])
    .bind(new[2])
    .execute(&*DB)
    .await?;
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

pub async fn init_settings_from_params() -> sqlx::Result<()> {
    let cfg = GeneralSettings {
        pct_list_1:   PCT_LIST_1,
        pct_list_2:   PCT_LIST_2,
        weights:      WEIGHTS,
        amount:       AMOUNT,
        pool_number:  POOL_NUMBER,
    };
    upsert_general_settings(&cfg).await
}