use crate::ai::ollama_service::analyze_news_with_llm;
use crate::macros::themes::{MacroEvent, Theme};
use reqwest;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct NewsResponse {
    pub articles: Vec<Article>,
}

#[derive(Deserialize, Debug)]
pub struct Article {
    pub title: String,
    pub description: Option<String>,
    pub source: Source,
}

#[derive(Deserialize, Debug)]
pub struct Source {
    pub name: String,
}

pub async fn fetch_trending_news_llm(
    model: &str,
) -> Result<Vec<MacroEvent>, Box<dyn std::error::Error>> {
    let api_key = std::env::var("NEWS_API_KEY").expect("NEWS_API_KEY must be set");
    let url = format!(
        "https://newsapi.org/v2/top-headlines?country=us&apiKey={}",
        api_key
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let response = client.get(&url).send().await?;
    let json: NewsResponse = response.json().await?;

    let mut events = Vec::new();

    for article in json.articles {
        // Use LLM to analyze the headline
        match analyze_news_with_llm(&article.title, model).await {
            Ok(analysis) => {
                let theme = map_sector_to_theme(&analysis.sector);
                let sentiment_score = match analysis.sentiment.to_lowercase().as_str() {
                    "positive" => 1.0,
                    "negative" => -1.0,
                    _ => 0.0,
                };

                events.push(MacroEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    headline: article.title,
                    theme,
                    impact_score: analysis.impact_score * sentiment_score,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
            }
            Err(e) => {
                eprintln!(
                    "LLM Analysis failed for '{}': {}. Falling back to keyword matching.",
                    article.title, e
                );
                // Fallback to keyword matching
                let keywords: Vec<String> = article
                    .title
                    .split_whitespace()
                    .map(|s| s.to_lowercase())
                    .collect();
                let theme = Theme::from_keywords(&keywords);
                events.push(MacroEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    headline: article.title,
                    theme,
                    impact_score: 3.0,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
    }

    Ok(events)
}

fn map_sector_to_theme(sector: &str) -> Theme {
    let s = sector.to_lowercase();
    if s.contains("defense") || s.contains("military") || s.contains("weapon") {
        Theme::Defense
    } else if s.contains("ai")
        || s.contains("artificial intelligence")
        || s.contains("chip")
        || s.contains("semiconductor")
    {
        Theme::AI
    } else if s.contains("energy")
        || s.contains("oil")
        || s.contains("gas")
        || s.contains("renewable")
    {
        Theme::Energy
    } else if s.contains("health") || s.contains("pharma") || s.contains("biotech") {
        Theme::Healthcare
    } else {
        Theme::Unknown
    }
}
