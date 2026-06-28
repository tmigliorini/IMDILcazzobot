use std::future::IntoFuture;

use anyhow::{anyhow, Context};
use chrono::{Datelike, Utc};
use futures::future::join;
use futures::TryFutureExt;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, ParseMode, ReplyMarkup, User, UserId};

use page::Page;

use crate::{config, metrics, repo};
use crate::domain::{LanguageCode, Username};
use crate::handlers::{details, HandlerResult, reply_html, utils};
use crate::handlers::utils::{callbacks, Incrementor, page};
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::details_store::DetailsStore;
use crate::repo::{ChatIdPartiality, WinRateAware, UID};

const TOMORROW_SQL_CODE: &str = "GD0E1";

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum DickCommands {
    #[command(description = "grow")]
    Grow,
    #[command(description = "top")]
    Top,
}

pub async fn dick_cmd_handler(bot: Bot, msg: Message, cmd: DickCommands,
                              repos: repo::Repositories, incr: Incrementor,
                              config: config::AppConfig, details_store: DetailsStore) -> HandlerResult {
    let from = msg.from.as_ref().ok_or(anyhow!("unexpected absence of a FROM field"))?;
    let chat_id = msg.chat.id.into();
    let from_refs = FromRefs(from, &chat_id);
    match cmd {
        DickCommands::Grow => {
            metrics::CMD_GROW_COUNTER.chat.inc();
            let (text, keyboard) = grow_impl(&repos, incr, from_refs, &details_store).await?;
            let mut request = reply_html(bot, &msg, text);
            request.reply_markup = keyboard.map(ReplyMarkup::InlineKeyboard);
            request
        },
        DickCommands::Top => {
            metrics::CMD_TOP_COUNTER.chat.inc();
            let top = top_impl(&repos, &config, from_refs, Page::first(), TopView::Length).await?;
            let mut request = reply_html(bot, &msg, top.lines);
            if config.features.top_unlimited {
                let keyboard = ReplyMarkup::InlineKeyboard(build_pagination_keyboard(Page::first(), top.has_more_pages, TopView::Length));
                request.reply_markup.replace(keyboard);
            }
            request
        }
    }.await.context(format!("failed for {msg:?}"))?;
    Ok(())
}

pub struct FromRefs<'a>(pub &'a User, pub &'a ChatIdPartiality);

pub(crate) async fn grow_impl(repos: &repo::Repositories, incr: Incrementor, from_refs: FromRefs<'_>,
                              details_store: &DetailsStore) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let (from, chat_id) = (from_refs.0, from_refs.1);
    let name = utils::get_full_name(from);
    let user = repos.users.create_or_update(from.id, &name).await?;
    let days_since_registration = (Utc::now() - user.created_at).num_days() as u32;
    let increment = incr.growth_increment(from.id, chat_id.kind(), days_since_registration).await;
    let grow_result = repos.dicks.create_or_grow(from.id, chat_id, increment.total).await;
    let lang_code = LanguageCode::from_user(from);

    let (main_part, grow_details) = match grow_result {
        Ok(repo::GrowthResult { new_length, pos_in_top }) => {
            if let Err(e) = repos.ledger.record(chat_id, from.id, repo::LedgerCategory::Grow, increment.base, None).await {
                log::error!("couldn't record a ledger entry for a grow event ({}): {e}", from.id);
            }
            let event_key = if increment.total.is_negative() { "shrunk" } else { "grown" };
            let event_template = format!("commands.grow.direction.{event_key}");
            let event = t!(&event_template, locale = &lang_code);
            let answer = t!("commands.grow.result", locale = &lang_code,
                event = event, incr = increment.total.abs(), length = new_length).to_string();
            // the leaderboard position and any perks breakdown are deferred behind a "Dettagli"
            // button (see `details::maybe_deferred`) - the perks block already carries its own
            // leading blank line (see `Increment::perks_part_of_answer`), trimmed here so it
            // joins cleanly with the position line instead of doubling up.
            let position = pos_in_top.map(|pos| t!("commands.grow.position", locale = &lang_code, pos = pos).to_string());
            let perks_part = increment.perks_part_of_answer(&lang_code);
            let perks_detail = (!perks_part.is_empty()).then(|| perks_part.trim_start_matches('\n').to_string());
            let details = [position, perks_detail].into_iter().flatten().collect::<Vec<_>>();
            let details = (!details.is_empty()).then(|| details.join("\n\n"));
            (answer, details)
        },
        Err(e) => {
            let db_err = e.downcast::<sqlx::Error>()?;
            if let sqlx::Error::Database(e) = db_err {
                let text = e.code()
                    .filter(|c| c == TOMORROW_SQL_CODE)
                    .map(|_| t!("commands.grow.tomorrow", locale = &lang_code).to_string())
                    .ok_or(anyhow!(e))?;
                (text, None)
            } else {
                Err(db_err)?
            }
        }
    };
    let time_left_part = utils::date::get_time_till_next_day_string(&lang_code);
    let short_text = format!("{main_part}{time_left_part}");
    Ok(details::maybe_deferred(short_text, grow_details, Some(from.id), Some(details_store), &lang_code))
}

pub(crate) struct Top {
    pub lines: String,
    pub(crate) has_more_pages: bool,
}

impl Top {
    fn from(s: impl ToString) -> Self {
        Self {
            lines: s.to_string(),
            has_more_pages: false,
        }
    }

    fn with_more_pages(s: impl ToString) -> Self {
        Self {
            lines: s.to_string(),
            has_more_pages: true,
        }
    }
}

/// The position/name/"[+]"-growable parts shared by both `/top` views - only the trailing
/// `suffix` (battle stats for `TopView::Length`, the credit/debit breakdown for `TopView::Net`)
/// differs between them.
fn format_top_line(lang_code: &LanguageCode, from_id: UserId, i: usize, position: Option<i64>,
                   owner_uid: UID, owner_name: String, grown_at: chrono::DateTime<Utc>, value: i32, suffix: Option<String>) -> String {
    let escaped_name = Username::new(owner_name).escaped();
    let name = if from_id == <UID as Into<UserId>>::into(owner_uid) {
        format!("<u>{escaped_name}</u>")
    } else {
        escaped_name
    };
    let can_grow = Utc::now().num_days_from_ce() > grown_at.num_days_from_ce();
    let pos = position.unwrap_or((i+1) as i64);
    let mut line = t!("commands.top.line", locale = lang_code, n = pos, name = name, length = value).to_string();
    if let Some(suffix) = suffix {
        line.push_str(&suffix);
    }
    if can_grow {
        line.push_str(&t!("commands.top.can_grow_marker", locale = lang_code));
    };
    line
}

pub(crate) async fn top_impl(repos: &repo::Repositories, config: &config::AppConfig, from_refs: FromRefs<'_>,
                             page: Page, view: TopView) -> anyhow::Result<Top> {
    let (from, chat_id) = (from_refs.0, from_refs.1.kind());
    let lang_code = LanguageCode::from_user(from);
    let top_limit = config.top_limit;
    let offset = page * top_limit;
    let query_limit = top_limit + 1; // fetch +1 row to know whether more rows exist or not
    let (row_count, lines) = match view {
        TopView::Length => {
            let dicks = repos.dicks.get_top(&chat_id, offset, query_limit).await?;
            let lines = dicks.into_iter()
                .take(top_limit as usize)
                .enumerate()
                .map(|(i, d)| {
                    let suffix = (d.battles_total > 0).then(|| {
                        let win_rate = d.win_rate_percentage().round() as i64;
                        t!("commands.top.wr", locale = &lang_code, battles = d.battles_total, wr = win_rate).to_string()
                    });
                    format_top_line(&lang_code, from.id, i, d.position, d.owner_uid, d.owner_name, d.grown_at, d.length, suffix)
                })
                .collect::<Vec<String>>();
            (lines.len() as u32, lines)
        },
        TopView::Net => {
            let rows = repos.dicks.get_top_by_net(&chat_id, offset, query_limit).await?;
            // not every row is taken below (see `.take`), but `row_count` (used only to decide
            // `has_more_pages`) must reflect the full fetched batch, including the extra lookahead
            // row - so it's captured before `.take` rather than derived from `lines.len()`.
            let row_count = rows.len() as u32;
            let lines = rows.into_iter()
                .take(top_limit as usize)
                .enumerate()
                .map(|(i, r)| {
                    let main_line = format_top_line(&lang_code, from.id, i, r.position, r.owner_uid, r.owner_name, r.grown_at, r.net, None);
                    // `net - raw_length` is the net adjustment from loans: positive means `r` is,
                    // on balance, owed more than they owe (a creditor); negative means the
                    // opposite (a debtor). Zero needs no breakdown at all. Shown on its own
                    // italic line below the main one, rather than crammed in next to it.
                    let delta = r.net - r.raw_length;
                    let breakdown = match delta.cmp(&0) {
                        std::cmp::Ordering::Greater => Some(t!("commands.top.net_breakdown.creditor", locale = &lang_code,
                            ghei = r.raw_length, delta = delta).to_string()),
                        std::cmp::Ordering::Less => Some(t!("commands.top.net_breakdown.debtor", locale = &lang_code,
                            ghei = r.raw_length, delta = delta.abs()).to_string()),
                        std::cmp::Ordering::Equal => None,
                    };
                    match breakdown {
                        Some(breakdown) => format!("{main_line}\n{breakdown}"),
                        None => main_line,
                    }
                })
                .collect::<Vec<String>>();
            (row_count, lines)
        },
    };
    let has_more_pages = row_count > top_limit;

    let res = if lines.is_empty() {
        Top::from(t!("commands.top.empty", locale = &lang_code))
    } else {
        let title = match view {
            TopView::Length => t!("commands.top.title", locale = &lang_code).to_string(),
            TopView::Net => t!("commands.top.net_title", locale = &lang_code).to_string(),
        };
        let intro_part = match view {
            TopView::Length => String::default(),
            TopView::Net => format!("{}\n\n", t!("commands.top.net_intro", locale = &lang_code)),
        };
        let ending = t!("commands.top.ending", locale = &lang_code);
        let text = format!("{title}\n\n{intro_part}{}\n\n{ending}", lines.join("\n"));
        if has_more_pages {
            Top::with_more_pages(text)
        } else {
            Top::from(text)
        }
    };
    Ok(res)
}

/// Which figure `/top` ranks players by - plain `length`, or `length` netted against every debt/
/// credit position (see `repo::Dicks::get_top_by_net`). Reachable via a toggle button below the
/// usual ⬅️/➡️ pagination row (see `build_pagination_keyboard`), independent of which page is
/// currently shown.
#[derive(Copy, Clone, Debug, PartialEq, Eq, derive_more::Display)]
pub(crate) enum TopView {
    #[display("len")]
    Length,
    #[display("net")]
    Net,
}

impl TopView {
    fn toggled(self) -> Self {
        match self {
            TopView::Length => TopView::Net,
            TopView::Net => TopView::Length,
        }
    }
}

#[derive(derive_more::Display)]
#[display("{page}:{view}")]
pub(crate) struct TopCallbackData {
    page: u32,
    view: TopView,
}

impl CallbackDataWithPrefix for TopCallbackData {
    fn prefix() -> &'static str {
        "top"
    }
}

impl TryFrom<String> for TopCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.as_str().split(':');
        let page = callbacks::parse_part(&mut parts, &err, "page")?;
        let view = match parts.next() {
            Some("len") => TopView::Length,
            Some("net") => TopView::Net,
            _ => return Err(err.missing_part("view")),
        };
        Ok(Self { page, view })
    }
}

pub fn page_callback_filter(query: CallbackQuery) -> bool {
    TopCallbackData::check_prefix(query)
}

pub async fn page_callback_handler(bot: Bot, q: CallbackQuery,
                                   config: config::AppConfig, repos: repo::Repositories) -> HandlerResult {
    let edit_msg_req_params = callbacks::get_params_for_message_edit(&q)?;
    if !config.features.top_unlimited {
        return answer_callback_feature_disabled(bot, &q, edit_msg_req_params).await
    }

    let data = TopCallbackData::parse(&q).map_err(|e| anyhow!(e))?;
    let page = Page(data.page);
    let chat_id_kind = edit_msg_req_params.clone().into();
    let chat_id_partiality = ChatIdPartiality::Specific(chat_id_kind);
    let from_refs = FromRefs(&q.from, &chat_id_partiality);
    let top = top_impl(&repos, &config, from_refs, page, data.view).await?;

    let keyboard = build_pagination_keyboard(page, top.has_more_pages, data.view);
    let (answer_callback_query_result, edit_message_result) = match &edit_msg_req_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut edit_message_text_req = bot.edit_message_text(*chat_id, *message_id, top.lines);
            edit_message_text_req.parse_mode.replace(ParseMode::Html);
            edit_message_text_req.reply_markup.replace(keyboard);
            join(
                bot.answer_callback_query(&q.id).into_future(),
                edit_message_text_req.into_future().map_ok(|_| ())
            ).await
        },
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut edit_message_text_inline_req = bot.edit_message_text_inline(inline_message_id, top.lines);
            edit_message_text_inline_req.parse_mode.replace(ParseMode::Html);
            edit_message_text_inline_req.reply_markup.replace(keyboard);
            join(
                bot.answer_callback_query(&q.id).into_future(),
                edit_message_text_inline_req.into_future().map_ok(|_| ())
            ).await
        }
    };
    answer_callback_query_result.context(format!("failed to answer a callback query {q:?}"))?;
    edit_message_result.context(format!("failed to edit the message of {edit_msg_req_params:?}"))?;
    Ok(())
}

pub fn build_pagination_keyboard(page: Page, has_more_pages: bool, view: TopView) -> InlineKeyboardMarkup {
    let mut nav_row = Vec::new();
    if page > 0 {
        let data = TopCallbackData { page: page.0 - 1, view }.to_data_string();
        nav_row.push(InlineKeyboardButton::callback("⬅️", data))
    }
    if has_more_pages {
        let data = TopCallbackData { page: page.0 + 1, view }.to_data_string();
        nav_row.push(InlineKeyboardButton::callback("➡️", data))
    }
    // switching views resets to the first page - the two rankings are unrelated orderings, so a
    // page number from one means nothing in the other.
    let toggle_label = match view.toggled() {
        TopView::Length => "🔢",
        TopView::Net => "💰",
    };
    let toggle_data = TopCallbackData { page: 0, view: view.toggled() }.to_data_string();
    let toggle_row = vec![InlineKeyboardButton::callback(toggle_label, toggle_data)];
    // Telegram rejects an empty button row, which `nav_row` can be on a single-page leaderboard.
    let rows = if nav_row.is_empty() { vec![toggle_row] } else { vec![nav_row, toggle_row] };
    InlineKeyboardMarkup::new(rows)
}

async fn answer_callback_feature_disabled(bot: Bot, q: &CallbackQuery, edit_msg_req_params: callbacks::EditMessageReqParamsKind) -> HandlerResult {
    let lang_code = LanguageCode::from_user(&q.from);

    let mut answer = bot.answer_callback_query(&q.id);
    answer.show_alert.replace(true);
    answer.text.replace(t!("errors.feature_disabled", locale = &lang_code).to_string());
    answer.await?;

    match edit_msg_req_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) =>
            bot.edit_message_reply_markup(chat_id, message_id)
                .await.map(|_| ())?,
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } =>
            bot.edit_message_reply_markup_inline(inline_message_id)
                .await.map(|_| ())?
    };
    Ok(())
}
