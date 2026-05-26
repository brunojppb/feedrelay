//! Caption prompt templates.
//!
//! Implements the verbatim system and user prompts from PRD Appendix A.
//! This module is intentionally free of HTTP and DB concerns — it only
//! renders strings.

use crate::caption::CaptionContext;

const SYSTEM_PROMPT: &str = "You write short, authentic Instagram captions for a personal travel account. \
The user posts photos from trips with their spouse. Captions are first-person, warm but not saccharine, \
1-2 sentences, no emojis unless the photo really calls for one, no hashtags in the caption itself. \
Avoid generic travel cliches (\"wanderlust\", \"adventure awaits\", \"blessed\"). \
Avoid AI tells (em dashes, \"It's not just X, it's Y\" constructions). \
Speak like a thoughtful person who travels a lot, not a content marketer.\n\
\n\
Do NOT mention the city or country in the caption text. \
The location is appended automatically by the system after your text. \
Mentioning it in your output would cause it to appear twice.";

/// The rendered system and user prompts, ready to send to OpenAI.
pub struct RenderedPrompts {
    pub system: String,
    pub user: String,
}

/// Render the system and user prompts from the PRD Appendix A templates.
///
/// The `date` slot renders as `YYYY-MM-DD`; if missing, it renders as
/// `"an unknown date"` — this form is stable so the cache key doesn't drift.
///
/// Location edge cases:
/// - Both city and country present → "Lisbon, Portugal"
/// - City absent, country present → "Portugal"
/// - Both absent → "an unknown location" (the filter rejects this case in
///   practice, but we are robust here so the function never panics)
pub fn render_prompts(ctx: &CaptionContext<'_>) -> RenderedPrompts {
    let location = render_location(ctx.city, ctx.country);
    let date_str = ctx
        .date
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "an unknown date".to_string());

    let user = format!(
        "Here is a photo taken in {location} on {date_str}. Write a caption.\n\
\n\
Return JSON: {{ \"caption\": \"...\", \"hashtags\": [\"...\", \"...\"], \"alt_text\": \"...\" }}\n\
\n\
- `caption`: 1-2 sentences about the photo's mood or content, without naming the location \
(it gets appended automatically as \" - {location}\")\n\
- `hashtags`: 3-5 location-specific tags (not generic). Lowercase, no `#` prefix.\n\
- `alt_text`: describes the visual contents for screen readers."
    );

    RenderedPrompts {
        system: SYSTEM_PROMPT.to_string(),
        user,
    }
}

/// Build the human-readable location string for prompt slots.
///
/// - `(Some("Lisbon"), Some("Portugal"))` → `"Lisbon, Portugal"`
/// - `(None,           Some("Portugal"))` → `"Portugal"`
/// - `(Some("Lisbon"), None)             ` → `"Lisbon"`
/// - `(None,           None)             ` → `"an unknown location"`
fn render_location(city: Option<&str>, country: Option<&str>) -> String {
    match (city, country) {
        (Some(c), Some(co)) => format!("{c}, {co}"),
        (None, Some(co)) => co.to_string(),
        (Some(c), None) => c.to_string(),
        (None, None) => "an unknown location".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn make_ctx<'a>(
        city: Option<&'a str>,
        country: Option<&'a str>,
        date: Option<NaiveDate>,
    ) -> CaptionContext<'a> {
        CaptionContext {
            city,
            country,
            date,
        }
    }

    #[test]
    fn render_prompts_with_city_and_country() {
        let date = NaiveDate::from_ymd_opt(2024, 9, 15).unwrap();
        let ctx = make_ctx(Some("Lisbon"), Some("Portugal"), Some(date));
        let p = render_prompts(&ctx);

        // System prompt should contain the key constraint verbatim
        assert!(
            p.system.contains("Do NOT mention the city or country"),
            "system prompt missing location constraint"
        );

        // User prompt should fill both slots
        assert!(
            p.user.contains("Lisbon, Portugal"),
            "user prompt missing city+country: {:?}",
            p.user
        );
        assert!(
            p.user.contains("2024-09-15"),
            "user prompt missing date: {:?}",
            p.user
        );

        // Stable cache key: same inputs must produce identical strings
        let p2 = render_prompts(&ctx);
        assert_eq!(p.system, p2.system);
        assert_eq!(p.user, p2.user);
    }

    #[test]
    fn render_prompts_country_only() {
        let date = NaiveDate::from_ymd_opt(2025, 3, 1).unwrap();
        let ctx = make_ctx(None, Some("Japan"), Some(date));
        let p = render_prompts(&ctx);

        assert!(
            p.user.contains("Japan"),
            "user prompt missing country: {:?}",
            p.user
        );
        // Should NOT contain a comma (no city prefix)
        assert!(
            !p.user.contains(", Japan"),
            "unexpected comma before country: {:?}",
            p.user
        );
        assert!(
            p.user.contains("2025-03-01"),
            "user prompt missing date: {:?}",
            p.user
        );
    }

    #[test]
    fn render_prompts_both_none_falls_back_gracefully() {
        let ctx = make_ctx(None, None, None);
        let p = render_prompts(&ctx);

        assert!(
            p.user.contains("an unknown location"),
            "expected 'an unknown location' fallback: {:?}",
            p.user
        );
        assert!(
            p.user.contains("an unknown date"),
            "expected 'an unknown date' fallback: {:?}",
            p.user
        );
    }
}
