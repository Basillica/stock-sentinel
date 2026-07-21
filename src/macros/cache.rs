use crate::macros::themes::MacroContext;
use chrono::Duration;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedMacroContext = Arc<RwLock<MacroContext>>;

pub async fn get_or_update_context(
    context: &SharedMacroContext,
    update_fn: impl FnOnce() -> tokio::task::JoinHandle<
        Result<(), Box<dyn std::error::Error + Send + Sync>>,
    >,
    ttl: i64,
) -> MacroContext {
    let ctx = context.read().await;
    let last_update = if ctx.recent_events.is_empty() {
        chrono::Utc::now() - Duration::minutes(ttl + 1) // Force update if empty
    } else {
        // Assume the last event's timestamp is close to the last update
        chrono::DateTime::parse_from_rfc3339(&ctx.recent_events.last().unwrap().timestamp)
            .unwrap_or_default()
            .with_timezone(&chrono::Utc)
    };

    let now = chrono::Utc::now();
    if (now - last_update).num_minutes() < ttl {
        // drop(ctx);
        return ctx.clone();
    }

    drop(ctx);

    // Release the lock and spawn the update task
    let context_clone = Arc::clone(context);
    let update_handle = update_fn();

    // Wait for the update to complete
    match update_handle.await {
        Ok(Ok(())) => {
            let ctx = context_clone.write().await;
            ctx.clone()
        }
        Ok(Err(e)) => {
            eprintln!("Failed to update macro context: {}", e);
            let ctx = context_clone.read().await;
            ctx.clone()
        }
        Err(e) => {
            eprintln!("Task join error: {}", e);
            let ctx = context_clone.read().await;
            ctx.clone()
        }
    }
}
