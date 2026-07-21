use crate::analysis::technical::Signal;
use rusqlite::Connection;

pub async fn init_db(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS stock_signals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT NOT NULL,
            signal TEXT NOT NULL,
            confidence REAL NOT NULL,
            reason TEXT,
            timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS historical_data (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT NOT NULL,
            date TEXT NOT NULL,
            close REAL NOT NULL,
            volume INTEGER,
            fetched_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )?;

    Ok(())
}

pub async fn save_signal(
    conn: &Connection,
    symbol: &str,
    signal: &Signal,
    confidence: f64,
    reason: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute(
        "INSERT INTO stock_signals (symbol, signal, confidence, reason) VALUES (?, ?, ?, ?)",
        rusqlite::params![symbol, format!("{:?}", signal), confidence, reason],
    )?;

    Ok(())
}

// pub async fn get_latest_signals(
//     conn: &Connection,
// ) -> Result<Vec<(String, String, f64, String, String)>, Box<dyn std::error::Error>> {
//     let rows = sqlx::query(
//         "SELECT symbol, signal, confidence, reason, timestamp FROM stock_signals ORDER BY timestamp DESC LIMIT 10"
//     )
//     .fetch_all(pool)
//     .await?;

//     let mut signals = Vec::new();
//     for row in rows {
//         let symbol: String = row.try_get("symbol")?;
//         let signal: String = row.try_get("signal")?;
//         let confidence: f64 = row.try_get("confidence")?;
//         let reason: String = row.try_get("reason")?;
//         let timestamp: String = row.try_get("timestamp")?;
//         signals.push((symbol, signal, confidence, reason, timestamp));
//     }

//     Ok(signals)
// }
