use std::borrow::Cow;
use anyhow::anyhow;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::payloads::SendMessageSetters;
use teloxide::types::{InlineKeyboardMarkup, LinkPreviewOptions, Message, ReplyMarkup, UserId};
use crate::{config, metrics, repo};
use crate::config::DickOfDaySelectionMode;
use crate::domain::LanguageCode;
use crate::handlers::{details, FromRefs, HandlerResult, reply_html, utils};
use crate::handlers::utils::Incrementor;
use crate::handlers::utils::details_store::DetailsStore;

const DOD_ALREADY_CHOSEN_SQL_CODE: &str = "GD0E2";

#[derive(BotCommands, Clone)]
#[command(rename_rule = "snake_case")]
pub enum DickOfDayCommands {
    #[command(description = "dod")]
    DickOfDay,
    Dod,
}

pub async fn dod_cmd_handler(bot: Bot, msg: Message, cfg: config::AppConfig, repos: repo::Repositories,
                             incr: Incrementor, details_store: DetailsStore) -> HandlerResult {
    metrics::CMD_DOD_COUNTER.chat.inc();
    let from = msg.from.as_ref().ok_or(anyhow!("unexpected absence of a FROM field"))?;
    let chat_id = msg.chat.id.into();
    let from_refs = FromRefs(from, &chat_id);
    let (text, keyboard) = dick_of_day_impl(cfg, &repos, incr, from_refs, &details_store).await?;
    let mut request = reply_html(bot, &msg, text)
        .link_preview_options(disabled_link_preview());
    request.reply_markup = keyboard.map(ReplyMarkup::InlineKeyboard);
    request.await?;
    Ok(())
}

pub(crate) async fn dick_of_day_impl(cfg: config::AppConfig, repos: &repo::Repositories, incr: Incrementor,
                                     from_refs: FromRefs<'_>, details_store: &DetailsStore) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let (from, chat_id) = (from_refs.0, from_refs.1);
    let lang_code = LanguageCode::from_user(from);
    let winner = match cfg.features.dod_selection_mode {
        DickOfDaySelectionMode::WEIGHTS => {
            repos.users.get_random_active_member_with_poor_in_priority(&chat_id.kind()).await?
        },
        DickOfDaySelectionMode::EXCLUSION if cfg.dod_rich_exclusion_ratio.is_some() => {
            let rich_exclusion_ratio = cfg.dod_rich_exclusion_ratio.unwrap();
            repos.users.get_random_active_poor_member(&chat_id.kind(), rich_exclusion_ratio).await?
        },
        _ => repos.users.get_random_active_member(&chat_id.kind()).await?
    };
    let (answer, dod_details) = match winner {
        Some(winner) => {
            let increment = incr.dod_increment(from.id, chat_id.kind()).await;
            let dod_result = repos.dicks.set_dod_winner(chat_id, UserId(winner.uid as u64), increment.total).await;
            let (main_part, dod_details) = match dod_result {
                Ok(Some(repo::GrowthResult{ new_length, pos_in_top })) => {
                    if let Err(e) = repos.ledger.record(chat_id, UserId(winner.uid as u64), repo::LedgerCategory::Grow, increment.base as i32, None).await {
                        log::error!("couldn't record a ledger entry for a dod event ({}): {e}", winner.uid);
                    }
                    let answer = t!("commands.dod.result", locale = &lang_code,
                        uid = winner.uid, name = winner.name.escaped(), growth = increment.total, length = new_length).to_string();
                    // same Dettagli treatment as `dick::grow_impl`: position and any perks
                    // breakdown are deferred, the winner announcement itself stays visible.
                    let position = pos_in_top.map(|pos| t!("commands.dod.position", locale = &lang_code, pos = pos).to_string());
                    let perks_part = increment.perks_part_of_answer(&lang_code);
                    let perks_detail = (!perks_part.is_empty()).then(|| perks_part.trim_start_matches('\n').to_string());
                    let details = [position, perks_detail].into_iter().flatten().collect::<Vec<_>>();
                    let details = (!details.is_empty()).then(|| details.join("\n\n"));
                    (answer, details)
                },
                Ok(None) => {
                    log::error!("there was an attempt to set a non-existent dick as a winner (UserID={}, ChatId={})",
                        winner.uid, chat_id);
                    (t!("commands.dod.no_candidates", locale = &lang_code).to_string(), None)
                }
                Err(e) => {
                    match e.downcast::<sqlx::Error>()? {
                        sqlx::Error::Database(db_err)
                        if db_err.code() == Some(Cow::Borrowed(DOD_ALREADY_CHOSEN_SQL_CODE)) => {
                            // the trigger's own exception message happens to carry the winner's
                            // raw name, but trusting it directly would skip `Username::escaped()`
                            // - re-fetching it through the application layer keeps this on the
                            // same safe, HTML-escaped path as every other displayed name.
                            let winner_name = repos.dicks.get_today_dod_winner_name(&chat_id.kind()).await?
                                .map(|name| crate::domain::Username::new(name).escaped())
                                .unwrap_or_else(|| t!("commands.dod.unknown_winner", locale = &lang_code).to_string());
                            (t!("commands.dod.already_chosen", locale = &lang_code, name = winner_name).to_string(), None)
                        }
                        e => Err(e)?
                    }
                }
            };
            let time_left_part = utils::date::get_time_till_next_day_string(&lang_code);
            (format!("{main_part}{time_left_part}"), dod_details)
        },
        None => (t!("commands.dod.no_candidates", locale = &lang_code).to_string(), None)
    };
    let announcement = repos.announcements.get_new(&chat_id.kind(), &lang_code).await?
        .map(|announcement| format!("\n\n<i>{announcement}</i>"))
        .unwrap_or_default();
    let short_text = format!("{answer}{announcement}");
    // a dod result names the winner, who may not be whoever ran the command - anyone may expand
    // the Dettagli button (no owner gating), same reasoning as pvp/donate/presta results.
    Ok(details::maybe_deferred(short_text, dod_details, None, Some(details_store), &lang_code))
}

fn disabled_link_preview() -> LinkPreviewOptions {
    LinkPreviewOptions {
        is_disabled: true,

        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}
