use rust_i18n::t;
use teloxide::Bot;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, UserId};
use crate::domain::LanguageCode;
use crate::handlers::HandlerResult;
use crate::handlers::utils::callbacks::{self, CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::details_store::DetailsStore;

#[derive(derive_more::Display)]
#[display("{token}")]
struct DetailsCallbackData {
    token: String,
}

impl CallbackDataWithPrefix for DetailsCallbackData {
    fn prefix() -> &'static str {
        "details"
    }
}

impl TryFrom<String> for DetailsCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        if data.is_empty() {
            return Err(InvalidCallbackDataBuilder(&data).missing_part("token"));
        }
        Ok(Self { token: data })
    }
}

fn build_details_button(token: &str, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let label = t!("inline.details_button", locale = lang_code).to_string();
    let data = DetailsCallbackData { token: token.to_owned() }.to_data_string();
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(label, data)]])
}

/// Wraps `short_text` with a "📊 Dettagli" button over `details` whenever both a store is given
/// and there's actually something to defer; otherwise falls back to showing everything inline
/// with no button - the same shape `pvp_finish`/`donate_finish`/`p2p_loan_finish` already returned
/// before this existed. `crate::handlers::combo` calls this once on its own combined two-leg
/// `(short_text, details)` pair, rather than once per leg, so a combo result gets a single
/// Dettagli button covering both legs instead of two duplicated detail blocks.
pub(crate) fn maybe_deferred(short_text: String, details: Option<String>, owner: Option<UserId>,
                             store: Option<&DetailsStore>, lang_code: &LanguageCode) -> (String, Option<InlineKeyboardMarkup>) {
    match (store, details) {
        (Some(store), Some(details)) => {
            let full_text = format!("{short_text}\n\n{details}");
            let token = store.insert(owner, full_text);
            (short_text, Some(build_details_button(&token, lang_code)))
        },
        (_, Some(details)) => (format!("{short_text}\n\n{details}"), None),
        (_, None) => (short_text, None),
    }
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    DetailsCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, store: DetailsStore) -> HandlerResult {
    let data = DetailsCallbackData::parse(&query)?;
    let lang_code = LanguageCode::from_user(&query.from);

    let mut answer = bot.answer_callback_query(&query.id);
    let text = match store.get(&data.token, query.from.id) {
        Some(text) => text,
        None => {
            answer.show_alert.replace(true);
            answer.text.replace(t!("inline.callback.errors.invalid_data", locale = &lang_code).to_string());
            answer.await?;
            return Ok(());
        }
    };

    let edit_params = callbacks::get_params_for_message_edit(&query)?;
    match edit_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut req = bot.edit_message_text(chat_id, message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.await?;
        }
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut req = bot.edit_message_text_inline(inline_message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.await?;
        }
    }
    answer.await?;
    Ok(())
}
