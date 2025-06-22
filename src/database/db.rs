// db.rs
use once_cell::sync::Lazy;
use sqlx::{sqlite::{SqlitePoolOptions, SqliteConnectOptions}, Pool, Sqlite};
use std::{env, path::PathBuf};


pub static DB: Lazy<Pool<Sqlite>> = Lazy::new(|| {
    // 1) Составляем путь: текущая папка + bot.db
    let mut db_path: PathBuf = env::current_dir().expect("Не удалось получить CWD");
    db_path.push("bot.db");

    // 2) Опции: создаём файл, если его нет
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);

    // 3) Соединяем лениво — без unwrap!
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_lazy_with(opts)
});