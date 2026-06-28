use rust_i18n::t;
use teloxide::Bot;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode, UserId};
use crate::config::AppConfig;
use crate::domain::{LanguageCode, Username};
use crate::handlers::{combo, donate, p2p_loan, pvp, offer_keyboard, utils, HandlerResult};
use crate::handlers::amount_picker::OfferKind;
use crate::handlers::utils::callbacks::{self, CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::wizard_store::{LegState, WizardMode, WizardState, WizardStore};
use crate::repo::{self, ComboLeg, ComboOffer, Repositories};

#[derive(derive_more::Display)]
#[display("{token}:{action}")]
pub(crate) struct WizardCallbackData {
    token: String,
    action: String,
}

impl CallbackDataWithPrefix for WizardCallbackData {
    fn prefix() -> &'static str {
        "wizard"
    }
}

impl TryFrom<String> for WizardCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let (token, action) = data.split_once(':').ok_or_else(|| err.missing_part("action"))?;
        Ok(Self { token: token.to_owned(), action: action.to_owned() })
    }
}

fn btn(label: impl Into<String>, token: &str, action: impl Into<String>) -> InlineKeyboardButton {
    let data = WizardCallbackData { token: token.to_owned(), action: action.into() }.to_data_string();
    InlineKeyboardButton::callback(label.into(), data)
}

/// The entry point from the listone (see `InlineCommand::Wizard`): a fresh session plus its
/// first screen.
pub(crate) fn start(store: &WizardStore, owner: UserId, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let token = store.create(owner);
    render_mode(&token, lang_code)
}

fn cancel_row(token: &str, lang_code: &LanguageCode) -> Vec<InlineKeyboardButton> {
    vec![btn(t!("wizard.cancel_button", locale = lang_code).to_string(), token, "no")]
}

/// Every screen but the very first (`render_mode`, which has nothing to go back to) gets both
/// "🔙 Indree" and "❌ Annulla" on the same row.
fn nav_row(token: &str, lang_code: &LanguageCode) -> Vec<InlineKeyboardButton> {
    vec![
        btn(t!("wizard.back_button", locale = lang_code).to_string(), token, "bk"),
        btn(t!("wizard.cancel_button", locale = lang_code).to_string(), token, "no"),
    ]
}

fn render_mode(token: &str, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let text = t!("wizard.mode.text", locale = lang_code).to_string();
    let rows = vec![
        vec![btn(t!("wizard.mode.single", locale = lang_code).to_string(), token, "mode:s")],
        vec![btn(t!("wizard.mode.combo", locale = lang_code).to_string(), token, "mode:c")],
        cancel_row(token, lang_code),
    ];
    (text, InlineKeyboardMarkup::new(rows))
}

fn leg_suffix(leg_n: u8, mode: WizardMode, lang_code: &LanguageCode) -> String {
    if mode == WizardMode::Combo {
        t!("wizard.leg_label", locale = lang_code, n = leg_n).to_string()
    } else {
        String::default()
    }
}

fn render_kind(token: &str, leg_n: u8, mode: WizardMode, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let text = format!("{}{}", t!("wizard.kind.text", locale = lang_code), leg_suffix(leg_n, mode, lang_code));
    let rows = vec![
        vec![btn(t!("wizard.kind.pvp", locale = lang_code).to_string(), token, format!("kind:{leg_n}:p"))],
        vec![btn(t!("wizard.kind.donate", locale = lang_code).to_string(), token, format!("kind:{leg_n}:d"))],
        vec![btn(t!("wizard.kind.presta", locale = lang_code).to_string(), token, format!("kind:{leg_n}:l"))],
        nav_row(token, lang_code),
    ];
    (text, InlineKeyboardMarkup::new(rows))
}

/// Shared by the amount and rate/probability keypads - three rows of 1-9, then ⌫/0/✅, under
/// whatever `top_rows` the caller wants (the give/take or default/custom toggles).
fn keypad_rows(token: &str, field: &str, leg_n: u8, top_rows: Vec<Vec<InlineKeyboardButton>>) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows = top_rows;
    for chunk in [["1", "2", "3"], ["4", "5", "6"], ["7", "8", "9"]] {
        rows.push(chunk.iter().map(|d| btn(d.to_string(), token, format!("{field}:{leg_n}:{d}"))).collect());
    }
    rows.push(vec![
        btn("⌫", token, format!("{field}:{leg_n}:b")),
        btn("0", token, format!("{field}:{leg_n}:0")),
        btn("✅", token, format!("{field}:{leg_n}:k")),
    ]);
    rows
}

fn render_amount(token: &str, leg_n: u8, leg: &LegState, mode: WizardMode, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let buf_display = if leg.amount_buf.is_empty() { "0" } else { &leg.amount_buf };
    let mut top_rows = Vec::new();
    let sign_text = if matches!(leg.kind, Some(OfferKind::Donate) | Some(OfferKind::Presta)) {
        let (give_key, take_key) = match leg.kind {
            Some(OfferKind::Donate) => ("wizard.sign.donate_give", "wizard.sign.donate_take"),
            _ => ("wizard.sign.presta_give", "wizard.sign.presta_take"),
        };
        let give_label = mark_selected(t!(give_key, locale = lang_code).to_string(), !leg.is_pull);
        let take_label = mark_selected(t!(take_key, locale = lang_code).to_string(), leg.is_pull);
        top_rows.push(vec![
            btn(give_label, token, format!("sign:{leg_n}:g")),
            btn(take_label, token, format!("sign:{leg_n}:t")),
        ]);
        t!(if leg.is_pull { take_key } else { give_key }, locale = lang_code).to_string()
    } else {
        String::default()
    };
    let text = format!("{}{}\n\n{}", t!("wizard.amount.text", locale = lang_code), leg_suffix(leg_n, mode, lang_code),
        t!("wizard.amount.current", locale = lang_code, sign = sign_text, amount = buf_display));
    let mut rows = keypad_rows(token, "amt", leg_n, top_rows);
    rows.push(nav_row(token, lang_code));
    (text, InlineKeyboardMarkup::new(rows))
}

fn mark_selected(label: String, selected: bool) -> String {
    if selected { format!("✓ {label}") } else { label }
}

fn render_rate_or_prob(token: &str, leg_n: u8, leg: &LegState, mode: WizardMode, config: &AppConfig, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let buf_display = if leg.rate_buf.is_empty() { "0" } else { &leg.rate_buf };
    match leg.kind {
        Some(OfferKind::Pvp) => {
            let text = format!("{}{}\n\n{}", t!("wizard.prob.text", locale = lang_code), leg_suffix(leg_n, mode, lang_code),
                t!("wizard.prob.current", locale = lang_code, amount = buf_display));
            let top = vec![vec![btn(t!("wizard.prob.standard", locale = lang_code).to_string(), token, format!("rdef:{leg_n}"))]];
            let mut rows = keypad_rows(token, "rate", leg_n, top);
            rows.push(nav_row(token, lang_code));
            (text, InlineKeyboardMarkup::new(rows))
        },
        _ => {
            let default_pct = p2p_loan::rate_to_pct(config.p2p_loan_interest_rate);
            let sign_label = mark_selected(t!("wizard.rate.positive", locale = lang_code).to_string(), !leg.rate_is_negative);
            let neg_label = mark_selected(t!("wizard.rate.negative", locale = lang_code).to_string(), leg.rate_is_negative);
            let text = format!("{}{}\n\n{}", t!("wizard.rate.text", locale = lang_code), leg_suffix(leg_n, mode, lang_code),
                t!("wizard.rate.current", locale = lang_code, amount = buf_display));
            let top = vec![
                vec![btn(t!("wizard.rate.default", locale = lang_code, rate = default_pct).to_string(), token, format!("rdef:{leg_n}"))],
                vec![
                    btn(sign_label, token, format!("rsign:{leg_n}:p")),
                    btn(neg_label, token, format!("rsign:{leg_n}:n")),
                ],
            ];
            let mut rows = keypad_rows(token, "rate", leg_n, top);
            rows.push(nav_row(token, lang_code));
            (text, InlineKeyboardMarkup::new(rows))
        }
    }
}

const TARGET_PAGE_SIZE: usize = 10;

fn render_target(token: &str, state: &WizardState, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let text = t!("wizard.target.text", locale = lang_code).to_string();
    let mut rows = vec![vec![btn(t!("wizard.target.open", locale = lang_code).to_string(), token, "tgt:o")]];

    let candidates = state.target_candidates.as_deref().unwrap_or(&[]);
    let page = state.target_page as usize;
    let start = page * TARGET_PAGE_SIZE;
    let page_items = candidates.iter().enumerate().skip(start).take(TARGET_PAGE_SIZE);
    let mut chip_row = Vec::new();
    for (idx, (_, name)) in page_items {
        chip_row.push(btn(name.clone(), token, format!("tgt:s:{idx}")));
        if chip_row.len() == 2 {
            rows.push(std::mem::take(&mut chip_row));
        }
    }
    if !chip_row.is_empty() {
        rows.push(chip_row);
    }

    let mut page_nav_row = Vec::new();
    if page > 0 {
        page_nav_row.push(btn("⬅️", token, format!("tgt:p:{}", page - 1)));
    }
    if start + TARGET_PAGE_SIZE < candidates.len() {
        page_nav_row.push(btn("➡️", token, format!("tgt:p:{}", page + 1)));
    }
    if !page_nav_row.is_empty() {
        rows.push(page_nav_row);
    }
    rows.push(nav_row(token, lang_code));
    (text, InlineKeyboardMarkup::new(rows))
}

fn leg_preview_text(leg: &LegState, name: &Username, target_name: Option<&Username>, config: &AppConfig, lang_code: &LanguageCode) -> Option<String> {
    let amount = leg.amount? as i32;
    let signed_amount = if leg.is_pull { -amount } else { amount };
    match leg.kind? {
        OfferKind::Pvp => {
            let probability_pct = leg.rate_or_prob.flatten();
            Some(pvp::battle_offer_text(name, target_name, leg.amount?, probability_pct, lang_code))
        },
        OfferKind::Donate => Some(donate::donate_offer_text(name, target_name, signed_amount, lang_code)),
        OfferKind::Presta => {
            let rate_pct = leg.rate_or_prob.flatten().unwrap_or_else(|| p2p_loan::rate_to_pct(config.p2p_loan_interest_rate));
            let (abs_amount, _) = donate::split_amount(signed_amount);
            let interest = repo::compute_interest(abs_amount, (rate_pct / 100.0) as f32)?;
            Some(p2p_loan::p2p_loan_offer_text(name, target_name, signed_amount, rate_pct, interest, lang_code))
        }
    }
}

fn render_preview(token: &str, state: &WizardState, name: &Username, config: &AppConfig, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let target_name = state.target.flatten()
        .and_then(|uid| state.target_candidates.as_ref()?.iter().find(|(u, _)| *u == uid))
        .map(|(_, n)| Username::new(n.clone()));

    let leg1_text = leg_preview_text(&state.leg1, name, target_name.as_ref(), config, lang_code)
        .unwrap_or_else(|| t!("wizard.preview.invalid_leg", locale = lang_code).to_string());
    let text = if state.mode == Some(WizardMode::Combo) {
        let leg2_text = leg_preview_text(&state.leg2, name, target_name.as_ref(), config, lang_code)
            .unwrap_or_else(|| t!("wizard.preview.invalid_leg", locale = lang_code).to_string());
        format!("{}\n\n{leg1_text}\n\n{leg2_text}", t!("wizard.preview.intro", locale = lang_code))
    } else {
        format!("{}\n\n{leg1_text}", t!("wizard.preview.intro", locale = lang_code))
    };

    let rows = vec![
        vec![btn(t!("wizard.preview.confirm", locale = lang_code).to_string(), token, "go")],
        nav_row(token, lang_code),
    ];
    (text, InlineKeyboardMarkup::new(rows))
}

/// Looks at what's still missing in `state` and renders whichever screen comes next - the single
/// source of truth for "where are we in the flow", so the callback handler never has to track a
/// separate step counter.
fn render(token: &str, state: &WizardState, name: &Username, config: &AppConfig, lang_code: &LanguageCode) -> (String, InlineKeyboardMarkup) {
    let Some(mode) = state.mode else {
        return render_mode(token, lang_code);
    };
    if let Some(screen) = render_leg_if_incomplete(token, 1, &state.leg1, mode, config, lang_code) {
        return screen;
    }
    if mode == WizardMode::Combo {
        if let Some(screen) = render_leg_if_incomplete(token, 2, &state.leg2, mode, config, lang_code) {
            return screen;
        }
    }
    if state.target.is_none() {
        return render_target(token, state, lang_code);
    }
    render_preview(token, state, name, config, lang_code)
}

fn render_leg_if_incomplete(token: &str, leg_n: u8, leg: &LegState, mode: WizardMode, config: &AppConfig, lang_code: &LanguageCode) -> Option<(String, InlineKeyboardMarkup)> {
    if leg.kind.is_none() {
        return Some(render_kind(token, leg_n, mode, lang_code));
    }
    if leg.amount.is_none() {
        return Some(render_amount(token, leg_n, leg, mode, lang_code));
    }
    if leg.needs_rate_screen() && leg.rate_or_prob.is_none() {
        return Some(render_rate_or_prob(token, leg_n, leg, mode, config, lang_code));
    }
    None
}

/// Unsets whichever field `LegState`'s own most-recently-resolved screen filled in, mirroring
/// `render_leg_if_incomplete`'s own precedence in reverse. `None` means the leg was already at
/// its very first screen (kind not chosen yet) - there's nothing left within it to undo.
fn unwind_leg(leg: &mut LegState) -> Option<()> {
    if leg.needs_rate_screen() && leg.rate_or_prob.is_some() {
        leg.rate_or_prob = None;
    } else if leg.amount.is_some() {
        leg.amount = None;
    } else if leg.kind.is_some() {
        leg.kind = None;
    } else {
        return None;
    }
    Some(())
}

/// The "🔙 Indree" action: figures out which screen `render` would currently show (the same way
/// `render` does - by inspecting what's still unset) and undoes exactly the one field that got
/// us there, so the *previous* call to `render` shows the screen before this one. A no-op on the
/// very first screen (mode not chosen yet), which has nothing to go back to.
fn go_back(state: &mut WizardState) {
    let Some(mode) = state.mode else { return };

    if !state.leg1.is_complete() {
        if unwind_leg(&mut state.leg1).is_none() {
            state.mode = None;
        }
        return;
    }
    if mode == WizardMode::Combo && !state.leg2.is_complete() {
        if unwind_leg(&mut state.leg2).is_none() {
            // leg2's kind was never chosen - back up into leg1 instead, which (being complete)
            // always has something to undo.
            unwind_leg(&mut state.leg1);
        }
        return;
    }
    if state.target.is_none() {
        let last_leg = if mode == WizardMode::Combo { &mut state.leg2 } else { &mut state.leg1 };
        unwind_leg(last_leg);
        return;
    }
    state.target = None;
}

/// Mutates `state` according to `action`, validating the two cases that can actually fail
/// (a presta rate the configured cap rejects, or a pvp probability outside (0, 100)) - on
/// failure the relevant buffer/field is left untouched so the same screen is shown again.
/// Returns the locale key of an alert to show, if any.
fn apply_action(state: &mut WizardState, action: &str, config: &AppConfig) -> Option<&'static str> {
    let mut parts = action.split(':');
    match parts.next() {
        Some("bk") => go_back(state),
        Some("mode") => {
            state.mode = match parts.next() {
                Some("s") => Some(WizardMode::Single),
                Some("c") => Some(WizardMode::Combo),
                _ => state.mode,
            };
        },
        Some("kind") => {
            let leg_n = parse_leg(parts.next());
            let kind = match parts.next() {
                Some("p") => Some(OfferKind::Pvp),
                Some("d") => Some(OfferKind::Donate),
                Some("l") => Some(OfferKind::Presta),
                _ => None,
            };
            if let (Some(leg_n), Some(kind)) = (leg_n, kind) {
                state.leg_mut(leg_n).kind = Some(kind);
            }
        },
        Some("sign") => {
            if let Some(leg_n) = parse_leg(parts.next()) {
                state.leg_mut(leg_n).is_pull = matches!(parts.next(), Some("t"));
            }
        },
        Some("amt") => {
            if let Some(leg_n) = parse_leg(parts.next()) {
                let leg = state.leg_mut(leg_n);
                match parts.next() {
                    Some("b") => { leg.amount_buf.pop(); },
                    Some("k") => {
                        match leg.amount_buf.parse::<u16>() {
                            Ok(v) if v > 0 => leg.amount = Some(v),
                            _ => return Some("wizard.errors.amount_must_be_positive"),
                        }
                    },
                    Some(d) if is_digit(d) && leg.amount_buf.len() < 5 => leg.amount_buf.push_str(d),
                    _ => {}
                }
            }
        },
        Some("rsign") => {
            if let Some(leg_n) = parse_leg(parts.next()) {
                state.leg_mut(leg_n).rate_is_negative = matches!(parts.next(), Some("n"));
            }
        },
        Some("rdef") => {
            if let Some(leg_n) = parse_leg(parts.next()) {
                state.leg_mut(leg_n).rate_or_prob = Some(None);
            }
        },
        Some("rate") => {
            if let Some(leg_n) = parse_leg(parts.next()) {
                let is_pvp = state.leg(leg_n).kind == Some(OfferKind::Pvp);
                let leg = state.leg_mut(leg_n);
                match parts.next() {
                    Some("b") => { leg.rate_buf.pop(); },
                    Some("k") => {
                        let Ok(v) = leg.rate_buf.parse::<f64>() else { return Some("wizard.errors.invalid_amount") };
                        if is_pvp {
                            if !(0.0 < v && v < 100.0) {
                                return Some("wizard.errors.invalid_probability");
                            }
                            leg.rate_or_prob = Some(Some(v));
                        } else {
                            let signed = if leg.rate_is_negative { -v } else { v };
                            let abs_amount = leg.amount.unwrap_or(0);
                            if repo::compute_interest(abs_amount, (signed / 100.0) as f32).is_none() {
                                return Some("wizard.errors.rate_too_high");
                            }
                            leg.rate_or_prob = Some(Some(signed));
                        }
                    },
                    Some(d) if is_digit(d) && leg.rate_buf.len() < 3 => leg.rate_buf.push_str(d),
                    _ => {}
                }
            }
        },
        Some("tgt") => {
            match parts.next() {
                Some("o") => state.target = Some(None),
                Some("p") => if let Some(p) = parts.next().and_then(|s| s.parse().ok()) {
                    state.target_page = p;
                },
                Some("s") => if let Some(idx) = parts.next().and_then(|s| s.parse::<usize>().ok()) {
                    if let Some(candidates) = &state.target_candidates {
                        if let Some((uid, _)) = candidates.get(idx) {
                            state.target = Some(Some(*uid));
                        }
                    }
                },
                _ => {}
            }
        },
        _ => {}
    }
    let _ = config; // reserved for future validation needing config (kept for signature stability)
    None
}

fn parse_leg(s: Option<&str>) -> Option<u8> {
    s.and_then(|s| s.parse().ok())
}

fn is_digit(s: &str) -> bool {
    s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    WizardCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, store: WizardStore, config: AppConfig,
                              repos: Repositories) -> HandlerResult {
    let data = WizardCallbackData::parse(&query)?;
    let lang_code = LanguageCode::from_user(&query.from);
    let name = utils::get_full_name(&query.from);

    let owner = query.from.id;

    if data.action == "no" {
        // `with_state` is the only source of truth for who owns this token (it isn't encoded in
        // the callback_data itself, unlike e.g. `LoanCallbackData`'s `uid` field) - every branch
        // below must go through it before acting, exactly like this one, or anyone else in the
        // same chat could poke another player's in-progress wizard via its buttons.
        let owned = store.with_state(&data.token, owner, |_| ()).is_some();
        if !owned {
            return send_error_callback_answer(&bot, &query, "inline.callback.errors.another_user").await;
        }
        store.remove(&data.token);
        let text = t!("wizard.cancelled", locale = &lang_code).to_string();
        edit_message(&bot, &query, &text, None).await?;
        bot.answer_callback_query(&query.id).await?;
        return Ok(());
    }

    if data.action == "go" {
        let result = store.with_state(&data.token, owner, |state| {
            (state.mode, state.target, state.leg1.clone(), state.leg2.clone(), state.target_candidates.clone())
        });
        let Some((mode, target, leg1, leg2, candidates)) = result else {
            return send_error_callback_answer(&bot, &query, "inline.callback.errors.another_user").await;
        };
        let target_uid = target.flatten();
        let target_name = target_uid
            .and_then(|uid| candidates.as_ref()?.iter().find(|(u, _)| *u == uid))
            .map(|(_, n)| Username::new(n.clone()));
        // the token is only removed once `finalize` actually succeeds (e.g. the combo offer's
        // insert went through) - on failure it's left in place, so tapping "✅ Creala!" again
        // retries from the exact same, already-filled-in state instead of losing it.
        match finalize(mode, target_uid, target_name.as_ref(), leg1, leg2, &repos, owner, &name, &config, &lang_code).await {
            Ok((text, keyboard)) => {
                store.remove(&data.token);
                edit_message(&bot, &query, &text, keyboard).await?;
                bot.answer_callback_query(&query.id).await?;
            }
            Err(e) => {
                log::error!("couldn't finalize a wizard offer (token={}, owner={owner}): {e}", data.token);
                send_error_callback_answer(&bot, &query, "wizard.errors.finalize_failed").await?;
            }
        }
        return Ok(());
    }

    let outcome = store.with_state(&data.token, owner, |state| apply_action(state, &data.action, &config));
    let Some(error_key) = outcome else {
        return send_error_callback_answer(&bot, &query, "inline.callback.errors.another_user").await;
    };
    if let Some(error_key) = error_key {
        return send_error_callback_answer(&bot, &query, error_key).await;
    }

    let needs_fetch = store.with_state(&data.token, owner, |state| {
        state.mode.is_some()
            && state.leg1.is_complete()
            && (state.mode != Some(WizardMode::Combo) || state.leg2.is_complete())
            && state.target.is_none()
            && state.target_candidates.is_none()
    }).unwrap_or(false);
    if needs_fetch {
        let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);
        let members = repos.users.get_chat_members(&chat_id.kind()).await?;
        let candidates = members.into_iter()
            .filter(|u| UserId(u.uid as u64) != owner)
            .map(|u| (UserId(u.uid as u64), u.name.to_string()))
            .collect::<Vec<_>>();
        store.with_state(&data.token, owner, |state| { state.target_candidates = Some(candidates); });
    }

    let rendered = store.with_state(&data.token, owner, |state| render(&data.token, state, &name, &config, &lang_code));
    if let Some((text, keyboard)) = rendered {
        edit_message(&bot, &query, &text, Some(keyboard)).await?;
    }
    bot.answer_callback_query(&query.id).await?;
    Ok(())
}

async fn edit_message(bot: &Bot, query: &CallbackQuery, text: &str, keyboard: Option<InlineKeyboardMarkup>) -> HandlerResult {
    match callbacks::get_params_for_message_edit(query)? {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut req = bot.edit_message_text(chat_id, message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup = keyboard;
            req.await?;
        },
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut req = bot.edit_message_text_inline(inline_message_id, text);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup = keyboard;
            req.await?;
        },
    }
    Ok(())
}

async fn send_error_callback_answer(bot: &Bot, query: &CallbackQuery, t_key: &str) -> HandlerResult {
    let lang_code = LanguageCode::from_user(&query.from);
    let mut answer = bot.answer_callback_query(&query.id);
    answer.show_alert.replace(true);
    answer.text.replace(t!(t_key, locale = &lang_code).to_string());
    answer.await?;
    Ok(())
}

fn build_single_offer(leg: &LegState, target_uid: Option<UserId>, target_name: Option<&Username>, proposer: UserId,
                      name: &Username, config: &AppConfig, lang_code: &LanguageCode) -> (String, Option<InlineKeyboardMarkup>) {
    let amount = leg.amount.unwrap_or(0);
    let signed_amount = if leg.is_pull { -(amount as i32) } else { amount as i32 };
    match leg.kind {
        Some(OfferKind::Pvp) => {
            let probability_pct = leg.rate_or_prob.flatten();
            let text = pvp::battle_offer_text(name, target_name, amount, probability_pct, lang_code);
            let btn_label = t!("commands.pvp.button", locale = lang_code).to_string();
            let data = pvp::BattleCallbackData::new(proposer, amount, target_uid, probability_pct).to_data_string();
            let accept_btn = InlineKeyboardButton::callback(btn_label, data);
            (text, Some(offer_keyboard(accept_btn, proposer, target_uid, lang_code)))
        },
        Some(OfferKind::Donate) => {
            let text = donate::donate_offer_text(name, target_name, signed_amount, lang_code);
            let btn_label_key = if leg.is_pull { "commands.donate.button_pull" } else { "commands.donate.button" };
            let btn_label = t!(btn_label_key, locale = lang_code).to_string();
            let data = donate::DonateCallbackData::new(proposer, signed_amount, target_uid).to_data_string();
            let accept_btn = InlineKeyboardButton::callback(btn_label, data);
            (text, Some(offer_keyboard(accept_btn, proposer, target_uid, lang_code)))
        },
        Some(OfferKind::Presta) => {
            let rate_pct = leg.rate_or_prob.flatten().unwrap_or_else(|| p2p_loan::rate_to_pct(config.p2p_loan_interest_rate));
            let custom_rate = leg.rate_or_prob.flatten();
            match repo::compute_interest(amount, (rate_pct / 100.0) as f32) {
                None => (t!("commands.presta.errors.rate_too_high", locale = lang_code, rate = rate_pct, amount = amount).to_string(), None),
                Some(interest) => {
                    let text = p2p_loan::p2p_loan_offer_text(name, target_name, signed_amount, rate_pct, interest, lang_code);
                    let btn_label_key = if leg.is_pull { "commands.presta.button_pull" } else { "commands.presta.button" };
                    let btn_label = t!(btn_label_key, locale = lang_code).to_string();
                    let data = p2p_loan::P2PLoanCallbackData::new(proposer, signed_amount, target_uid, custom_rate).to_data_string();
                    let accept_btn = InlineKeyboardButton::callback(btn_label, data);
                    (text, Some(offer_keyboard(accept_btn, proposer, target_uid, lang_code)))
                }
            }
        },
        None => (t!("wizard.errors.invalid_amount", locale = lang_code).to_string(), None),
    }
}

fn leg_to_combo_leg(leg: &LegState) -> Option<ComboLeg> {
    let amount = leg.amount? as i32;
    let signed_amount = if leg.is_pull { -amount } else { amount };
    Some(match leg.kind? {
        OfferKind::Pvp => ComboLeg::Pvp { bet: leg.amount?, probability_pct: leg.rate_or_prob.flatten() },
        OfferKind::Donate => ComboLeg::Donate { amount: signed_amount },
        OfferKind::Presta => ComboLeg::P2PLoan { amount: signed_amount, interest_rate_pct: leg.rate_or_prob.flatten() },
    })
}

async fn finalize(mode: Option<WizardMode>, target_uid: Option<UserId>, target_name: Option<&Username>, leg1: LegState, leg2: LegState,
                  repos: &Repositories, proposer: UserId, name: &Username, config: &AppConfig, lang_code: &LanguageCode) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    Ok(match mode {
        Some(WizardMode::Single) => build_single_offer(&leg1, target_uid, target_name, proposer, name, config, lang_code),
        Some(WizardMode::Combo) => {
            let (Some(combo_leg1), Some(combo_leg2)) = (leg_to_combo_leg(&leg1), leg_to_combo_leg(&leg2)) else {
                return Ok((t!("wizard.errors.invalid_amount", locale = lang_code).to_string(), None));
            };
            let leg1_text = leg_preview_text(&leg1, name, target_name, config, lang_code).unwrap_or_default();
            let leg2_text = leg_preview_text(&leg2, name, target_name, config, lang_code).unwrap_or_default();
            let text = format!("{leg1_text}\n\n{leg2_text}");
            let offer = ComboOffer::new(proposer, target_uid, combo_leg1, combo_leg2);
            let token = repos.combo_offers.insert(&offer).await?;
            (text, Some(combo::combo_offer_keyboard(&token, target_uid, lang_code)))
        },
        None => (t!("wizard.errors.invalid_amount", locale = lang_code).to_string(), None),
    })
}
