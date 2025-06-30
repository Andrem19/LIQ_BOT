// db/triggers.rs
use crate::database::db::DB;
use sqlx::Result;
use tokio::sync::mpsc::UnboundedSender;
use sqlx;
use sqlx::Error;
use sqlx::Row;
use crate::telegram_service::tl_engine::ServiceCommand;

#[derive(Debug, Clone, Default)]
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
// pub async fn get_trigger(name: &str) -> Result<Option<Trigger>> {
//     let row_opt = sqlx::query("SELECT name, state, position FROM triggers WHERE name = ?1")
//         .bind(name)
//         .fetch_optional(&*DB)
//         .await?;

//     if let Some(row) = row_opt {
//         let name: String = row.try_get("name")?;
//         let state: i32   = row.try_get("state")?;
//         let position: String = row.try_get("position")?;
//         Ok(Some(Trigger {
//             name,
//             state:    state != 0,
//             position,
//         }))
//     } else {
//         Ok(None)
//     }
// }
pub async fn get_trigger(name: &str) -> Trigger {
    // 1) fetch_one – если ни одной строки, сразу Err и panic!
    let row = sqlx::query(
            "SELECT name, state, position \
             FROM triggers \
             WHERE name = ?1"
        )
        .bind(name)
        .fetch_one(&*DB)
        .await
        .expect(&format!("DB error: could not fetch trigger `{}`", name));

    // 2) Разбираем колонки, unwrap’им ошибки десериализации
    let name: String = row
        .try_get("name")
        .expect("DB error: `name` column missing or has wrong type");

    let state_i: i32 = row
        .try_get("state")
        .expect("DB error: `state` column missing or has wrong type");

    let position: String = row
        .try_get("position")
        .expect("DB error: `position` column missing or has wrong type");

    // 3) Конструируем и возвращаем
    Trigger {
        name,
        state:    state_i != 0,
        position,
    }
}


/// Удалить флаг по имени. Возвращает `Some(Trigger)` удалённую запись или `None`.
pub async fn delete_trigger(name: &str) -> Result<Option<Trigger>> {
    let old = get_trigger(name).await;
    sqlx::query("DELETE FROM triggers WHERE name = ?1")
            .bind(name)
            .execute(&*DB)
            .await?;
    Ok(Some(old))
}

pub async fn update_trigger(tr: &Trigger) {
    // 1) Убедимся, что триггер существует (паника, если нет)
    let _existing: Trigger = get_trigger(&tr.name).await;

    // 2) Выполним сам UPDATE (паника, если ошибка)
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
    .await
    .expect(&format!("DB error: failed to update trigger `{}`", tr.name));
}

pub async fn auto_trade_switch(switch: bool, tx: Option<&UnboundedSender<ServiceCommand>>) -> Result<()>  {
    let mut t = Trigger {
        name: "auto_trade".into(),
        state: switch,
        position: "opening".into(),
    };
    // Пытаемся сохранить/обновить
    match upsert_trigger(&t).await {
        Ok(_) => {
            if let Some(tx) = tx {
                // Если передан канал — шлём сообщение
                let msg: String = format!("Trigger `auto_trade_switch` {}", switch);
                let _ = tx.send(ServiceCommand::SendMessage(
                    msg,
                ));
            }
        }
        Err(e) => {
            if let Some(tx) = tx {
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("❌ Failed to enable trigger auto_trade_switch: {}", e),
                ));
            }
        }
    }
    Ok(())
}

pub async fn pool_report_run(switch: bool, tx: &UnboundedSender<ServiceCommand>) -> Result<()>  {
    let mut t = Trigger {
        name: "pool_report_run".into(),
        state: switch,
        position: "".into(),
    };
    match upsert_trigger(&t).await {
        Ok(_) => {
            let msg = format!("Trigger `pool_report_run` {}", switch);
            let _ = tx.send(ServiceCommand::SendMessage(
                msg,
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

pub async fn closing_switcher(
    switch: bool,
    tx: Option<&UnboundedSender<ServiceCommand>>,
) -> Result<()> {
    // Формируем триггер
    let t = Trigger {
        name: "closing".into(),
        state: switch,
        position: String::default(),
    };

    // Пытаемся сохранить/обновить
    match upsert_trigger(&t).await {
        Ok(_) => {
            if let Some(tx) = tx {
                // Если передан канал — шлём сообщение
                let msg = format!("Trigger `closing_switcher` {}", switch);
                let _ = tx.send(ServiceCommand::SendMessage(
                    msg,
                ));
            }
        }
        Err(e) => {
            if let Some(tx) = tx {
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("❌ Failed to enable trigger closing_switcher: {}", e),
                ));
            }
        }
    }

    Ok(())
}

pub async fn report_info_reset(
    switch: bool,
    tx: Option<&UnboundedSender<ServiceCommand>>,
) -> Result<()> {
    // Формируем триггер
    let t = Trigger {
        name: "report_info_reset".into(),
        state: switch,
        position: String::default(),
    };

    // Пытаемся сохранить/обновить
    match upsert_trigger(&t).await {
        Ok(_) => {
            if let Some(tx) = tx {
                // Если передан канал — шлём сообщение
                let msg = format!("Trigger `report_info_reset` {}", switch);
                let _ = tx.send(ServiceCommand::SendMessage(
                    msg,
                ));
            }
        }
        Err(e) => {
            if let Some(tx) = tx {
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("❌ Failed to enable trigger new_position_switcher: {}", e),
                ));
            }
        }
    }

    Ok(())
}

pub async fn limit_switcher(
    switch: bool,
    tx: Option<&UnboundedSender<ServiceCommand>>,
) -> Result<()> {
    // Формируем триггер
    let t = Trigger {
        name: "limit".into(),
        state: switch,
        position: String::default(),
    };

    // Пытаемся сохранить/обновить
    match upsert_trigger(&t).await {
        Ok(_) => {
            if let Some(tx) = tx {
                // Если передан канал — шлём сообщение
                let msg = format!("Trigger `limit` {}", switch);
                let _ = tx.send(ServiceCommand::SendMessage(
                    msg,
                ));
            }
        }
        Err(e) => {
            if let Some(tx) = tx {
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("❌ Failed to enable trigger limit: {}", e),
                ));
            }
        }
    }

    Ok(())
}

pub async fn opening_switcher(
    switch: bool,
    tx: Option<&UnboundedSender<ServiceCommand>>,
) -> Result<()> {
    // Формируем триггер
    let t = Trigger {
        name: "opening".into(),
        state: switch,
        position: String::default(),
    };

    // Пытаемся сохранить/обновить
    match upsert_trigger(&t).await {
        Ok(_) => {
            if let Some(tx) = tx {
                // Если передан канал — шлём сообщение
                let msg = format!("Trigger `opening_switcher` {}", switch);
                let _ = tx.send(ServiceCommand::SendMessage(
                    msg,
                ));
            }
        }
        Err(e) => {
            if let Some(tx) = tx {
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("❌ Failed to enable trigger opening_switcher: {}", e),
                ));
            }
        }
    }

    Ok(())
}