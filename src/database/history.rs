// src/database/history.rs
use once_cell::sync::Lazy;
use sqlx::{Row, Sqlite, sqlite::SqlitePoolOptions, Pool};
use crate::database::db::DB;
use crate::database::positions::get_pool_config;
use chrono::{DateTime, Utc};
use chrono::Duration;

use chrono::NaiveDate;

/// Одна запись о сессии открытия/закрытия позиции
#[derive(Debug, Clone)]
pub struct SessionHistory {
    pub id:             i64,
    pub date_opened:    DateTime<Utc>,
    pub date_closed:    DateTime<Utc>,
    pub pool_name:      String,
    pub range_lower:    f64,
    pub range_upper:    f64,
    pub sum_open:       f64,
    pub sum_close:      f64,
    pub commissions:    f64,
}

#[derive(Debug)]
pub struct SessionStatistics {
    /// начало периода (00:00 UTC)
    pub period_start:    DateTime<Utc>,
    /// конец периода (23:59:59 UTC)
    pub period_end:      DateTime<Utc>,
    /// сколько сессий в периоде
    pub session_count:   usize,
    /// суммарные комиссии
    pub total_commissions: f64,
    /// суммарная прибыль/убыток (sum_close − sum_open)
    pub total_profit:    f64,
    /// net-прибыль: total_profit + total_commissions
    pub net_profit:      f64,
    /// средняя продолжительность сессии
    pub average_duration: Duration,
}


/// Инициализация модуля: создаём таблицу истории
pub async fn init_history_module() -> sqlx::Result<()> {
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS session_history (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            date_opened    TEXT NOT NULL,
            date_closed    TEXT NOT NULL,
            pool_name      TEXT NOT NULL,
            range_lower    REAL NOT NULL,
            range_upper    REAL NOT NULL,
            sum_open       REAL NOT NULL,
            sum_close      REAL NOT NULL,
            commissions    REAL NOT NULL
        );
    "#)
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Создаёт новую запись истории на основе текущего pool_config,
/// фиксируя дату открытия, дату закрытия (now), имя пула,
/// минимальный lower_price, максимальный upper_price,
/// sum_open = total_value_open,
/// sum_close = total_value_current,
/// commissions = сумма всех трёх commission_collected.
pub async fn record_session_history() -> sqlx::Result<i64> {
    // 1) Получаем текущую конфигурацию
    let cfg_opt = get_pool_config().await?;
    let cfg: crate::types::PoolConfig = cfg_opt.ok_or_else(|| sqlx::Error::Protocol(
        "No pool config found to record history".into()
    ))?;

    // 2) Вычисляем диапазоны
    let mut lowers = Vec::new();
    let mut uppers = Vec::new();
    if let Some(pos) = &cfg.position_1 {
        lowers.push(pos.lower_price);
        uppers.push(pos.upper_price);
    }
    if let Some(pos) = &cfg.position_2 {
        lowers.push(pos.lower_price);
        uppers.push(pos.upper_price);
    }
    if let Some(pos) = &cfg.position_3 {
        lowers.push(pos.lower_price);
        uppers.push(pos.upper_price);
    }
    let range_lower = lowers.into_iter().fold(f64::INFINITY, f64::min).min(0.0);
    let range_upper = uppers.into_iter().fold(0.0, f64::max);

    // 3) Сумма комиссий
    let commissions = cfg.commission_collected_1
        + cfg.commission_collected_2
        + cfg.commission_collected_3;

    // 4) Вставляем запись
    let now = Utc::now();
    let result = sqlx::query(r#"
        INSERT INTO session_history (
            date_opened,
            date_closed,
            pool_name,
            range_lower,
            range_upper,
            sum_open,
            sum_close,
            commissions
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
    "#)
    .bind(cfg.date_opened.to_rfc3339())
    .bind(now.to_rfc3339())
    .bind(&cfg.name)
    .bind(range_lower)
    .bind(range_upper)
    .bind(cfg.total_value_open)
    .bind(cfg.total_value_current)
    .bind(commissions)
    .execute(&*DB)
    .await?;

    Ok(result.last_insert_rowid())
}

/// Получить все записи истории
pub async fn get_all_sessions() -> sqlx::Result<Vec<SessionHistory>> {
    let rows = sqlx::query("SELECT * FROM session_history ORDER BY date_closed DESC")
        .fetch_all(&*DB)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let dt_open: String = row.try_get("date_opened")?;
        let dt_close: String = row.try_get("date_closed")?;
        let entry = SessionHistory {
            id:          row.try_get("id")?,
            date_opened: DateTime::parse_from_rfc3339(&dt_open)
                              .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                              .with_timezone(&Utc),
            date_closed: DateTime::parse_from_rfc3339(&dt_close)
                              .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                              .with_timezone(&Utc),
            pool_name:   row.try_get("pool_name")?,
            range_lower: row.try_get("range_lower")?,
            range_upper: row.try_get("range_upper")?,
            sum_open:    row.try_get("sum_open")?,
            sum_close:   row.try_get("sum_close")?,
            commissions: row.try_get("commissions")?,
        };
        out.push(entry);
    }
    Ok(out)
}

/// Получить одну запись истории по id
pub async fn get_session(id: i64) -> sqlx::Result<Option<SessionHistory>> {
    if let Some(row) = sqlx::query("SELECT * FROM session_history WHERE id = ?1")
        .bind(id)
        .fetch_optional(&*DB)
        .await?
    {
        let dt_open: String = row.try_get("date_opened")?;
        let dt_close: String = row.try_get("date_closed")?;
        let entry = SessionHistory {
            id:          row.try_get("id")?,
            date_opened: DateTime::parse_from_rfc3339(&dt_open)
                              .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                              .with_timezone(&Utc),
            date_closed: DateTime::parse_from_rfc3339(&dt_close)
                              .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                              .with_timezone(&Utc),
            pool_name:   row.try_get("pool_name")?,
            range_lower: row.try_get("range_lower")?,
            range_upper: row.try_get("range_upper")?,
            sum_open:    row.try_get("sum_open")?,
            sum_close:   row.try_get("sum_close")?,
            commissions: row.try_get("commissions")?,
        };
        Ok(Some(entry))
    } else {
        Ok(None)
    }
}

/// Удалить запись истории по id, вернуть true если удалилось
pub async fn delete_session(id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query("DELETE FROM session_history WHERE id = ?1")
        .bind(id)
        .execute(&*DB)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn get_session_history(
    from_days: i64,
    to_days: i64,
) -> sqlx::Result<Vec<SessionHistory>> {
    // 1. Вычисляем дневные границы в UTC:
    let today: NaiveDate = Utc::now().date_naive();
    let start_date = today + Duration::days(from_days);
    let end_date   = today + Duration::days(to_days);

    let start_dt = DateTime::<Utc>::from_utc(start_date.and_hms_opt(0,0,0).unwrap(), Utc);
    let end_dt   = DateTime::<Utc>::from_utc(end_date  .and_hms_opt(23,59,59).unwrap(), Utc);

    let start_str = start_dt.to_rfc3339();
    let end_str   = end_dt.to_rfc3339();

    // 2. Выполняем запрос по date_opened в нужном диапазоне
    let rows = sqlx::query(
        r#"
        SELECT id, date_opened, date_closed, pool_name,
               range_lower, range_upper,
               sum_open, sum_close, commissions
          FROM session_history
         WHERE date_opened >= ?1
           AND date_opened <= ?2
         ORDER BY date_opened DESC
        "#
    )
    .bind(start_str)
    .bind(end_str)
    .fetch_all(&*DB)
    .await?;

    // 3. Преобразуем строки в структуры
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let opened_s: String = row.try_get("date_opened")?;
        let closed_s: String = row.try_get("date_closed")?;
        // парсим ISO8601
        let date_opened = DateTime::parse_from_rfc3339(&opened_s)
            .map_err(|e| sqlx::Error::Protocol(format!("Invalid date_opened: {}", e)))?
            .with_timezone(&Utc);
        let date_closed = DateTime::parse_from_rfc3339(&closed_s)
            .map_err(|e| sqlx::Error::Protocol(format!("Invalid date_closed: {}", e)))?
            .with_timezone(&Utc);

        result.push(SessionHistory {
            id:            row.try_get("id")?,
            date_opened,
            date_closed,
            pool_name:     row.try_get("pool_name")?,
            range_lower:   row.try_get("range_lower")?,
            range_upper:   row.try_get("range_upper")?,
            sum_open:      row.try_get("sum_open")?,
            sum_close:     row.try_get("sum_close")?,
            commissions:   row.try_get("commissions")?,
        });
    }

    Ok(result)
}

pub async fn get_session_statistics(
    from_days: i64,
    to_days: i64,
) -> sqlx::Result<SessionStatistics> {
    // 1) границы периода
    let today: NaiveDate = Utc::now().date_naive();
    let start_date = today + Duration::days(from_days);
    let end_date   = today + Duration::days(to_days);
    let period_start = DateTime::<Utc>::from_utc(start_date.and_hms_opt(0,0,0).unwrap(), Utc);
    let period_end   = DateTime::<Utc>::from_utc(end_date  .and_hms_opt(23,59,59).unwrap(), Utc);

    // 2) получаем все сессии за период
    let sessions = get_session_history(from_days, to_days).await?;

    // 3) агрегация
    let session_count    = sessions.len();
    let total_commissions: f64 = sessions.iter().map(|s| s.commissions).sum();
    let total_profit: f64       = sessions.iter().map(|s| s.sum_close - s.sum_open).sum();
    let net_profit = total_profit + total_commissions;

    // 4) средняя продолжительность
    let total_secs: i64 = sessions.iter()
        .map(|s| (s.date_closed - s.date_opened).num_seconds())
        .sum();
    let average_duration = if session_count > 0 {
        Duration::seconds(total_secs / session_count as i64)
    } else {
        Duration::zero()
    };

    Ok(SessionStatistics {
        period_start,
        period_end,
        session_count,
        total_commissions,
        total_profit,
        net_profit,
        average_duration,
    })
}