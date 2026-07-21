#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Theme {
    Defense,
    AI,
    Energy,
    Healthcare,
    Consumer,
    Unknown,
}

impl Theme {
    pub fn from_keywords(keywords: &[String]) -> Self {
        let text: String = keywords.join(" ").to_lowercase();

        if text.contains("defense")
            || text.contains("military")
            || text.contains("weapon")
            || text.contains("rheingold")
        {
            return Theme::Defense;
        }
        if text.contains("ai") || text.contains("artificial intelligence") || text.contains("chip")
        {
            return Theme::AI;
        }
        if text.contains("energy")
            || text.contains("oil")
            || text.contains("gas")
            || text.contains("renewable")
        {
            return Theme::Energy;
        }
        if text.contains("health") || text.contains("pharma") || text.contains("biotech") {
            return Theme::Healthcare;
        }

        Theme::Unknown
    }
}

#[derive(Debug, Clone)]
pub struct MacroEvent {
    pub id: String,
    pub headline: String,
    pub theme: Theme,
    pub impact_score: f64,
    pub timestamp: String,
}

#[derive(Debug, Clone)]
pub struct MacroContext {
    pub active_themes: Vec<Theme>,
    pub recent_events: Vec<MacroEvent>,
}

impl MacroContext {
    pub fn new() -> Self {
        MacroContext {
            active_themes: vec![],
            recent_events: vec![],
        }
    }

    pub fn add_event(&mut self, event: MacroEvent) {
        self.recent_events.push(event.clone());
        if !self.active_themes.contains(&event.theme) {
            self.active_themes.push(event.theme);
        }
    }

    pub fn is_theme_active(&self, theme: &Theme) -> bool {
        self.active_themes.contains(theme)
    }
}
