use crate::database::triggers;
use serde::{Serialize, Deserialize};
use anyhow::{Result, Context};
use crate::params::WSOL;
use crate::utils::get_sol_price_usd;

#[derive(Debug, Serialize, Deserialize)]
pub struct Thresholds {
    pub lower_then:       bool,
    pub higher_then:      bool,
    pub lower_then_value: f64,
    pub higher_then_value: f64,
}

const TRIGGER_NAME: &str = "limit";

/// Общая внутренняя функция: сериализует пороги и сохраняет их в базу через upsert_trigger.
async fn upsert_thresholds(th: &Thresholds) -> Result<()> {
    // Сериализация не должна паниковать
    let payload = serde_json::to_string(th)
        .context("Failed to serialize Thresholds to JSON")?;
    
    let trigger = triggers::Trigger {
        name:     TRIGGER_NAME.to_string(),
        state:    true,
        position: payload,
    };

    // Передаём ошибку дальше с контекстом
    triggers::upsert_trigger(&trigger).await
        .context("Failed to upsert trigger in database")?;

    Ok(())
}

/// Устанавливает только нижний лимит.
pub async fn set_low_limit(limit: f64) -> Result<()> {
    let cfg = Thresholds {
        lower_then:       true,
        higher_then:      false,
        lower_then_value: limit,
        higher_then_value: 0.0,
    };
    upsert_thresholds(&cfg).await
}

/// Устанавливает только верхний лимит.
pub async fn set_high_limit(limit: f64) -> Result<()> {
    let cfg = Thresholds {
        lower_then:       false,
        higher_then:      true,
        lower_then_value: 0.0,
        higher_then_value: limit,
    };
    upsert_thresholds(&cfg).await
}

/// Устанавливает оба лимита сразу.
pub async fn set_both_limits(lower: f64, upper: f64) -> Result<()> {
    let cfg = Thresholds {
        lower_then:       true,
        higher_then:      true,
        lower_then_value: lower,
        higher_then_value: upper,
    };
    upsert_thresholds(&cfg).await
}

pub fn parse_thresholds(s: &str) -> Result<Thresholds> {
    serde_json::from_str(s)
        .context("Failed to parse Thresholds from JSON")
}

pub async fn is_limit_trigger_satisfied() -> Result<bool> {
    // 1) Получаем триггер из БД
    let trigger = match triggers::get_trigger(TRIGGER_NAME).await? {
        Some(t) if t.state => t,
        _ => return Ok(false), // нет записи или state=false
    };

    // 2) Проверяем, что в положении есть данные
    let pos = trigger.position.trim();
    if pos.is_empty() {
        return Ok(false);
    }

    // 3) Парсим JSON в Thresholds
    let thresholds = parse_thresholds(pos)
        .context("Failed to parse Thresholds JSON from trigger.position")?;

    // 4) Получаем текущую цену SOL
    let price = get_sol_price_usd(WSOL, false)
        .await
        .context("Failed to fetch SOL price")?;

    // 5) Оцениваем пороги
    let mut triggered = false;
    if thresholds.lower_then && price < thresholds.lower_then_value {
        triggered = true;
    }
    if thresholds.higher_then && price > thresholds.higher_then_value {
        triggered = true;
    }

    Ok(triggered)
}