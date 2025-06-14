// src/telegram_service/commands.rs

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use futures::future::BoxFuture;
use futures::FutureExt;
use chrono::Local;

pub type CommandFunc = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct Commander {
    // потокобезопасный словарь команд
    commands: Arc<Mutex<HashMap<Vec<String>, CommandFunc>>>,
    logs: bool,
}

impl Commander {
    pub fn new(logs: bool) -> Self {
        Commander {
            commands: Arc::new(Mutex::new(HashMap::new())),
            logs,
        }
    }

    /// Регистрирует команду. Теперь &self, не &mut self.
    pub fn add_command<F, Fut>(&self, cmd: &[&str], func: F)
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut map = self.commands.lock().unwrap();
        map.insert(
            cmd.iter().map(|s| s.to_string()).collect(),
            Arc::new(move |params: Vec<String>| func(params).boxed()),
        );
    }

    /// Разбирает входящую строку на команду и параметры
    pub fn decode_str(&self, prompt: &str) -> (Vec<String>, Vec<String>) {
        let mut cmd = Vec::new();
        let mut params = Vec::new();
        for el in prompt.split_whitespace() {
            if let Some(idx) = el.find("--") {
                params.push(el[idx+2..].to_string());
            } else {
                cmd.push(el.to_lowercase());
            }
        }
        (cmd, params)
    }

    /// Выполняет команду, если она есть
    pub async fn exec_command(&self, prompt: &str) {
        let (cmd, params) = self.decode_str(prompt);
    
        // Сначала — берём из мапы клон функции-колбэка и сразу отпускаем замок:
        let maybe_cb = {
            let map = self.commands.lock().unwrap();
            map.get(&cmd).cloned()
        };
    
        if let Some(cb) = maybe_cb {
            // Только здесь вызываем асинхронно нашу команду
            cb(params).await;
    
            if self.logs {
                println!(
                    "{} Выполнена: {}",
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    cmd.join(" ")
                );
            }
        } else if self.logs {
            println!(
                "{} Не найдена: {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                cmd.join(" ")
            );
        }
    }

    /// Возвращает дерево зарегистрированных команд
    pub fn show_tree(&self) -> String {
        let map = self.commands.lock().unwrap();
        map.keys().map(|k| k.join(" ")).collect::<Vec<_>>().join("\n")
    }
}
