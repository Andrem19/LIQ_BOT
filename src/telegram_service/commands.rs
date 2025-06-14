// src/telegram_service/commands.rs

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use futures::future::BoxFuture;
use futures::FutureExt;
use chrono::Local;

/// Тип команды: функция, принимающая вектор строк и возвращающая async-будущее.
pub type CommandFunc = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct Commander {
    commands: Arc<Mutex<HashMap<Vec<String>, CommandFunc>>>,
    logs: bool,
}

impl Commander {
    /// Создаёт новый командер (включить/выключить логи на ваше усмотрение).
    pub fn new(logs: bool) -> Self {
        Commander {
            commands: Arc::new(Mutex::new(HashMap::new())),
            logs,
        }
    }

    /// Регистрирует команду, например: `cmd = ["status"]`
    /// и ассоциированную с ней async-функцию.
    pub async fn add_command<F, Fut>(&self, cmd: &[&str], func: F)
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut commands = self.commands.lock().await;
        commands.insert(
            cmd.iter().map(|s| s.to_string().to_lowercase()).collect(),
            Arc::new(move |params: Vec<String>| func(params).boxed()),
        );
    }

    /// Дешифратор: разбивает строку на «команду» (ключевые слова) и «параметры» (--param).
    pub fn decode_str(&self, prompt: &str) -> (Vec<String>, Vec<String>) {
        let mut command = Vec::new();
        let mut params = Vec::new();
        for el in prompt.split_whitespace() {
            if let Some(idx) = el.find("--") {
                params.push(el[idx + 2..].to_string());
            } else {
                command.push(el.to_lowercase());
            }
        }
        (command, params)
    }

    /// Выполняет найденную команду (если зарегистрирована).
    pub async fn exec_command(&self, prompt: &str) {
        let (command, params) = self.decode_str(prompt);
        let commands = self.commands.lock().await;
        if let Some(func) = commands.get(&command) {
            func(params).await;
            if self.logs {
                println!(
                    "{} Command successfully executed: {}",
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    command.join(" ")
                );
            }
        } else if self.logs {
            println!(
                "{} Command not found: {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                command.join(" ")
            );
        }
    }

    /// Просто выводит дерево зарегистрированных команд (для отладки).
    pub async fn show_tree(&self) -> String {
        let commands = self.commands.lock().await;
        let mut result = String::new();
        for cmd in commands.keys() {
            result.push_str(&format!("{} -- params: [..]\n", cmd.join(" ")));
        }
        result
    }
}
