#[tokio::main]
async fn main() {
    let client = reqwest::Client::new();
    match client.get("https://api.github.com").header("User-Agent", "tls-check").send().await {
        Ok(resp) => println!("OK: status={}", resp.status()),
        Err(e) => println!("ERR: {e:#}"),
    }
}
