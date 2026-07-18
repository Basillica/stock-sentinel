use anyhow::{Context, Result};
use async_trait::async_trait;

#[async_trait]
pub trait NewsProvider: Send + Sync {
    async fn headlines(&self, symbol: &str, company_hint: Option<&str>) -> Result<Vec<String>>;
}

/// Google News RSS needs no API key and no signup, which matters for a
/// side-project you actually want to keep running. Swap for NewsAPI,
/// Finnhub's news endpoint, or Alpha Vantage news later if you want more
/// structured sources (publisher, timestamp, dedup).
pub struct GoogleNewsRssProvider {
    client: reqwest::Client,
}

impl GoogleNewsRssProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("failed to build HTTPS client - is the rustls-tls feature enabled?");
        Self { client }
    }
}

#[async_trait]
impl NewsProvider for GoogleNewsRssProvider {
    async fn headlines(&self, symbol: &str, company_hint: Option<&str>) -> Result<Vec<String>> {
        let query = match company_hint {
            Some(name) => format!("{symbol} {name} stock"),
            None => format!("{symbol} stock"),
        };
        let url = format!(
            "https://news.google.com/rss/search?q={}&hl=en-US&gl=US&ceid=US:en",
            urlencode(&query)
        );

        let body = self
            .client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (stock-sentinel research bot)")
            .send()
            .await
            .context("failed to reach Google News RSS")?
            .text()
            .await
            .context("failed to read RSS body")?;

        Ok(extract_titles(&body))
    }
}

/// Deliberately minimal - Google News RSS <item><title>...</title></item> is
/// simple enough that a real XML parser is overkill for a first cut. If you
/// later hit malformed feeds, swap this for `roxmltree` or `quick-xml`.
fn extract_titles(xml: &str) -> Vec<String> {
    let mut titles = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<title>") {
        rest = &rest[start + "<title>".len()..];
        if let Some(end) = rest.find("</title>") {
            let raw = &rest[..end];
            let decoded = html_unescape(raw.trim());
            // Skip the feed's own title (first <title> is the channel title,
            // e.g. "\"AAPL stock\" - Google News"), keep item titles.
            if !decoded.contains(" - Google News") && !decoded.is_empty() {
                titles.push(decoded);
            }
            rest = &rest[end + "</title>".len()..];
        } else {
            break;
        }
    }
    titles
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "%20".to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_item_titles_and_skips_channel_title() {
        let xml = r#"
            <rss><channel>
                <title>"AAPL stock" - Google News</title>
                <item><title>Apple beats earnings estimates</title></item>
                <item><title>iPhone sales &amp; services growth continue</title></item>
            </channel></rss>
        "#;
        let titles = extract_titles(xml);
        assert_eq!(titles.len(), 2);
        assert_eq!(titles[0], "Apple beats earnings estimates");
        assert_eq!(titles[1], "iPhone sales & services growth continue");
    }
}
