use std::str::FromStr;
use rust_i18n::t;
use strum_macros::EnumString;
use teloxide::Bot;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputMessageContentText, ParseMode, UserId};
use crate::config::AppConfig;
use crate::domain::LanguageCode;
use crate::handlers::{donate, p2p_loan, pvp, offer_keyboard, HandlerResult};
use crate::handlers::utils::callbacks::{self, CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::get_full_name;
use crate::repo;
use crate::check_invoked_by_owner_and_get_answer_params;

/// Which of the three "default-amount" listone entries (see `inline::EXTERNAL_VARIANTS`) a chip
/// belongs to - the picker carries only the chosen amount, never a target or probability/rate
/// (those stay reachable exclusively via the existing free-text inline syntax for power users).
#[derive(Clone, Copy, PartialEq, Eq, strum_macros::Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum OfferKind {
    Pvp,
    Donate,
    Presta,
}

#[derive(derive_more::Display)]
#[display("{uid}:{kind}:{amount}")]
pub(crate) struct AmountPickerCallbackData {
    uid: UserId,
    kind: OfferKind,
    amount: i32,
}

impl CallbackDataWithPrefix for AmountPickerCallbackData {
    fn prefix() -> &'static str {
        "amtpick"
    }
}

impl TryFrom<String> for AmountPickerCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let uid = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let kind_str = parts.next().ok_or_else(|| err.missing_part("kind"))?;
        let kind = OfferKind::from_str(kind_str).map_err(|e| err.parsing_err(e))?;
        let amount = callbacks::parse_part(&mut parts, &err, "amount")?;
        Ok(Self { uid, kind, amount })
    }
}

/// Sensible preset amounts around a configured `default`: half (floored at 1), the default
/// itself, double, and quintuple - deduplicated, since a tiny default (e.g. `1`) would otherwise
/// repeat the same value after halving.
fn amount_presets(default: u16) -> Vec<u16> {
    let mut presets = vec![(default / 2).max(1), default, default.saturating_mul(2), default.saturating_mul(5)];
    presets.sort_unstable();
    presets.dedup();
    presets
}

fn build_picker_result(result_id: &str, kind: OfferKind, presets: &[i32], uid: UserId, lang_code: &LanguageCode, title_key: &str, hint: String) -> InlineQueryResult {
    let title = t!(title_key, locale = lang_code).to_string();
    let content = InputMessageContent::Text(InputMessageContentText::new(&hint));
    let buttons = presets.iter()
        .map(|&amount| {
            let label = t!("inline.amount_picker.preset_button", locale = lang_code, amount = amount).to_string();
            let data = AmountPickerCallbackData { uid, kind, amount }.to_data_string();
            InlineKeyboardButton::callback(label, data)
        })
        .collect::<Vec<_>>();
    InlineQueryResultArticle::new(result_id, title, content)
        .reply_markup(teloxide::types::InlineKeyboardMarkup::new(vec![buttons]))
        .into()
}

pub(super) fn build_pvp_picker_result(uid: UserId, lang_code: &LanguageCode, app_config: &AppConfig) -> InlineQueryResult {
    let presets = amount_presets(app_config.pvp_default_bet).into_iter().map(i32::from).collect::<Vec<_>>();
    let hint = t!("inline.amount_picker.hint.pvp", locale = lang_code).to_string();
    build_picker_result("pvp-picker", OfferKind::Pvp, &presets, uid, lang_code, "inline.results.titles.pvp_picker", hint)
}

pub(super) fn build_donate_picker_result(uid: UserId, lang_code: &LanguageCode, app_config: &AppConfig) -> InlineQueryResult {
    let presets = amount_presets(app_config.donate_default_amount).into_iter().map(i32::from).collect::<Vec<_>>();
    let hint = t!("inline.amount_picker.hint.donate", locale = lang_code).to_string();
    build_picker_result("donate-picker", OfferKind::Donate, &presets, uid, lang_code, "inline.results.titles.donate_picker", hint)
}

pub(super) fn build_presta_picker_result(uid: UserId, lang_code: &LanguageCode, app_config: &AppConfig) -> InlineQueryResult {
    let rate_pct = p2p_loan::rate_to_pct(app_config.p2p_loan_interest_rate);
    // only offer a preset whose interest is actually computable at the configured rate - a
    // misconfigured rate that's too high for the larger presets simply drops them rather than
    // showing a button that would fail on tap.
    let presets = amount_presets(app_config.p2p_loan_default_amount).into_iter()
        .filter(|&amount| repo::compute_interest(amount, app_config.p2p_loan_interest_rate).is_some())
        .map(i32::from)
        .collect::<Vec<_>>();
    let hint = t!("inline.amount_picker.hint.presta", locale = lang_code, rate = rate_pct).to_string();
    build_picker_result("presta-picker", OfferKind::Presta, &presets, uid, lang_code, "inline.results.titles.presta_picker", hint)
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    AmountPickerCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, app_config: AppConfig) -> HandlerResult {
    let data = AmountPickerCallbackData::parse(&query)?;
    let (answer, lang_code) = check_invoked_by_owner_and_get_answer_params!(bot, query, data.uid);
    let name = get_full_name(&query.from);

    let (text, keyboard) = match data.kind {
        OfferKind::Pvp => {
            let bet = data.amount.clamp(1, u16::MAX as i32) as u16;
            let text = pvp::battle_offer_text(&name, None, bet, None, &lang_code);
            let btn_label = t!("commands.pvp.button", locale = &lang_code).to_string();
            let btn_data = pvp::BattleCallbackData::new(data.uid, bet, None, None).to_data_string();
            let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
            (text, Some(offer_keyboard(accept_btn, data.uid, None, &lang_code)))
        },
        OfferKind::Donate => {
            let text = donate::donate_offer_text(&name, None, data.amount, &lang_code);
            let btn_label = t!("commands.donate.button", locale = &lang_code).to_string();
            let btn_data = donate::DonateCallbackData::new(data.uid, data.amount, None).to_data_string();
            let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
            (text, Some(offer_keyboard(accept_btn, data.uid, None, &lang_code)))
        },
        OfferKind::Presta => {
            let (abs_amount, _) = donate::split_amount(data.amount);
            let rate_pct = p2p_loan::rate_to_pct(app_config.p2p_loan_interest_rate);
            // the amount was only ever offered as a preset after passing this same check in
            // `build_presta_picker_result`, so this should always succeed - `None` here only
            // means the configured rate changed in the (narrow) window between the two calls,
            // in which case showing an explicit error beats silently defaulting to 0% interest.
            match repo::compute_interest(abs_amount, app_config.p2p_loan_interest_rate) {
                None => (t!("commands.presta.errors.rate_too_high", locale = &lang_code, rate = rate_pct, amount = abs_amount).to_string(), None),
                Some(interest) => {
                    let text = p2p_loan::p2p_loan_offer_text(&name, None, data.amount, rate_pct, interest, &lang_code);
                    let btn_label = t!("commands.presta.button", locale = &lang_code).to_string();
                    let btn_data = p2p_loan::P2PLoanCallbackData::new(data.uid, data.amount, None, None).to_data_string();
                    let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
                    (text, Some(offer_keyboard(accept_btn, data.uid, None, &lang_code)))
                }
            }
        },
    };

    let edit_params = callbacks::get_params_for_message_edit(&query)?;
    match edit_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut req = bot.edit_message_text(chat_id, message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup = keyboard;
            req.await?;
        }
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut req = bot.edit_message_text_inline(inline_message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup = keyboard;
            req.await?;
        }
    }
    answer.await?;
    Ok(())
}
