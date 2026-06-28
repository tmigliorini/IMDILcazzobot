use anyhow::anyhow;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::prelude::Message;
use crate::handlers::{FromRefs, HandlerResult, reply_html};
use crate::{metrics, reply_html, repo};
use crate::config::{AppConfig, BattlesFeatureToggles};
use crate::domain::LanguageCode;
use crate::repo::WinRateAware;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum StatsCommands {
    #[command(description = "stats")]
    Stats
}

pub async fn cmd_handler(bot: Bot, msg: Message, repos: repo::Repositories, app_config: AppConfig) -> HandlerResult {
    metrics::CMD_STATS.chat.inc();
    
    let features = app_config.features.pvp;
    if features.show_stats {
        let from = msg.from.as_ref().ok_or(anyhow!("unexpected absence of a FROM field"))?;
        let chat_id = msg.chat.id.into();
        let from_refs = FromRefs(from, &chat_id);

        let answer = if msg.chat.is_private() {
            personal_stats_impl(&repos, from_refs).await?
        } else {
            chat_stats_impl(&repos, from_refs, features).await?
        };

        reply_html!(bot, msg, answer);
    } else {
        log::info!("ignoring the /stats command since it's disabled");
    }
    Ok(())
}

const LEDGER_CATEGORIES: [(repo::LedgerCategory, &str); 6] = [
    (repo::LedgerCategory::Grow, "grow"),
    (repo::LedgerCategory::Pvp, "pvp"),
    (repo::LedgerCategory::Donate, "donate"),
    (repo::LedgerCategory::LoanInterest, "loan_interest"),
    (repo::LedgerCategory::LoanPrincipal, "loan_principal"),
    (repo::LedgerCategory::Tax, "tax"),
];

async fn personal_stats_impl(repos: &repo::Repositories, from_refs: FromRefs<'_>) -> anyhow::Result<String> {
    let lang_code = LanguageCode::from_user(from_refs.0);
    repos.personal_stats.get(from_refs.0.id).await
        .map(|stats| t!("commands.stats.personal", locale = &lang_code,
            chats = stats.chats, max_length = stats.max_length, total_length = stats.total_length).to_string())
}

/// Shared between the personal/chat `/stats` breakdown (one player's dare/avere per category)
/// and the chat-wide economic report (every player's, summed - see `chat_economy_report_impl`):
/// same table shape, different data source and title.
fn render_category_breakdown(breakdown: &[repo::CategoryBreakdown], lang_code: &LanguageCode, title_key: &str) -> String {
    let category_lines = LEDGER_CATEGORIES.iter()
        .map(|(category, key)| {
            let (dare, avere) = breakdown.iter()
                .find(|b| b.category == *category)
                .map(|b| (b.dare, b.avere))
                .unwrap_or((0, 0));
            let sum = avere - dare;
            let category_key = format!("commands.stats.categories.{key}");
            let category_name = t!(&category_key, locale = lang_code);
            t!("commands.stats.category_line", locale = lang_code,
                category = category_name, dare = dare, avere = avere, sum = format!("{sum:+}")).to_string()
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let title = t!(title_key, locale = lang_code);

    format!("{title}\n\n{category_lines}")
}

async fn category_breakdown(repos: &repo::Repositories, from_refs: FromRefs<'_>) -> anyhow::Result<String> {
    let lang_code = LanguageCode::from_user(from_refs.0);
    let breakdown = repos.ledger.get_breakdown(from_refs.1, from_refs.0.id).await?;
    Ok(render_category_breakdown(&breakdown, &lang_code, "commands.stats.categories.title"))
}

/// The chat-wide counterpart to `/stats`' own per-player category breakdown: every player's
/// dare/avere summed together, so the chat can see its overall economic activity (total taxes
/// moved, total interest realized, etc.) at a glance - reachable from the "ℹ️ Informazion" inline
/// menu (see `crate::handlers::info::InfoSection::Report`), there's no dedicated slash command.
pub(crate) async fn chat_economy_report_impl(repos: &repo::Repositories, from_refs: FromRefs<'_>) -> anyhow::Result<String> {
    let lang_code = LanguageCode::from_user(from_refs.0);
    let breakdown = repos.ledger.get_chat_breakdown(from_refs.1).await?;
    Ok(render_category_breakdown(&breakdown, &lang_code, "commands.report.title"))
}

pub(crate) async fn chat_stats_impl(repos: &repo::Repositories, from_refs: FromRefs<'_>, features: BattlesFeatureToggles) -> anyhow::Result<String> {
    let lang_code = LanguageCode::from_user(from_refs.0);
    let (length, position) = repos.dicks.fetch_dick(from_refs.0.id, &from_refs.1.kind()).await?
        .map(|dick| (dick.length, dick.position.unwrap_or_default()))
        .unwrap_or_default();
    let length_stats = t!("commands.stats.length", locale = &lang_code,
        length = length, pos = position).to_string();
    let pvp_stats = repos.pvp_stats.get_stats(&from_refs.1.kind(), from_refs.0.id).await
        .map(|stats| t!("commands.stats.pvp", locale = &lang_code,
            win_rate = stats.win_rate_formatted(), win_streak = stats.win_streak_max,
            battles = stats.battles_total, wins = stats.battles_won,
            acquired = stats.acquired_length, lost = stats.lost_length).to_string())?;
    let breakdown = category_breakdown(repos, from_refs).await?;
    let notice_part = if features.show_stats_notice {
        let notice = t!("commands.stats.notice", locale = &lang_code);
        format!("\n\n<i>{notice}</i>")
    } else {
        String::default()
    };
    Ok(format!("{length_stats}\n\n{pvp_stats}\n\n{breakdown}{notice_part}"))
}
