use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Structured output we ask the local model for. Small instruct models
/// (llama3.2:3b, qwen2.5:3b-instruct, phi3.5) are plenty for this - it's
/// classification + summarization, not reasoning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsAnalysis {
    /// -1.0 (very bearish) .. 1.0 (very bullish). 0 = neutral/no signal.
    pub sentiment: f64,
    /// 0.0..1.0 - how much the model trusts its own read of these headlines.
    /// Low confidence (thin/ambiguous headlines) should count for less in
    /// aggregation than high confidence.
    pub confidence: f64,
    /// Short, plain-language reasons, e.g. "guidance cut", "new product launch".
    pub key_points: Vec<String>,
    /// Anything that looks like it deserves a human look regardless of
    /// sentiment score - halts, investigations, restatements, etc.
    pub risk_flags: Vec<String>,
}

/// Macro/geopolitical theme analysis - deliberately a different shape and
/// prompt from `NewsAnalysis`. This is NOT a buy signal generator: it
/// summarizes what's happening and flags it for you to go research,
/// exactly like the risk_flags path in company news does. Catching a
/// move like Rheinmetal's after Germany's defense spending announcement
/// *before* it's obvious requires reading between headlines that a
/// simple sentiment score can't capture - so this asks the model to
/// reason about magnitude and durability, not just polarity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeAnalysis {
    /// 0.0 (nothing new here) .. 1.0 (a genuinely major, durable shift).
    /// Deliberately conservative by design - most news cycles about any
    /// given theme are noise, not a Rheinmetal-style inflection point.
    pub relevance: f64,
    pub summary: String,
    /// Which of the theme's tracked symbols this news plausibly affects,
    /// and why - always framed as "go research this", never "buy this".
    pub affected_symbols: Vec<String>,
    pub reasoning: String,
}

pub struct OllamaClient {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaClient {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            client: reqwest::Client::new(),
        }
    }

    async fn generate_json<T: for<'de> Deserialize<'de>>(&self, prompt: String) -> Result<T> {
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
            "options": { "temperature": 0.1 }
        });

        let resp: OllamaGenerateResponse = self
            .client
            .post(format!("{}/api/generate", self.base_url))
            .json(&body)
            .send()
            .await
            .context("failed to reach local Ollama - is `ollama serve` running?")?
            .json()
            .await
            .context("Ollama response wasn't the expected shape")?;

        serde_json::from_str(resp.response.trim())
            .context("model did not return valid JSON for the requested schema")
    }

    /// Ask the local model to read a batch of headlines for one symbol and
    /// return strict JSON. We ask twice-over for JSON-only (system prompt +
    /// explicit instruction) because small models drift into prose otherwise.
    pub async fn analyze_headlines(
        &self,
        symbol: &str,
        headlines: &[String],
    ) -> Result<NewsAnalysis> {
        if headlines.is_empty() {
            return Ok(NewsAnalysis {
                sentiment: 0.0,
                confidence: 0.0,
                key_points: vec![],
                risk_flags: vec![],
            });
        }

        let joined = headlines
            .iter()
            .take(15) // keep the prompt small - a local 3B model doesn't need 50 headlines
            .enumerate()
            .map(|(i, h)| format!("{}. {}", i + 1, h))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "You are a financial news classifier. You are NOT a forecaster - you are only \
             summarizing what these headlines say, not predicting price moves.\n\n\
             Stock: {symbol}\n\
             Headlines:\n{joined}\n\n\
             Respond with ONLY a JSON object, no other text, no markdown fences, matching \
             exactly this shape:\n\
             {{\"sentiment\": <float -1.0 to 1.0>, \"confidence\": <float 0.0 to 1.0>, \
             \"key_points\": [<short strings>], \"risk_flags\": [<short strings, empty if none>]}}\n\n\
             risk_flags should only contain things like: trading halt, fraud investigation, \
             restatement, delisting, guidance withdrawal, executive departure under scrutiny. \
             Leave it empty for ordinary news."
        );

        self.generate_json(prompt).await
    }

    /// Ask the local model to assess whether a batch of headlines about a
    /// macro/geopolitical theme represents a durable, meaningfully
    /// impactful shift - or just routine news volume.
    pub async fn analyze_theme(
        &self,
        theme_name: &str,
        tracked_symbols: &[String],
        headlines: &[String],
    ) -> Result<ThemeAnalysis> {
        if headlines.is_empty() {
            return Ok(ThemeAnalysis {
                relevance: 0.0,
                summary: "No headlines found for this theme in the current window.".into(),
                affected_symbols: vec![],
                reasoning: String::new(),
            });
        }

        let joined = headlines
            .iter()
            .take(20)
            .enumerate()
            .map(|(i, h)| format!("{}. {}", i + 1, h))
            .collect::<Vec<_>>()
            .join("\n");
        let symbols_list = tracked_symbols.join(", ");

        let prompt = format!(
            "You are a macro/geopolitical news analyst helping an investor decide whether \
             something happening in the world is worth researching further for a specific \
             theme they're tracking. You are NOT giving investment advice or predicting \
             prices - you are flagging whether this looks like a genuinely major, durable \
             shift (like a government announcing a large sustained policy change) versus \
             routine news noise.\n\n\
             Theme: {theme_name}\n\
             Symbols the investor is tracking for this theme: {symbols_list}\n\
             Recent headlines:\n{joined}\n\n\
             Respond with ONLY a JSON object, no other text, no markdown fences, matching \
             exactly this shape:\n\
             {{\"relevance\": <float 0.0 to 1.0, be conservative - most news is not a major \
             inflection point>, \"summary\": <one or two sentence plain-language summary>, \
             \"affected_symbols\": [<subset of the tracked symbols this plausibly affects>], \
             \"reasoning\": <brief note on why this is or isn't durable/significant>}}"
        );

        self.generate_json(prompt).await
    }
}

#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}
