use anyhow::{anyhow, Context};
use rust_i18n::t;
use teloxide::Bot;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, ChosenInlineResult, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery, InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputMessageContentText, ParseMode, UserId};
use crate::handlers::{details, CallbackResult, HandlerResult, send_error_callback_answer, utils};
use crate::handlers::donate::{self, split_amount};
use crate::handlers::p2p_loan::{self, rate_to_pct};
use crate::handlers::pvp::{self, build_inline_target_error_result, UserInfo};
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackData, InvalidCallbackDataBuilder};
use crate::handlers::utils::details_store::DetailsStore;
use crate::handlers::utils::inline_target::{parse_combo_inline_query, ComboLegQuery};
use crate::config::AppConfig;
use crate::domain::{LanguageCode, Username};
use crate::metrics;
use sqlx::{Postgres, Transaction};
use crate::repo::{compute_interest, AcceptOutcome, ChatIdKind, ChatIdPartiality, ComboLeg, ComboOffer, Repositories};

#[derive(Clone, Copy)]
pub(crate) enum ComboAction {
    Accept,
    Cancel,
    Reject,
}

impl std::fmt::Display for ComboAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ComboAction::Accept => "accept",
            ComboAction::Cancel => "cancel",
            ComboAction::Reject => "reject",
        };
        write!(f, "{s}")
    }
}

/// Carries only an action tag and a token - the actual offer (both legs, proposer, target)
/// lives server-side in `repo::ComboOffers`, since Telegram's 64-byte `callback_data` limit leaves
/// no room to encode two full offers in one button (see that store's doc comment).
#[derive(derive_more::Display)]
#[display("{action}:{token}")]
pub(crate) struct ComboCallbackData {
    action: ComboAction,
    token: String,
}

impl ComboCallbackData {
    pub(super) fn new(action: ComboAction, token: &str) -> Self {
        Self { action, token: token.to_owned() }
    }
}

impl CallbackDataWithPrefix for ComboCallbackData {
    fn prefix() -> &'static str {
        "combo"
    }
}

impl TryFrom<String> for ComboCallbackData {
    type Error = InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let (action_str, token) = data.split_once(':').ok_or_else(|| err.missing_part("token"))?;
        let action = match action_str {
            "accept" => ComboAction::Accept,
            "cancel" => ComboAction::Cancel,
            "reject" => ComboAction::Reject,
            _ => return Err(err.missing_part("action")),
        };
        Ok(Self { action, token: token.to_owned() })
    }
}

/// Combo's own keyboard, separate from `crate::handlers::offer_keyboard`: its buttons carry a
/// token (looked up in `repo::ComboOffers`) rather than the offer's own parameters.
pub(super) fn combo_offer_keyboard(token: &str, target: Option<UserId>, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let accept_label = t!("commands.combo.button", locale = lang_code);
    let mut top_row = vec![InlineKeyboardButton::callback(accept_label, ComboCallbackData::new(ComboAction::Accept, token).to_data_string())];
    if target.is_some() {
        let reject_label = t!("inline.callback.reject_button", locale = lang_code);
        top_row.push(InlineKeyboardButton::callback(reject_label, ComboCallbackData::new(ComboAction::Reject, token).to_data_string()));
    }
    let cancel_label = t!("inline.callback.cancel_button", locale = lang_code);
    let cancel_btn = InlineKeyboardButton::callback(cancel_label, ComboCallbackData::new(ComboAction::Cancel, token).to_data_string());
    InlineKeyboardMarkup::new(vec![top_row, vec![cancel_btn]])
}

/// Combo has no `BotCommands` enum of its own to derive a toggle key from (it's recognized by
/// free-text syntax anywhere in an inline query, not a `/command` - see `parse_combo_inline_query`),
/// so it's checked directly against `CachedEnvToggles` under this literal key (`DISABLE_CMD_COMBO`),
/// for parity with every other feature's `DISABLE_CMD_*` toggle.
pub const TOGGLE_KEY: &str = "combo";

pub fn inline_filter(query: InlineQuery, config: AppConfig) -> bool {
    config.command_toggles.enabled(TOGGLE_KEY) && parse_combo_inline_query(&query.query).is_some()
}

pub fn chosen_inline_result_filter(result: ChosenInlineResult) -> bool {
    parse_combo_inline_query(&result.query).is_some()
}

pub async fn inline_chosen_handler() -> HandlerResult {
    metrics::INLINE_COUNTER.finished();
    Ok(())
}

struct LegPreview {
    text: String,
    leg: ComboLeg,
}

/// Validates and previews one leg, applying the *same* checks the matching single-offer inline
/// handler already does before building its own offer (pvp's probability-range filter, presta's
/// `compute_interest`/rate check) - `Err` carries the localized error text to show instead.
fn build_leg(query: &ComboLegQuery, proposer_name: &Username, target_name: Option<&Username>, config: &AppConfig, lang_code: &LanguageCode) -> Result<LegPreview, String> {
    match query {
        ComboLegQuery::Pvp(q) => {
            if let Some(pct) = q.probability_pct.filter(|pct| *pct <= 0.0 || *pct >= 100.0) {
                return Err(t!("commands.pvp.errors.invalid_probability", locale = lang_code, probability = pct).to_string());
            }
            let text = pvp::battle_offer_text(proposer_name, target_name, q.amount, q.probability_pct, lang_code);
            Ok(LegPreview { text, leg: ComboLeg::Pvp { bet: q.amount, probability_pct: q.probability_pct } })
        },
        ComboLegQuery::Donate { amount, .. } => {
            let text = donate::donate_offer_text(proposer_name, target_name, *amount, lang_code);
            Ok(LegPreview { text, leg: ComboLeg::Donate { amount: *amount } })
        },
        ComboLegQuery::P2PLoan(q) => {
            let (abs_amount, _) = split_amount(q.amount);
            let rate_pct = q.interest_rate_pct.unwrap_or(rate_to_pct(config.p2p_loan_interest_rate));
            let interest = compute_interest(abs_amount, (rate_pct / 100.0) as f32)
                .ok_or_else(|| t!("commands.presta.errors.rate_too_high", locale = lang_code, rate = rate_pct, amount = abs_amount).to_string())?;
            let text = p2p_loan::p2p_loan_offer_text(proposer_name, target_name, q.amount, rate_pct, interest, lang_code);
            Ok(LegPreview { text, leg: ComboLeg::P2PLoan { amount: q.amount, interest_rate_pct: q.interest_rate_pct } })
        },
    }
}

fn text_only_article(id: &str, text: String) -> InlineQueryResult {
    let content = InputMessageContent::Text(InputMessageContentText::new(&text));
    InlineQueryResultArticle::new(id, text, content).into()
}

async fn answer_with(bot: Bot, query: &InlineQuery, res: InlineQueryResult) -> HandlerResult {
    let mut answer = bot.answer_inline_query(&query.id, vec![res.clone()])
        .is_personal(true);
    if cfg!(debug_assertions) {
        answer.cache_time.replace(1);
    }
    answer.await.context(format!("couldn't answer a callback query {query:?} with {res:?}"))?;
    Ok(())
}

pub async fn inline_handler(bot: Bot, query: InlineQuery, repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::INLINE_COUNTER.invoked();

    let parsed = parse_combo_inline_query(&query.query)
        .ok_or_else(|| anyhow!("inline query '{}' couldn't be parsed by the combo handler", query.query))?;
    let lang_code = LanguageCode::from_user(&query.from);
    let name = utils::get_full_name(&query.from);

    // a combo's two legs can never target two different people, nor mix an open leg with a
    // targeted one - so a name is only legal here if it's either the sole one given, or both
    // legs agree on it.
    let target_name = match (parsed.leg1.target_name(), parsed.leg2.target_name()) {
        (Some(n1), Some(n2)) if !n1.eq_ignore_ascii_case(n2) => {
            let text = t!("commands.combo.errors.target_mismatch", locale = &lang_code).to_string();
            return answer_with(bot, &query, text_only_article("combo-target-mismatch", text)).await;
        },
        (Some(n1), _) => Some(n1),
        (None, n2) => n2,
    };

    let target = match target_name {
        None => None,
        Some(target_name) => {
            let mut matches = repos.users.find_by_exact_name(target_name).await?;
            match matches.len() {
                0 => return answer_with(bot, &query, build_inline_target_error_result("commands.pvp.errors.target_not_found", &lang_code, target_name)).await,
                1 => Some(matches.pop().expect("len checked above")),
                _ => return answer_with(bot, &query, build_inline_target_error_result("commands.pvp.errors.target_ambiguous", &lang_code, target_name)).await,
            }
        },
    };
    let target_uid = target.as_ref().map(|t| UserId(t.uid as u64));
    let target_username = target.as_ref().map(|t| &t.name);

    let leg1 = match build_leg(&parsed.leg1, &name, target_username, &config, &lang_code) {
        Ok(leg) => leg,
        Err(err_text) => return answer_with(bot, &query, text_only_article("combo-error", err_text)).await,
    };
    let leg2 = match build_leg(&parsed.leg2, &name, target_username, &config, &lang_code) {
        Ok(leg) => leg,
        Err(err_text) => return answer_with(bot, &query, text_only_article("combo-error", err_text)).await,
    };

    let text = format!("{}\n\n{}", leg1.text, leg2.text);
    let offer = ComboOffer::new(query.from.id, target_uid, leg1.leg, leg2.leg);
    let token = repos.combo_offers.insert(&offer).await?;
    let title = t!("inline.results.titles.combo", locale = &lang_code);
    let content = InputMessageContent::Text(InputMessageContentText::new(text).parse_mode(ParseMode::Html));
    let res = InlineQueryResultArticle::new("combo", title, content)
        .reply_markup(combo_offer_keyboard(&token, target_uid, &lang_code))
        .into();
    answer_with(bot, &query, res).await
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    ComboCallbackData::check_prefix(query)
}

/// Whichever leg type's `_core_in_tx` actually succeeded, carried forward to `finish_leg` -
/// mirrors `ComboLeg` itself, but holding each type's *result* instead of its offer parameters.
enum LegCore {
    Pvp(pvp::BattleCoreResult),
    Donate(donate::DonateCoreResult),
    P2PLoan(p2p_loan::P2PLoanCoreResult),
}

/// Runs one leg's core (the affordability check and, if it passes, the actual transfer) against
/// the shared `tx` - the same `tx` both legs use, so leg2's own check sees leg1's transfer even
/// though neither has committed yet (see `callback_handler`'s doc comment for why that matters).
/// `Ok(Err(result))` means this leg's own check failed - `result` is *exactly* the message the
/// matching single-offer accept path would have shown for that same failure (so leg1 and leg2
/// failures alike read identically to a standalone rejection, not a combo-specific one).
async fn try_leg_core(tx: &mut Transaction<'_, Postgres>, repos: &Repositories, chat_id: &ChatIdPartiality, chat_id_kind: &ChatIdKind, internal_chat_id: i64, config: &AppConfig, lang_code: &LanguageCode, proposer: UserId, acceptor: UserId, leg: &ComboLeg) -> anyhow::Result<Result<LegCore, CallbackResult>> {
    match leg {
        ComboLeg::Pvp { bet, probability_pct } => {
            let p = pvp::BattleParams { repos: repos.clone(), features: config.features.pvp, chat_id: chat_id.clone(), lang_code: lang_code.clone(), tax_bottom_ranks: config.tax.bottom_ranks };
            Ok(match pvp::pvp_core_in_tx(tx, &p, internal_chat_id, proposer, acceptor, *bet, *probability_pct).await? {
                pvp::BattleAffordability::BothEnough(core) => Ok(LegCore::Pvp(core)),
                pvp::BattleAffordability::InitiatorNotEnough =>
                    Err(CallbackResult::EditMessage(t!("commands.pvp.errors.not_enough.initiator", locale = lang_code).to_string(), None)),
                pvp::BattleAffordability::AcceptorNotEnough =>
                    Err(CallbackResult::ShowError(t!("commands.pvp.errors.not_enough.acceptor", locale = lang_code).to_string())),
            })
        },
        ComboLeg::Donate { amount } => {
            Ok(match donate::donate_core_in_tx(tx, chat_id_kind, internal_chat_id, proposer, acceptor, *amount).await? {
                Some(core) => Ok(LegCore::Donate(core)),
                None => Err(CallbackResult::ShowError(t!("commands.donate.errors.not_enough", locale = lang_code).to_string())),
            })
        },
        ComboLeg::P2PLoan { amount, interest_rate_pct } => {
            Ok(match p2p_loan::p2p_loan_core_in_tx(tx, repos, chat_id_kind, internal_chat_id, proposer, acceptor, *amount, *interest_rate_pct, config.p2p_loan_interest_rate).await? {
                p2p_loan::P2PLoanAffordability::Ok(core) => Ok(LegCore::P2PLoan(core)),
                p2p_loan::P2PLoanAffordability::NotEnough =>
                    Err(CallbackResult::ShowError(t!("commands.presta.errors.not_enough", locale = lang_code).to_string())),
                p2p_loan::P2PLoanAffordability::RateTooHigh { rate_pct, abs_amount } =>
                    Err(CallbackResult::ShowError(t!("commands.presta.errors.rate_too_high", locale = lang_code, rate = rate_pct, amount = abs_amount).to_string())),
            })
        },
    }
}

/// The post-commit half of a leg that already succeeded: ledger, stats/debt settlement, and the
/// result text - the same `*_finish` a standalone offer of that type uses. Returns `(short_text,
/// details)` exactly like those functions do, so `callback_handler` can combine both legs' pieces
/// into one headline and one shared, deferrable details blob instead of two duplicated ones.
async fn finish_leg(repos: Repositories, chat_id: &ChatIdPartiality, config: &AppConfig, lang_code: &LanguageCode, acceptor: &UserInfo, internal_chat_id: i64, core: LegCore) -> anyhow::Result<(String, Option<String>)> {
    match core {
        LegCore::Pvp(core) => {
            let p = pvp::BattleParams { repos, features: config.features.pvp, chat_id: chat_id.clone(), lang_code: lang_code.clone(), tax_bottom_ranks: config.tax.bottom_ranks };
            pvp::pvp_finish(&p, acceptor, internal_chat_id, core).await
        },
        LegCore::Donate(core) => {
            let p = donate::DonateParams { repos, chat_id: chat_id.clone(), lang_code: lang_code.clone(), tax_bottom_ranks: config.tax.bottom_ranks };
            donate::donate_finish(&p, acceptor, internal_chat_id, core).await
        },
        LegCore::P2PLoan(core) => {
            let p = p2p_loan::P2PLoanParams { repos, chat_id: chat_id.clone(), lang_code: lang_code.clone(), interest_rate: config.p2p_loan_interest_rate, tax_bottom_ranks: config.tax.bottom_ranks };
            p2p_loan::p2p_loan_finish(&p, acceptor, internal_chat_id, core).await
        },
    }
}

fn net_result_line(net_delta: i32, lang_code: &LanguageCode) -> String {
    match net_delta.cmp(&0) {
        std::cmp::Ordering::Greater => t!("commands.combo.results.net.gain", locale = lang_code, net = net_delta).to_string(),
        std::cmp::Ordering::Less => t!("commands.combo.results.net.loss", locale = lang_code, net = net_delta.abs()).to_string(),
        std::cmp::Ordering::Equal => t!("commands.combo.results.net.even", locale = lang_code).to_string(),
    }
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories, config: AppConfig, details_store: DetailsStore) -> HandlerResult {
    let callback_data = ComboCallbackData::parse(&query)?;
    let lang_code = LanguageCode::from_user(&query.from);

    match callback_data.action {
        ComboAction::Cancel => {
            return match repos.combo_offers.try_cancel(&callback_data.token, query.from.id).await? {
                Some(_) => {
                    let text = t!("inline.callback.offer_cancelled", locale = &lang_code).to_string();
                    CallbackResult::EditMessage(text, None).apply(bot, query).await.map_err(Into::into)
                },
                None => send_error_callback_answer(bot, query, "inline.callback.errors.another_user").await,
            };
        },
        ComboAction::Reject => {
            return match repos.combo_offers.try_reject(&callback_data.token, query.from.id).await? {
                Some(_) => {
                    let text = t!("inline.callback.offer_rejected", locale = &lang_code).to_string();
                    CallbackResult::EditMessage(text, None).apply(bot, query).await.map_err(Into::into)
                },
                None => send_error_callback_answer(bot, query, "inline.callback.errors.another_user").await,
            };
        },
        ComboAction::Accept => {},
    }

    let offer = match repos.combo_offers.try_accept(&callback_data.token, query.from.id).await? {
        AcceptOutcome::NotFound => return send_error_callback_answer(bot, query, "commands.combo.errors.expired").await,
        AcceptOutcome::SamePerson => return send_error_callback_answer(bot, query, "commands.combo.errors.same_person").await,
        AcceptOutcome::WrongTarget => return send_error_callback_answer(bot, query, "commands.combo.errors.not_target").await,
        AcceptOutcome::Accepted(offer) => offer,
    };

    let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);
    let chat_id_kind = chat_id.kind();
    let acceptor: UserInfo = query.from.clone().into();
    let internal_chat_id = repos.dicks.resolve_chat(&chat_id).await?;
    // measured once up front and once after both legs commit, so the final message can report
    // one net "what actually changed for you" figure rather than leaving the player to add up
    // two separate per-leg numbers themselves.
    let acceptor_length_before = repos.dicks.fetch_length(acceptor.uid, &chat_id_kind).await?;

    // both legs' core transfer share this one transaction: leg2 only ever runs if leg1 already
    // succeeded *within it* (so leg2's own check sees leg1's not-yet-committed effect - this is
    // what makes a leg2 that only becomes affordable *because of* leg1, e.g. "lend me 30 combo
    // borrow 30 back at a negative rate" netting to zero, work correctly), and the transaction
    // is only ever committed once *both* have succeeded. If leg2 fails, dropping `tx` without
    // committing rolls leg1 back too, automatically, via Postgres - a real "both or nothing",
    // not best-effort bookkeeping.
    let mut tx = repos.dicks.begin_tx().await?;

    let leg1_core = match try_leg_core(&mut tx, &repos, &chat_id, &chat_id_kind, internal_chat_id, &config, &lang_code, offer.proposer, acceptor.uid, &offer.leg1).await? {
        Ok(core) => core,
        Err(result) => return result.apply(bot, query).await.map_err(Into::into), // tx dropped here: nothing was ever applied
    };
    let leg2_core = match try_leg_core(&mut tx, &repos, &chat_id, &chat_id_kind, internal_chat_id, &config, &lang_code, offer.proposer, acceptor.uid, &offer.leg2).await? {
        Ok(core) => core,
        Err(result) => return result.apply(bot, query).await.map_err(Into::into), // tx dropped here: leg1's transfer is rolled back too
    };
    tx.commit().await?;

    let (short1, details1) = finish_leg(repos.clone(), &chat_id, &config, &lang_code, &acceptor, internal_chat_id, leg1_core).await?;
    let (short2, details2) = finish_leg(repos.clone(), &chat_id, &config, &lang_code, &acceptor, internal_chat_id, leg2_core).await?;

    let acceptor_length_after = repos.dicks.fetch_length(acceptor.uid, &chat_id_kind).await?;
    let net_line = net_result_line(acceptor_length_after - acceptor_length_before, &lang_code);
    let combined_short = format!("{short1}\n\n{short2}\n\n{net_line}");
    let combined_details = [details1, details2].into_iter().flatten().collect::<Vec<_>>();
    let combined_details = (!combined_details.is_empty()).then(|| combined_details.join("\n\n"));
    // both legs' post-trade balances and leaderboard positions are folded into this one combined
    // result - a combo names one acceptor whose own combined outcome this is, so (like `/grow`'s
    // own Dettagli button) only they need to expand it.
    let (text, keyboard) = details::maybe_deferred(combined_short, combined_details, Some(acceptor.uid), Some(&details_store), &lang_code);
    CallbackResult::EditMessage(text, keyboard).apply(bot, query).await?;

    metrics::CMD_COMBO_COUNTER.inc();
    Ok(())
}
