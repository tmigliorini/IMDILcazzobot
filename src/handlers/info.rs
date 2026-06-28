use std::str::FromStr;
use rust_i18n::t;
use strum_macros::{EnumIter, EnumString};
use strum::IntoEnumIterator;
use teloxide::Bot;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, UserId};
use crate::config::AppConfig;
use crate::domain::LanguageCode;
use crate::external_text::ExternalTexts;
use crate::handlers::{p2p_loan, stats, statement, utils, FromRefs, HandlerResult};
use crate::handlers::utils::callbacks::{self, CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::page::Page;
use crate::help::HelpContainer;
use crate::repo::Repositories;
use crate::{check_invoked_by_owner_and_get_answer_params, metrics};

/// Five purely-informational entries that used to be their own top-level listone items -
/// consolidated behind a single "ℹ️ Info" entry (see `inline::InlineCommand`, which dropped them).
/// `Tax` stays out of this menu (and a top-level `InlineCommand` variant instead): unlike these
/// five, it actually executes a real redistribution, so it needs its own confirm round-trip
/// rather than being one tap inside something labeled "Info".
#[derive(Debug, Clone, Copy, strum_macros::Display, EnumIter, EnumString)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum InfoSection {
    Stats,
    Debiti,
    Estratto,
    Syntax,
    Presentation,
    Report,
}

/// `section: None` means "show the 6-button menu itself" - used by the "🔙 Indietro" button to
/// navigate back from a section to the menu.
pub(crate) struct InfoCallbackData {
    uid: UserId,
    section: Option<InfoSection>,
}

impl std::fmt::Display for InfoCallbackData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.section {
            Some(section) => write!(f, "{}:{section}", self.uid),
            None => write!(f, "{}:", self.uid),
        }
    }
}

impl CallbackDataWithPrefix for InfoCallbackData {
    fn prefix() -> &'static str {
        "info"
    }
}

impl TryFrom<String> for InfoCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let uid = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let section = parts.next()
            .filter(|s| !s.is_empty())
            .map(|s| InfoSection::from_str(s).map_err(|e| err.parsing_err(e)))
            .transpose()?;
        Ok(Self { uid, section })
    }
}

pub(crate) fn build_menu_keyboard(uid: UserId, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let buttons: Vec<InlineKeyboardButton> = InfoSection::iter()
        .map(|section| {
            let key = format!("inline.info_menu.titles.{section}");
            let label = t!(&key, locale = lang_code).to_string();
            let data = InfoCallbackData { uid, section: Some(section) }.to_data_string();
            InlineKeyboardButton::callback(label, data)
        })
        .collect();
    let rows = buttons.chunks(2).map(<[InlineKeyboardButton]>::to_vec).collect::<Vec<_>>();
    InlineKeyboardMarkup::new(rows)
}

fn build_back_keyboard(uid: UserId, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let label = t!("inline.info_menu.back_button", locale = lang_code).to_string();
    let data = InfoCallbackData { uid, section: None }.to_data_string();
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(label, data)]])
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    InfoCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories, config: AppConfig,
                              help_container: HelpContainer, external_texts: ExternalTexts) -> HandlerResult {
    let data = InfoCallbackData::parse(&query)?;
    let (answer, lang_code) = check_invoked_by_owner_and_get_answer_params!(bot, query, data.uid);

    let (text, keyboard) = match data.section {
        None => (t!("inline.info_menu.intro", locale = &lang_code).to_string(), build_menu_keyboard(data.uid, &lang_code)),
        // `Estratto` gets its own pagination row instead of the shared back-button keyboard - it's
        // the one section whose content doesn't fit in a single page, and once the ⬅️/➡️ buttons
        // are tapped, navigation continues entirely through `statement::callback_handler` (a
        // separate "stmt"-prefixed callback, already wired in `main.rs`), independent of this menu.
        Some(InfoSection::Estratto) => {
            metrics::CMD_STATEMENT_COUNTER.inline.inc();
            let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);
            let from_refs = FromRefs(&query.from, &chat_id);
            let statement = statement::statement_impl(&repos, from_refs, Page::first()).await?;
            let keyboard = statement::build_pagination_keyboard(data.uid, Page::first(), statement.has_more_pages);
            (statement.lines, keyboard)
        },
        // same reasoning as `Estratto` above: `Debiti` can also span more than one page, so it
        // gets its own pagination row (continuing through `p2p_loan::debiti_callback_handler`)
        // instead of the shared back-button keyboard.
        Some(InfoSection::Debiti) => {
            metrics::CMD_P2P_LOAN_STATUS_COUNTER.inline.inc();
            let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);
            let from_refs = FromRefs(&query.from, &chat_id);
            let status = p2p_loan::p2p_loan_status_impl(&repos, from_refs, Page::first()).await?;
            let keyboard = p2p_loan::build_debiti_pagination_keyboard(data.uid, Page::first(), status.has_more_pages);
            (status.lines, keyboard)
        },
        Some(section) => {
            let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);
            let from_refs = FromRefs(&query.from, &chat_id);
            // each branch keeps incrementing the same counter its own standalone command does,
            // so moving these behind the Info sub-menu doesn't lose any per-feature metrics.
            let body = match section {
                InfoSection::Stats => {
                    metrics::CMD_STATS.inline.inc();
                    stats::chat_stats_impl(&repos, from_refs, config.features.pvp).await?
                },
                InfoSection::Debiti => unreachable!("handled in its own match arm above"),
                InfoSection::Estratto => unreachable!("handled in its own match arm above"),
                InfoSection::Syntax => {
                    metrics::CMD_SYNTAX_COUNTER.inline.inc();
                    external_texts.syntax.clone()
                },
                InfoSection::Presentation => {
                    metrics::CMD_HELP_COUNTER.inc();
                    help_container.get_help_message(LanguageCode::from_user(&query.from), &external_texts.intro)
                },
                InfoSection::Report => stats::chat_economy_report_impl(&repos, from_refs).await?,
            };
            (body, build_back_keyboard(data.uid, &lang_code))
        }
    };

    let edit_params = callbacks::get_params_for_message_edit(&query)?;
    match edit_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut req = bot.edit_message_text(chat_id, message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup.replace(keyboard);
            req.await?;
        }
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut req = bot.edit_message_text_inline(inline_message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup.replace(keyboard);
            req.await?;
        }
    }
    answer.await?;
    Ok(())
}
