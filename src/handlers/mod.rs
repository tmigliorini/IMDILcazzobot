mod dick;
mod help;
mod start;
mod privacy;
mod dod;
mod import;
mod promo;
mod inline;
pub mod utils;
pub mod pvp;
pub mod donate;
pub mod perks;
pub mod loan;
pub mod stats;
pub mod tax;
pub mod syntax;
pub mod p2p_loan;
pub mod combo;
pub mod debt_settlement;
pub mod statement;
pub mod info;
pub mod amount_picker;
pub mod details;
pub mod wizard;

use derive_more::Constructor;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::payloads::{AnswerCallbackQuerySetters, SendMessage, SendMessageSetters};
use teloxide::requests::{JsonRequest, Requester};
use teloxide::sugar::request::RequestLinkPreviewExt;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, Message, ReplyParameters, UserId};
use teloxide::types::ParseMode::Html;

pub use dick::*;
pub use help::*;
pub use start::*;
pub use privacy::*;
pub use dod::*;
pub use import::*;
pub use inline::*;
pub use promo::*;
pub use loan::LoanCommands;
use crate::domain::LanguageCode;
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};

pub type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub enum CallbackResult {
    EditMessage(String, Option<InlineKeyboardMarkup>),
    ShowError(String),
}

impl CallbackResult {
    pub async fn apply(self, bot: Bot, callback_query: CallbackQuery) -> anyhow::Result<()> {
        let answer_req = bot.answer_callback_query(callback_query.id);
        match self {
            CallbackResult::EditMessage(text, keyboard) => {
                if let Some(message) = callback_query.message {
                    let mut edit_req = bot.edit_message_text(message.chat().id, message.id(), text);
                    edit_req.parse_mode.replace(Html);
                    edit_req.reply_markup = keyboard;

                    let edit_req_resp = edit_req.await;
                    if let Err(err) = edit_req_resp {
                        log::error!("couldn't edit the message ({}:{}): {}", message.chat().id, message.id(), err);
                        Err(err)?;
                    }
                } else if let Some(inline_message_id) = callback_query.inline_message_id {
                    let mut edit_req = bot.edit_message_text_inline(&inline_message_id, text);
                    edit_req.parse_mode.replace(Html);
                    edit_req.reply_markup = keyboard;

                    let edit_req_resp = edit_req.await;
                    if let Err(err) = edit_req_resp {
                        log::error!("couldn't edit the message ({}): {}", inline_message_id, err);
                        Err(err)?;
                    }
                };
                answer_req.await?;
            },
            CallbackResult::ShowError(err) => {
                answer_req
                    .text(err)
                    .show_alert(true)
                    .await?;
            }
        };
        Ok(())
    }
}

pub enum HandlerImplResult<D: CallbackDataWithPrefix> {
    WithKeyboard {
        text: String,
        buttons: Vec<CallbackButton<D>>
    },
    OnlyText(String)
}

#[derive(Constructor)]
pub struct CallbackButton<D: CallbackDataWithPrefix> {
    title: String,
    data: D,
}

impl <D: CallbackDataWithPrefix> HandlerImplResult<D> {
    pub fn text(&self) -> String {
        match self {
            HandlerImplResult::WithKeyboard { text, .. } => text,
            HandlerImplResult::OnlyText(text) => text
        }.clone()
    }

    pub fn keyboard(&self) -> Option<InlineKeyboardMarkup> {
        match self {
            HandlerImplResult::WithKeyboard { buttons, .. } => {
                let buttons = buttons.iter()
                    .map(|btn| InlineKeyboardButton::callback(btn.title.clone(), btn.data.to_data_string()));
                let keyboard = InlineKeyboardMarkup::new(vec![buttons]);
                Some(keyboard)
            }
            HandlerImplResult::OnlyText(_) => None
        }
    }
}

pub fn reply_html<T: Into<String>>(bot: Bot, msg: &Message, answer: T) -> JsonRequest<SendMessage> {
    // TODO: split to several messages if the answer is too long
    let mut answer = bot.send_message(msg.chat.id, answer)
        .parse_mode(Html)
        .disable_link_preview(true);
    if msg.chat.is_group() || msg.chat.is_supergroup() {
        answer.reply_parameters.replace(ReplyParameters::new(msg.id));
    }
    answer
}

#[macro_export]
macro_rules! reply_html {
    ($bot:ident, $msg:ident, $answer:expr) => {
        anyhow::Context::context(
            reply_html($bot, &$msg, $answer).await,
            format!("failed for {:?}", $msg)
        )?
    };
}

pub async fn send_error_callback_answer(bot: Bot, query: CallbackQuery, tr_key: &str) -> HandlerResult {
    let lang_code = LanguageCode::from_user(&query.from);
    bot.answer_callback_query(query.id)
        .show_alert(true)
        .text(t!(tr_key, locale = &lang_code))
        .await?;
    Ok(())
}

/// Shared by every "offer extended to someone else" action (pvp, donate, presta): a second,
/// secondary button next to the main accept one, letting the proposer (and only them) retract
/// their own offer before anyone accepts it. Purely a UI action - no funds are ever held in
/// escrow while an offer is pending, so there's nothing to undo on the data side.
#[derive(derive_more::Display)]
#[display("{proposer}")]
pub(crate) struct CancelOfferCallbackData {
    proposer: UserId,
}

impl CancelOfferCallbackData {
    pub(crate) fn new(proposer: UserId) -> Self {
        Self { proposer }
    }
}

impl CallbackDataWithPrefix for CancelOfferCallbackData {
    fn prefix() -> &'static str {
        "cancel-offer"
    }
}

impl TryFrom<String> for CancelOfferCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let proposer = data.parse::<u64>().map(UserId).map_err(|e| err.parsing_err(e))?;
        Ok(Self { proposer })
    }
}

/// A button for [`CancelOfferCallbackData`], to be added as its own row below the main action
/// button of any offer extended to someone else.
pub(crate) fn cancel_offer_button(proposer: UserId, lang_code: &LanguageCode) -> InlineKeyboardButton {
    let label = t!("inline.callback.cancel_button", locale = lang_code);
    InlineKeyboardButton::callback(label, CancelOfferCallbackData::new(proposer).to_data_string())
}

#[inline]
pub fn cancel_offer_callback_filter(query: CallbackQuery) -> bool {
    CancelOfferCallbackData::check_prefix(query)
}

pub async fn cancel_offer_callback_handler(bot: Bot, query: CallbackQuery) -> HandlerResult {
    let callback_data = CancelOfferCallbackData::parse(&query)?;
    if callback_data.proposer != query.from.id {
        return send_error_callback_answer(bot, query, "inline.callback.errors.another_user").await
    }
    let lang_code = LanguageCode::from_user(&query.from);
    let text = t!("inline.callback.offer_cancelled", locale = &lang_code).to_string();
    CallbackResult::EditMessage(text, None).apply(bot, query).await?;
    Ok(())
}

/// Symmetric to [`CancelOfferCallbackData`], but for the other side of a targeted offer: lets
/// the target (and only them) explicitly decline before accepting, rather than just leaving the
/// offer unclicked. Only ever shown for targeted offers - an open offer has no single "target"
/// who could meaningfully reject it on everyone's behalf.
#[derive(derive_more::Display)]
#[display("{target}")]
pub(crate) struct RejectOfferCallbackData {
    target: UserId,
}

impl RejectOfferCallbackData {
    pub(crate) fn new(target: UserId) -> Self {
        Self { target }
    }
}

impl CallbackDataWithPrefix for RejectOfferCallbackData {
    fn prefix() -> &'static str {
        "reject-offer"
    }
}

impl TryFrom<String> for RejectOfferCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let target = data.parse::<u64>().map(UserId).map_err(|e| err.parsing_err(e))?;
        Ok(Self { target })
    }
}

/// A button for [`RejectOfferCallbackData`], meant to sit right next to the main action button
/// (not on its own row, unlike the cancel button) so a targeted offer reads as one Accept/Reject
/// choice, with Cancel (for the proposer) on the row below.
pub(crate) fn reject_offer_button(target: UserId, lang_code: &LanguageCode) -> InlineKeyboardButton {
    let label = t!("inline.callback.reject_button", locale = lang_code);
    InlineKeyboardButton::callback(label, RejectOfferCallbackData::new(target).to_data_string())
}

#[inline]
pub fn reject_offer_callback_filter(query: CallbackQuery) -> bool {
    RejectOfferCallbackData::check_prefix(query)
}

pub async fn reject_offer_callback_handler(bot: Bot, query: CallbackQuery) -> HandlerResult {
    let callback_data = RejectOfferCallbackData::parse(&query)?;
    if callback_data.target != query.from.id {
        return send_error_callback_answer(bot, query, "inline.callback.errors.another_user").await
    }
    let lang_code = LanguageCode::from_user(&query.from);
    let text = t!("inline.callback.offer_rejected", locale = &lang_code).to_string();
    CallbackResult::EditMessage(text, None).apply(bot, query).await?;
    Ok(())
}

/// The standard two-row layout for any offer extended to someone else: the main action button
/// (and, when targeted, a reject button right next to it) on top, with the proposer's own
/// cancel/retract button alone on the row below.
pub(crate) fn offer_keyboard(accept_button: InlineKeyboardButton, proposer: UserId, target: Option<UserId>, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let mut top_row = vec![accept_button];
    if let Some(target) = target {
        top_row.push(reject_offer_button(target, lang_code));
    }
    InlineKeyboardMarkup::new(vec![
        top_row,
        vec![cancel_offer_button(proposer, lang_code)],
    ])
}

pub mod checks {
    use rust_i18n::t;
    use teloxide::Bot;
    use teloxide::types::Message;
    use crate::domain::LanguageCode;
    use super::{HandlerResult, reply_html};

    pub fn is_group_chat(msg: Message) -> bool {
        if msg.chat.is_private() || msg.chat.is_channel() {
            return false
        }
        true
    }

    pub fn is_not_group_chat(msg: Message) -> bool {
        !is_group_chat(msg)
    }

    pub async fn handle_not_group_chat(bot: Bot, msg: Message) -> HandlerResult {
        let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
        let answer = t!("errors.not_group_chat", locale = &lang_code);
        reply_html(bot, &msg, answer).await?;
        Ok(())
    }

    pub mod inline {
        use teloxide::Bot;
        use teloxide::payloads::AnswerInlineQuerySetters;
        use teloxide::prelude::{InlineQuery, Requester};
        use teloxide::types::ChatType;
        use super::HandlerResult;

        pub fn is_group_chat(query: InlineQuery) -> bool {
            query.chat_type
                .map(|t| [ChatType::Group, ChatType::Supergroup].contains(&t))
                .unwrap_or(false)
        }

        pub fn is_not_group_chat(query: InlineQuery) -> bool {
            !is_group_chat(query)
        }

        pub async fn handle_not_group_chat(bot: Bot, query: InlineQuery) -> HandlerResult {
            bot.answer_inline_query(query.id, vec![])
                .is_personal(true)
                .cache_time(1)
                .await?;
            Ok(())
        }
    }
}
