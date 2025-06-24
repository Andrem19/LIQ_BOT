// db/triggers.rs
use crate::database::db::DB;
use sqlx::Result;
use tokio::sync::mpsc::UnboundedSender;
use sqlx;
use sqlx::Row;
use crate::telegram_service::tl_engine::ServiceCommand;

#[derive(Debug, Clone)]
pub struct Trigger {
    pub name:     String,
    pub state:    bool,
    pub position: String,
}

pub async fn init() -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS triggers (
            name     TEXT PRIMARY KEY,
            state    INTEGER NOT NULL DEFAULT 0,
            position TEXT NOT NULL DEFAULT ''
        );
        "#
    )
    .execute(&*DB)
    .await?;
    Ok(())
}

/// Вставить новый или обновить существующий флаг.
/// Возвращает тот же `Trigger`, что и приняли.
pub async fn upsert_trigger(tr: &Trigger) -> Result<Trigger> {
    sqlx::query(
        r#"
        INSERT INTO triggers(name, state, position)
             VALUES (?1, ?2, ?3)
        ON CONFLICT(name) DO UPDATE SET
             state    = excluded.state,
             position = excluded.position
        "#
    )
    .bind(&tr.name)
    .bind(tr.state as i32)
    .bind(&tr.position)
    .execute(&*DB)
    .await?;
    Ok(tr.clone())
}

/// Получить флаг по имени. Если нет — `Ok(None)`.
pub async fn get_trigger(name: &str) -> Result<Option<Trigger>> {
    let row_opt = sqlx::query("SELECT name, state, position FROM triggers WHERE name = ?1")
        .bind(name)
        .fetch_optional(&*DB)
        .await?;

    if let Some(row) = row_opt {
        let name: String = row.try_get("name")?;
        let state: i32   = row.try_get("state")?;
        let position: String = row.try_get("position")?;
        Ok(Some(Trigger {
            name,
            state:    state != 0,
            position,
        }))
    } else {
        Ok(None)
    }
}

/// Удалить флаг по имени. Возвращает `Some(Trigger)` удалённую запись или `None`.
pub async fn delete_trigger(name: &str) -> Result<Option<Trigger>> {
    // 1) пытаемся получить старую запись
    if let Some(old) = get_trigger(name).await? {
        // 2) удаляем
        sqlx::query("DELETE FROM triggers WHERE name = ?1")
            .bind(name)
            .execute(&*DB)
            .await?;
        Ok(Some(old))
    } else {
        Ok(None)
    }
}

pub async fn update_trigger(tr: &Trigger) -> Result<Option<Trigger>> {
    // Сначала проверим, есть ли такой триггер
    if get_trigger(&tr.name).await?.is_some() {
        // Выполняем UPDATE по имени
        sqlx::query(
            r#"
            UPDATE triggers
               SET state    = ?1,
                   position = ?2
             WHERE name     = ?3
            "#
        )
        .bind(tr.state as i32)
        .bind(&tr.position)
        .bind(&tr.name)
        .execute(&*DB)
        .await?;
        // Возвращаем обновлённый триггер
        Ok(Some(tr.clone()))
    } else {
        // Нет такой записи
        Ok(None)
    }
}

pub async fn auto_trade_switch(switch: bool, tx: &UnboundedSender<ServiceCommand>) -> Result<()>  {
    let mut t = Trigger {
        name: "auto_trade".into(),
        state: switch,
        position: "opening".into(),
    };
    match upsert_trigger(&t).await {
        Ok(_) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                "✅ Trigger `auto_trade` enabled".into(),
            ));
        }
        Err(e) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                format!("❌ Failed to enable trigger: {}", e),
            ));
        }
    }
    Ok(())
}

pub async fn open_position_switch(switch: bool, tx: &UnboundedSender<ServiceCommand>) -> Result<()>  {
    let mut t = Trigger {
        name: "position_start_open".into(),
        state: switch,
        position: "".into(),
    };
    match upsert_trigger(&t).await {
        Ok(_) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                "✅ Trigger `position_start_open` enabled".into(),
            ));
        }
        Err(e) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                format!("❌ Failed to enable trigger: {}", e),
            ));
        }
    }
    Ok(())
}