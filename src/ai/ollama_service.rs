use reqwest;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct OllamaRequest {
    pub model: String,
    pub prompt: String,
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct OllamaResponse {
    pub response: String,
    pub done: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewsAnalysis {
    pub sector: String,
    pub sentiment: String,
    pub impact_score: f64,
}

/// Analyze a news headline for sector impact and sentiment using LLM
pub async fn analyze_news_with_llm(
    headline: &str,
    model: &str,
) -> Result<NewsAnalysis, Box<dyn std::error::Error>> {
    let prompt = format!(
        "Analyze the following news headline for its potential impact on stock markets. 
        Specifically, identify the relevant sector (e.g., Defense, AI, Energy, Healthcare, Consumer).
        Determine the sentiment (Positive, Negative, Neutral).
        Estimate the potential market impact on a scale of 1-10 (1 being negligible, 10 being major market mover).
        
        Headline: \"{}\"
        
        Return your answer in JSON format with keys: 'sector', 'sentiment', 'impact_score'.
        Example: {{\"sector\": \"Defense\", \"sentiment\": \"Positive\", \"impact_score\": 8}}",
        headline
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15)) // Timeout after 15 seconds
        .build()?;

    let url = "http://localhost:11434/api/generate";

    let request_body = OllamaRequest {
        model: model.to_string(),
        prompt,
        stream: false,
    };

    let response = client.post(url).json(&request_body).send().await?;

    if !response.status().is_success() {
        return Err(format!("Ollama API Error: {}", response.status()).into());
    }

    let ollama_resp: OllamaResponse = response.json().await?;

    // Parse the JSON from the LLM response
    let analysis: NewsAnalysis = serde_json::from_str(&ollama_resp.response)?;

    Ok(analysis)
}
