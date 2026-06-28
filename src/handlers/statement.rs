use std::future::IntoFuture;

use anyhow::{anyhow, Context};
use derive_more::Display;
use futures::future::join;
use futures::TryFutureExt;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, ParseMode, ReplyMarkup, UserId};

use crate::{check_invoked_by_owner_and_get_answer_params, metrics, repo};
use crate::domain::{LanguageCode, Username};
use crate::handlers::{FromRefs, HandlerResult, reply_html};
use crate::handlers::utils::callbacks::{self, CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::page::Page;
use crate::repo::{ChatIdPartiality, LedgerCategory, LedgerEntry};

const CALLBACK_PREFIX: &str = "stmt";
/// Comfortably under Telegram's 4096-character message limit even with the longest lines
/// (counterparty name + category + date/time).
const STATEMENT_PAGE_SIZE: u16 = 8;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum StatementCommands {
    #[command(description = "estratto")]
    Estratto,
}

pub async fn cmd_handler(bot: Bot, msg: Message, repos: repo::Repositories) -> HandlerResult {
    metrics::CMD_STATEMENT_COUNTER.chat.inc();

    let from = msg.from.as_ref().ok_or(anyhow!("unexpected absence of a FROM field"))?;
    let lang_code = LanguageCode::from_user(from);
    let chat_id = msg.chat.id.into();
    let from_refs = FromRefs(from, &chat_id);

    let statement = statement_impl(&repos, from_refs, Page::first()).await?;
    let mut request = reply_html(bot, &msg, statement.lines);
    if statement.has_more_pages {
        let keyboard = build_pagination_keyboard(from.id, Page::first(), statement.has_more_pages, &lang_code);
        request.reply_markup.replace(ReplyMarkup::InlineKeyboard(keyboard));
    }
    request.await.context(format!("failed for {msg:?}"))?;
    Ok(())
}

pub(crate) struct Statement {
    pub(crate) lines: String,
    pub(crate) has_more_pages: bool,
}

pub(crate) async fn statement_impl(repos: &repo::Repositories, from_refs: FromRefs<'_>, page: Page) -> anyhow::Result<Statement> {
    let (from, chat_id) = (from_refs.0, from_refs.1);
    let lang_code = LanguageCode::from_user(from);
    let offset = page * (STATEMENT_PAGE_SIZE as u32);
    let query_limit = STATEMENT_PAGE_SIZE + 1; // fetch +1 row to know whether more rows exist or not
    let balance = repos.dicks.fetch_length(from.id, &chat_id.kind()).await?;
    let entries = repos.ledger.get_page(chat_id, from.id, offset, query_limit).await?;
    let has_more_pages = entries.len() as u16 > STATEMENT_PAGE_SIZE;
    let entries = entries.into_iter().take(STATEMENT_PAGE_SIZE as usize).collect::<Vec<_>>();

    let title = t!("commands.estratto.title", locale = &lang_code, balance = balance);
    let body = if entries.is_empty() {
        t!("commands.estratto.empty", locale = &lang_code).to_string()
    } else {
        group_by_datetime(&entries).into_iter()
            .map(|(datetime, group)| {
                let header = t!("commands.estratto.group_header", locale = &lang_code, datetime = datetime);
                let lines = group.into_iter().map(|entry| format_entry(entry, &lang_code)).collect::<Vec<_>>();
                format!("{header}\n{}", lines.join("\n"))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    Ok(Statement { lines: format!("{title}\n\n{body}"), has_more_pages })
}

/// Groups consecutive entries (already newest-first from `get_page`) that share the same
/// *displayed* datetime - e.g. every row a single tax-debt redistribution touched at once - so
/// the caller can print one timestamp per group instead of repeating it on every line, which
/// would otherwise look like N unrelated events rather than one. Keyed by the formatted string
/// rather than raw equality of `created_at`: rows from the same logical event can differ by a
/// few milliseconds if they were inserted via separate statements (see `Ledger::record_many`),
/// but they still render to the same minute-precision string.
fn group_by_datetime(entries: &[LedgerEntry]) -> Vec<(String, Vec<&LedgerEntry>)> {
    let mut groups: Vec<(String, Vec<&LedgerEntry>)> = Vec::new();
    for entry in entries {
        let datetime = format_datetime(entry);
        match groups.last_mut() {
            Some((last_datetime, group)) if *last_datetime == datetime => group.push(entry),
            _ => groups.push((datetime, vec![entry])),
        }
    }
    groups
}

fn format_datetime(entry: &LedgerEntry) -> String {
    entry.created_at.with_timezone(&chrono_tz::Europe::Rome).format("%d/%m %H:%M").to_string()
}

fn format_entry(entry: &LedgerEntry, lang_code: &LanguageCode) -> String {
    let category_t_key = format!("commands.estratto.categories.{}", category_key(entry.category));
    let category = t!(&category_t_key, locale = lang_code);
    let amount = format!("{:+}", entry.amount);
    match &entry.counterparty {
        Some((_, name)) => {
            let counterparty = Username::new(name.clone()).escaped();
            t!("commands.estratto.line_with_counterparty", locale = lang_code,
                category = category, amount = amount, counterparty = counterparty).to_string()
        }
        None => t!("commands.estratto.line", locale = lang_code,
            category = category, amount = amount).to_string()
    }
}

fn category_key(category: LedgerCategory) -> &'static str {
    match category {
        LedgerCategory::Grow => "grow",
        LedgerCategory::Pvp => "pvp",
        LedgerCategory::Donate => "donate",
        LedgerCategory::LoanInterest => "loan_interest",
        LedgerCategory::LoanPrincipal => "loan_principal",
        LedgerCategory::Tax => "tax",
    }
}

/// Unlike `dick::build_pagination_keyboard` (shared, no owner check - a chat-wide leaderboard
/// looks the same to everyone), this statement is personal data: the callback data carries `uid`
/// so `callback_handler` can reject anyone else's clicks, the same way `LoanCallbackData` does.
/// Always carries a second row back to the Info menu (see `info::back_button`) underneath the
/// pagination row, on *every* page - not just the first one reached from that menu - so paging
/// through a long statement never strands the player without a way back.
pub(crate) fn build_pagination_keyboard(uid: UserId, page: Page, has_more_pages: bool, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();
    if page > 0 {
        let data = StatementCallbackData { uid, page: page.0 - 1 }.to_data_string();
        buttons.push(InlineKeyboardButton::callback("⬅️", data));
    }
    // re-requests this exact page as-is, for a fresher statement without losing position.
    let refresh_data = StatementCallbackData { uid, page: page.0 }.to_data_string();
    buttons.push(InlineKeyboardButton::callback("🔄", refresh_data));
    if has_more_pages {
        let data = StatementCallbackData { uid, page: page.0 + 1 }.to_data_string();
        buttons.push(InlineKeyboardButton::callback("➡️", data));
    }
    InlineKeyboardMarkup::new(vec![buttons]).append_row(vec![crate::handlers::info::back_button(uid, lang_code)])
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    StatementCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: repo::Repositories) -> HandlerResult {
    let data = StatementCallbackData::parse(&query)?;
    let (answer, lang_code) = check_invoked_by_owner_and_get_answer_params!(bot, query, data.uid);

    let edit_msg_req_params = callbacks::get_params_for_message_edit(&query)?;
    let chat_id_kind = edit_msg_req_params.clone().into();
    let chat_id_partiality = ChatIdPartiality::Specific(chat_id_kind);
    let from_refs = FromRefs(&query.from, &chat_id_partiality);
    let page = Page(data.page);
    let statement = statement_impl(&repos, from_refs, page).await?;

    let keyboard = build_pagination_keyboard(data.uid, page, statement.has_more_pages, &lang_code);
    let (answer_callback_query_result, edit_message_result) = match &edit_msg_req_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut edit_message_text_req = bot.edit_message_text(*chat_id, *message_id, statement.lines);
            edit_message_text_req.parse_mode.replace(ParseMode::Html);
            edit_message_text_req.reply_markup.replace(keyboard);
            join(
                answer.into_future(),
                edit_message_text_req.into_future().map_ok(|_| ())
            ).await
        },
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut edit_message_text_inline_req = bot.edit_message_text_inline(inline_message_id, statement.lines);
            edit_message_text_inline_req.parse_mode.replace(ParseMode::Html);
            edit_message_text_inline_req.reply_markup.replace(keyboard);
            join(
                answer.into_future(),
                edit_message_text_inline_req.into_future().map_ok(|_| ())
            ).await
        }
    };
    answer_callback_query_result.context(format!("failed to answer a callback query {query:?}"))?;
    edit_message_result.context(format!("failed to edit the message of {edit_msg_req_params:?}"))?;
    Ok(())
}

#[derive(Display)]
#[display("{uid}:{page}")]
struct StatementCallbackData {
    uid: UserId,
    page: u32,
}

impl CallbackDataWithPrefix for StatementCallbackData {
    fn prefix() -> &'static str {
        CALLBACK_PREFIX
    }
}

impl TryFrom<String> for StatementCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.as_str().split(':');
        let uid = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let page = callbacks::parse_part(&mut parts, &err, "page")?;
        Ok(Self { uid, page })
    }
}

#[cfg(test)]
mod test {
    use teloxide::types::{CallbackQuery, User, UserId};
    use crate::handlers::utils::callbacks::CallbackDataWithPrefix;
    use super::StatementCallbackData;

    #[test]
    fn test_parse_and_serialize() {
        let data = StatementCallbackData { uid: UserId(123456), page: 2 };
        let serialized = data.to_data_string();
        assert_eq!(serialized, "stmt:123456:2");

        let query = build_callback_query(serialized);
        let parsed = StatementCallbackData::parse(&query).expect("must parse a well-formed callback");
        assert_eq!(parsed.uid, UserId(123456));
        assert_eq!(parsed.page, 2);
    }

    /// `callback_handler` rejects a click from anyone but `uid` via the shared
    /// `check_invoked_by_owner_and_get_answer_params!` macro - this only checks that the
    /// callback's own `uid` field round-trips correctly, since the macro itself is already
    /// covered by `loan.rs`'s tests.
    #[test]
    fn test_uid_is_preserved_for_the_owner_check() {
        let data = StatementCallbackData { uid: UserId(42), page: 0 };
        let query = build_callback_query(data.to_data_string());
        let parsed = StatementCallbackData::parse(&query).expect("must parse a well-formed callback");
        assert_eq!(parsed.uid, UserId(42), "the owner check relies on this uid matching the original requester");
    }

    fn build_callback_query(data: String) -> CallbackQuery {
        CallbackQuery {
            id: "".to_string(),
            from: User {
                id: UserId(0),
                is_bot: false,
                first_name: "".to_string(),
                last_name: None,
                username: None,
                language_code: None,
                is_premium: false,
                added_to_attachment_menu: false,
            },
            message: None,
            inline_message_id: None,
            chat_instance: "".to_string(),
            data: Some(data),
            game_short_name: None,
        }
    }
}
