use anyhow::{anyhow, Context};
use futures::join;
use rand::Rng;
use rand::rngs::OsRng;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, ChosenInlineResult, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery, InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputMessageContentText, Message, ParseMode, ReplyMarkup, User, UserId};
use crate::handlers::{details, CallbackResult, HandlerResult, offer_keyboard, reply_html, send_error_callback_answer, utils};
use crate::handlers::debt_settlement::settle_gain_against_debts;
use crate::{metrics, reply_html, repo};
use crate::config::{AppConfig, BattlesFeatureToggles};
use crate::domain::{LanguageCode, Username};
use crate::handlers::utils::callbacks;
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackDataBuilder, NewLayoutValue};
use crate::handlers::utils::details_store::DetailsStore;
use crate::handlers::utils::locks::LockCallbackServiceFacade;
use sqlx::{Postgres, Transaction};
use crate::repo::{BattleStats, ChatIdPartiality, Dicks, Repositories, WinRateAware};

// let's calculate time offsets from 22.06.2024
const TIMESTAMP_MILLIS_SINCE_2024: i64 = 1719014400000;

#[derive(BotCommands, Clone, Copy)]
#[command(rename_rule = "lowercase")]
pub enum BattleCommands {
    #[command(description = "pvp")]
    Pvp(u16),
    Battle(u16),
    Attack(u16),
    Fight(u16),
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum BattleCommandsNoArgs {
    Pvp,
    Battle,
    Attack,
    Fight,
}

impl BattleCommands {
    fn bet(&self) -> u16 {
        match *self {
            Self::Battle(bet) => bet,
            Self::Pvp(bet) => bet,
            Self::Attack(bet) => bet,
            Self::Fight(bet) => bet,
        }
    }
}

#[derive(derive_more::Display)]
#[display("{initiator}:{bet}:{timestamp}:{target}:{probability}")]
pub(crate) struct BattleCallbackData {
    initiator: UserId,
    bet: u16,

    // used to prevent repeated clicks on the same button
    timestamp: NewLayoutValue<i64>,

    // set when the challenge was started as a reply to a specific player's message;
    // only that player may then accept it
    target: NewLayoutValue<UserId>,

    // an explicit win-probability (0-100, exclusive, fractional values allowed e.g. 0.0025) for
    // the initiator, overriding the default 50% (or skill-based) odds; the payout becomes
    // asymmetric accordingly (see `skewed_win_amount`)
    probability: NewLayoutValue<f64>,
}

impl BattleCallbackData {
    pub(super) fn new(initiator: UserId, bet: u16, target: Option<UserId>, probability: Option<f64>) -> Self {
        Self {
            initiator, bet,
            timestamp: new_short_timestamp(),
            target: target.into(),
            probability: probability.into(),
        }
    }
}

impl CallbackDataWithPrefix for BattleCallbackData {
    fn prefix() -> &'static str {
        "pvp"
    }
}

impl TryFrom<String> for BattleCallbackData {
    type Error = callbacks::InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let initiator = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let bet: u16 = callbacks::parse_part(&mut parts, &err, "bet")?;
        let timestamp = callbacks::parse_optional_part(&mut parts, &err)?;
        let target = callbacks::parse_optional_part::<_, u64>(&mut parts, &err)?.map(UserId);
        let probability = callbacks::parse_optional_part(&mut parts, &err)?;
        Ok(Self { initiator, bet, timestamp, target, probability })
    }
}

pub async fn cmd_handler(bot: Bot, msg: Message, cmd: BattleCommands,
                         repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::CMD_PVP_COUNTER.chat.inc();

    let user: UserInfo = msg.from.as_ref().ok_or(anyhow!("no FROM field in the PVP command handler"))?.into();
    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    let target: Option<UserInfo> = msg.reply_to_message().and_then(|m| m.from.clone()).map(UserInfo::from);
    if let Some(target) = &target {
        if target.uid == user.uid {
            reply_html!(bot, msg, t!("commands.pvp.errors.same_person", locale = &lang_code));
            return Ok(());
        }
    }

    let params = BattleParams {
        repos,
        features: config.features.pvp,
        chat_id: msg.chat.id.into(),
        lang_code,
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let (text, keyboard) = pvp_impl_start(params, user, cmd.bet(), target, None).await?;

    let mut answer = reply_html(bot, &msg, text);
    answer.reply_markup = keyboard.map(ReplyMarkup::InlineKeyboard);
    answer.await?;
    Ok(())
}

pub async fn cmd_handler_no_args(bot: Bot, msg: Message) -> HandlerResult {
    metrics::CMD_PVP_COUNTER.chat.inc();

    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    reply_html!(bot, msg, t!("commands.pvp.errors.no_args", locale = &lang_code));
    Ok(())
}

pub fn inline_filter(query: InlineQuery) -> bool {
    utils::inline_target::parse_pvp_inline_query(&query.query).is_some()
}

pub fn chosen_inline_result_filter(result: ChosenInlineResult) -> bool {
    utils::inline_target::parse_pvp_inline_query(&result.query).is_some()
}

pub async fn inline_handler(bot: Bot, query: InlineQuery, repos: Repositories) -> HandlerResult {
    metrics::INLINE_COUNTER.invoked();

    let parsed = utils::inline_target::parse_pvp_inline_query(&query.query)
        .ok_or_else(|| anyhow!("inline query '{}' couldn't be parsed by the pvp handler", query.query))?;
    let lang_code = LanguageCode::from_user(&query.from);
    let name = utils::get_full_name(&query.from);

    let res = if let Some(pct) = parsed.probability_pct.filter(|pct| *pct <= 0.0 || *pct >= 100.0) {
        let text = t!("commands.pvp.errors.invalid_probability", locale = &lang_code, probability = pct).to_string();
        let content = InputMessageContent::Text(InputMessageContentText::new(&text));
        InlineQueryResultArticle::new("pvp-invalid-probability", text, content).into()
    } else {
        match parsed.target_name {
            None => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, parsed.amount, None, parsed.probability_pct),
            Some(target_name) => match repos.users.find_by_exact_name(&target_name).await?.as_slice() {
                [] => build_inline_target_error_result("commands.pvp.errors.target_not_found", &lang_code, &target_name),
                [target] => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, parsed.amount, Some(target), parsed.probability_pct),
                _ => build_inline_target_error_result("commands.pvp.errors.target_ambiguous", &lang_code, &target_name),
            }
        }
    };

    let mut answer = bot.answer_inline_query(&query.id, vec![res.clone()])
        .is_personal(true);
    if cfg!(debug_assertions) {
        answer.cache_time.replace(1);
    }
    answer.await.context(format!("couldn't answer a callback query {query:?} with {res:?}"))?;
    Ok(())
}

/// The free-form description of a pvp offer (no button), shared by the slash-command path, the
/// inline-query path, and combo offers - so all three read identically for the same parameters.
pub(crate) fn battle_offer_text(name: &Username, target_name: Option<&Username>, bet: u16, probability_pct: Option<f64>, lang_code: &LanguageCode) -> String {
    let mut text = match target_name {
        Some(target_name) => t!("commands.pvp.results.start_targeted", locale = lang_code,
            name = name.escaped(), target_name = target_name.escaped(), bet = bet).to_string(),
        None => t!("commands.pvp.results.start", locale = lang_code, name = name.escaped(), bet = bet).to_string(),
    };
    if let Some(pct) = probability_pct {
        let (acceptor_probability, acceptor_win, acceptor_lose) = skewed_odds_for_acceptor(bet, pct);
        text.push_str(&format!("\n\n{}", t!("commands.pvp.results.skewed_odds", locale = lang_code,
            probability = acceptor_probability, win_amount = acceptor_win, lose_amount = acceptor_lose)));
    }
    text
}

pub(super) fn build_inline_keyboard_article_result(uid: UserId, lang_code: &LanguageCode, name: &Username, bet: u16,
                                                    target: Option<&repo::User>, probability_pct: Option<f64>) -> InlineQueryResult {
    log::debug!("Starting a PvP for {uid} (bet = {bet}, target = {target:?}, probability = {probability_pct:?})...");

    let title = t!("inline.results.titles.pvp", locale = lang_code, bet = bet);
    let text = battle_offer_text(name, target.map(|t| &t.name), bet, probability_pct, lang_code);
    let content = InputMessageContent::Text(InputMessageContentText::new(text).parse_mode(ParseMode::Html));
    let btn_label = t!("commands.pvp.button", locale = lang_code);
    let target_uid = target.map(|t| UserId(t.uid as u64));
    let btn_data = BattleCallbackData::new(uid, bet, target_uid, probability_pct).to_data_string();
    let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
    InlineQueryResultArticle::new("pvp", title, content)
        .reply_markup(offer_keyboard(accept_btn, uid, target_uid, lang_code))
        .into()
}

pub(crate) fn build_inline_target_error_result(t_key: &str, lang_code: &LanguageCode, name: &str) -> InlineQueryResult {
    let text = t!(t_key, locale = lang_code, name = name).to_string();
    let content = InputMessageContent::Text(InputMessageContentText::new(&text));
    InlineQueryResultArticle::new(format!("target-error:{name}"), text, content).into()
}

pub async fn inline_chosen_handler() -> HandlerResult {
    metrics::INLINE_COUNTER.finished();
    Ok(())
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    BattleCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories, config: AppConfig,
                              mut battle_locker: LockCallbackServiceFacade, details_store: DetailsStore) -> HandlerResult {
    let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);

    let callback_data = BattleCallbackData::parse(&query)?;
    if callback_data.initiator == query.from.id {
        return send_error_callback_answer(bot, query, "commands.pvp.errors.same_person").await;
    }
    if let NewLayoutValue::Some(target) = callback_data.target {
        if target != query.from.id {
            return send_error_callback_answer(bot, query, "commands.pvp.errors.not_target").await;
        }
    }
    let _battle_guard = match battle_locker.try_lock(&callback_data) {
        Some(lock) => lock,
        None => return send_error_callback_answer(bot, query, "commands.pvp.errors.battle_already_in_progress").await
    };

    let params = BattleParams {
        repos,
        features: config.features.pvp,
        lang_code: LanguageCode::from_user(&query.from),
        chat_id: chat_id.clone(),
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let probability_pct = match callback_data.probability {
        NewLayoutValue::Some(pct) => Some(pct),
        NewLayoutValue::None => None,
    };
    let attack_result = pvp_impl_attack(params, callback_data.initiator, query.from.clone().into(), callback_data.bet, probability_pct, &details_store).await?;
    attack_result.apply(bot, query).await?;

    metrics::CMD_PVP_COUNTER.inline.inc();
    Ok(())
}

pub(crate) struct BattleParams {
    pub(crate) repos: Repositories,
    pub(crate) features: BattlesFeatureToggles,
    pub(crate) chat_id: ChatIdPartiality,
    pub(crate) lang_code: LanguageCode,
    pub(crate) tax_bottom_ranks: usize,
}

#[derive(Clone)]
pub(crate) struct UserInfo {
    pub(crate) uid: UserId,
    pub(crate) name: Username,
}

impl From<&User> for UserInfo {
    fn from(value: &User) -> Self {
        Self {
            uid: value.id,
            name: utils::get_full_name(value)
        }
    }
}

impl From<User> for UserInfo {
    fn from(value: User) -> Self {
        (&value).into()
    }
}

impl From<repo::User> for UserInfo {
    fn from(value: repo::User) -> Self {
        Self {
            uid: UserId(value.uid as u64),
            name: value.name
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<UserId> for UserInfo {
    fn into(self) -> UserId {
        self.uid
    }
}

pub(crate) async fn pvp_impl_start(p: BattleParams, initiator: UserInfo, bet: u16, target: Option<UserInfo>, probability_pct: Option<f64>) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    // the initiator's potential loss always stays capped at the nominal bet, exactly like the
    // classic symmetric battle - only their potential WIN is scaled by the explicit probability
    let enough = p.repos.dicks.check_dick(&p.chat_id.kind(), initiator.uid, bet).await?;
    log::debug!("Starting a PvP for {} in the chat with id = {} (bet = {bet}, enough = {enough})...", initiator.uid, p.chat_id);

    let data = if enough {
        let text = battle_offer_text(&initiator.name, target.as_ref().map(|t| &t.name), bet, probability_pct, &p.lang_code);
        let btn_label = t!("commands.pvp.button", locale = &p.lang_code);
        let target_uid = target.map(|t| t.uid);
        let btn_data = BattleCallbackData::new(initiator.uid, bet, target_uid, probability_pct).to_data_string();
        let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
        let keyboard = offer_keyboard(accept_btn, initiator.uid, target_uid, &p.lang_code);
        (text, Some(keyboard))
    } else {
        (t!("commands.pvp.errors.not_enough.initiator", locale = &p.lang_code).to_string(), None)
    };
    Ok(data)
}

/// What `pvp_core_in_tx` actually did when both sides could afford it, carried forward to
/// `pvp_finish` so it doesn't have to redo the random draw (it can't anyway - the outcome was
/// already decided) or re-read the lengths it already has in hand.
pub(crate) struct BattleCoreResult {
    winner: UserId,
    loser: UserId,
    bet: u16,
    winner_new_length: i32,
    loser_new_length: i32,
    winner_probability_pct: f64,
}

/// Either side may turn out unable to afford it - and unlike the deterministic donate/p2p-loan
/// legs, which side that is can't be known before checking, so both outcomes need their own
/// variant (the messaging differs: "initiator not enough" edits the offer message itself,
/// "acceptor not enough" is a private alert to whoever just clicked).
pub(crate) enum BattleAffordability {
    BothEnough(BattleCoreResult),
    InitiatorNotEnough,
    AcceptorNotEnough,
}

/// The core of a battle - the affordability check, the random draw, and (if both sides can
/// currently afford it) the actual length transfer - against an externally owned `tx` that this
/// never commits, exactly like `Dicks::move_length_in_tx`. The random draw itself needs no
/// database access, so it's free to run between the check and the transfer without involving
/// `tx` at all; the skill-based-probability lookup (a read of *past* battles, unaffected by
/// anything this call or a sibling leg in the same transaction might do) likewise doesn't need
/// to be part of it.
pub(crate) async fn pvp_core_in_tx(tx: &mut Transaction<'_, Postgres>, p: &BattleParams, internal_chat_id: i64, initiator: UserId, acceptor: UserId, bet: u16, probability_pct: Option<f64>) -> anyhow::Result<BattleAffordability> {
    let chat_id_kind = p.chat_id.kind();
    // the initiator's potential loss always stays capped at the nominal bet (same as the classic
    // symmetric battle); only the amount they'd WIN is scaled by the explicit probability, and
    // that scaled amount is exactly what the acceptor stands to lose if the initiator wins -
    // hence it's the acceptor's side of the "enough length" check that becomes variable.
    let win_amount = probability_pct.map(|pct| skewed_win_amount(bet, pct)).unwrap_or(bet);
    // a transaction is a single connection, so these two checks - unlike the pool-backed
    // version every other accept path still uses - can't run concurrently; sequential is the
    // price of sharing one transaction with a sibling leg (see `crate::handlers::combo`).
    let enough_initiator = Dicks::check_dick_with(&mut **tx, &chat_id_kind, initiator, bet).await?;
    let enough_acceptor = Dicks::check_dick_with(&mut **tx, &chat_id_kind, acceptor, if p.features.check_acceptor_length { win_amount } else { 0 }).await?;

    log::debug!("Executing the battle: initiator = {initiator} (enough = {enough_initiator}), acceptor = {acceptor} (enough = {enough_acceptor}), bet = {bet}, probability = {probability_pct:?}...");

    if !(enough_initiator && enough_acceptor) {
        return Ok(if enough_acceptor { BattleAffordability::InitiatorNotEnough } else { BattleAffordability::AcceptorNotEnough });
    }

    let p_initiator_wins = match probability_pct {
        Some(pct) => pct as f64 / 100.0,
        None if p.features.skill_based_probability => {
            let (initiator_stats, acceptor_stats) = join!(
                p.repos.pvp_stats.get_stats(&chat_id_kind, initiator),
                p.repos.pvp_stats.get_stats(&chat_id_kind, acceptor),
            );
            win_probability(&initiator_stats?, &acceptor_stats?)
        },
        None => 0.5,
    };
    let (winner, loser) = choose_winner(initiator, acceptor, p_initiator_wins);
    // if the initiator won, the (possibly scaled) win_amount moves; otherwise it's just the
    // nominal bet, since the initiator's loss is always capped at it
    let bet = if winner == initiator { win_amount } else { bet };
    let (loser_new_length, winner_new_length) = Dicks::move_length_in_tx(tx, internal_chat_id, loser, winner, bet).await?;
    let winner_probability_pct = (if winner == initiator { p_initiator_wins } else { 1.0 - p_initiator_wins }) * 100.0;

    Ok(BattleAffordability::BothEnough(BattleCoreResult { winner, loser, bet, winner_new_length, loser_new_length, winner_probability_pct }))
}

/// Everything after the transfer itself is applied and committed: ledger, battle stats, debt
/// settlement, and building the result text - all best-effort/display, exactly as today for a
/// standalone battle (a failure here is logged, never rolled back) - so none of it needs to
/// share the transaction `pvp_core_in_tx` used.
pub(crate) async fn pvp_finish(p: &BattleParams, acceptor: &UserInfo, internal_chat_id: i64, core: BattleCoreResult) -> anyhow::Result<(String, Option<String>)> {
    let chat_id_kind = p.chat_id.kind();
    let winner_res = p.repos.dicks.growth_result_after(internal_chat_id, core.winner, core.winner_new_length).await?;
    let loser_res = p.repos.dicks.growth_result_after(internal_chat_id, core.loser, core.loser_new_length).await?;
    let bet_i32 = core.bet as i32;
    if let Err(e) = p.repos.ledger.record_many(&p.chat_id, repo::LedgerCategory::Pvp, &[(core.winner, bet_i32, Some(core.loser)), (core.loser, -bet_i32, Some(core.winner))]).await {
        log::error!("couldn't record ledger entries for a battle ({} beat {}, {}): {e}", core.winner, core.loser, core.bet);
    }

    let battle_stats = p.repos.pvp_stats.send_battle_result(&chat_id_kind, core.winner, core.loser, core.bet).await
        .inspect_err(|e| log::error!("couldn't send users' battle statistics for winner ({}) and loser ({}): {}", core.winner, core.loser, e))
        .ok()
        .filter(|_| p.features.show_stats)
        .map(|BattleStats { winner: winner_stats, loser: loser_stats }| {
            let mut stats_str = t!("commands.pvp.results.stats.text", locale = &p.lang_code,
                winner_win_rate = winner_stats.win_rate_formatted(), loser_win_rate = loser_stats.win_rate_formatted(),
                winner_win_streak = winner_stats.win_streak_current, winner_win_streak_max = winner_stats.win_streak_max,
            ).to_string();
            if loser_stats.prev_win_streak > 1 {
                stats_str.push('\n');
                stats_str.push_str(&t!("commands.pvp.results.stats.lost_win_streak", locale = &p.lang_code,
                    lost_win_streak = loser_stats.prev_win_streak));
            }
            // symmetric to the win-streak reporting above: the loser's own new lose streak (if
            // it's now actually a "streak", i.e. more than one), and - separately - the winner's
            // own lose streak getting snapped by this win, exactly like a win streak already was.
            if loser_stats.lose_streak_current > 1 {
                stats_str.push('\n');
                stats_str.push_str(&t!("commands.pvp.results.stats.lose_streak_current", locale = &p.lang_code,
                    lose_streak = loser_stats.lose_streak_current));
            }
            if winner_stats.prev_lose_streak > 1 {
                stats_str.push('\n');
                stats_str.push_str(&t!("commands.pvp.results.stats.snapped_lose_streak", locale = &p.lang_code,
                    snapped_lose_streak = winner_stats.prev_lose_streak));
            }
            stats_str
        });

    // a battle award is a gain just like regular growth, DoD, a donation, or a promo bonus,
    // so it must settle the winner's debts the same way (see
    // `crate::handlers::debt_settlement::settle_gain_against_debts`) - otherwise a borrower
    // could dodge automatic repayment entirely just by winning it from PVP instead. The
    // winner's full `bet` was already credited above by `move_length`, so whatever gets
    // withheld here has to be debited back from them explicitly.
    let settlement = settle_gain_against_debts(&p.repos, core.winner, &chat_id_kind, bet_i32, p.tax_bottom_ranks, true).await
        .inspect_err(|e| log::error!("couldn't settle debts from a battle award: {e}"))
        .unwrap_or_default();
    let withheld_part = settlement.message(&p.lang_code);
    let winner_res = if settlement.total_withheld > 0 {
        p.repos.dicks.grow_no_attempts_check(&chat_id_kind, core.winner, -settlement.total_withheld).await?
    } else {
        winner_res
    };

    let winner_info = get_user_info(&p.repos.users, core.winner, acceptor).await?;
    let loser_info = get_user_info(&p.repos.users, core.loser, acceptor).await?;
    let main_part = t!("commands.pvp.results.finish", locale = &p.lang_code,
        winner_name = winner_info.name.escaped(), winner_length = winner_res.new_length, loser_length = loser_res.new_length, bet = core.bet,
        winner_probability = format!("{:.2}%", core.winner_probability_pct));
    // the bet's own outcome and any debt automatically withheld from it stay visible by default;
    // the leaderboard positions and the win-rate/streak stats are returned separately so the
    // caller can defer them behind a Dettagli button (see `details::maybe_deferred`) - standalone
    // callers defer per-battle, while a combo leg (see `crate::handlers::combo::callback_handler`)
    // folds them into one combined details blob covering both legs.
    let short_text = format!("{main_part}{withheld_part}");
    let position_block = if let (Some(winner_pos), Some(loser_pos)) = (winner_res.pos_in_top, loser_res.pos_in_top) {
        let winner_pos = t!("commands.pvp.results.position.winner", locale = &p.lang_code, name = winner_info.name.escaped(), pos = winner_pos);
        let loser_pos = t!("commands.pvp.results.position.loser", locale = &p.lang_code, name = loser_info.name.escaped(), pos = loser_pos);
        Some(format!("{winner_pos}\n{loser_pos}"))
    } else {
        None
    };
    let details = [position_block, battle_stats].into_iter().flatten().collect::<Vec<_>>();
    let details = (!details.is_empty()).then(|| details.join("\n\n"));
    Ok((short_text, details))
}

pub(crate) async fn pvp_impl_attack(p: BattleParams, initiator: UserId, acceptor: UserInfo, bet: u16, probability_pct: Option<f64>,
                                    details_store: &DetailsStore) -> anyhow::Result<CallbackResult> {
    let internal_chat_id = p.repos.dicks.resolve_chat(&p.chat_id).await?;
    let mut tx = p.repos.dicks.begin_tx().await?;
    let affordability = pvp_core_in_tx(&mut tx, &p, internal_chat_id, initiator, acceptor.uid, bet, probability_pct).await?;
    let result = match affordability {
        BattleAffordability::InitiatorNotEnough => {
            CallbackResult::EditMessage(t!("commands.pvp.errors.not_enough.initiator", locale = &p.lang_code).to_string(), None)
        },
        BattleAffordability::AcceptorNotEnough => {
            CallbackResult::ShowError(t!("commands.pvp.errors.not_enough.acceptor", locale = &p.lang_code).to_string())
        },
        BattleAffordability::BothEnough(core) => {
            tx.commit().await?;
            // a battle names two people, neither of whom "owns" the result more than the other -
            // anyone may expand the Dettagli button.
            let (short_text, details) = pvp_finish(&p, &acceptor, internal_chat_id, core).await?;
            let (text, keyboard) = details::maybe_deferred(short_text, details, None, Some(details_store), &p.lang_code);
            CallbackResult::EditMessage(text, keyboard)
        }
    };
    Ok(result)
}

/// Fair-odds win amount for an explicit win-probability `pct` (1-99) of the initiator: the
/// initiator's potential LOSS always stays capped at the nominal `bet` (exactly like the classic
/// symmetric battle); only this WIN amount is scaled by the chosen probability. At pct=50 this
/// reduces to exactly `bet`, matching the classic battle. A high pct (self-favored) shrinks the
/// win to a fraction of the bet; a low pct (self-handicap) multiplies it well above the bet.
pub(crate) fn skewed_win_amount(bet: u16, pct: f64) -> u16 {
    let p = pct / 100.0;
    ((bet as f64) * (1.0 - p) / p).floor().clamp(0.0, u16::MAX as f64) as u16
}

/// The same skewed odds, but phrased from the ACCEPTOR's point of view - they're the one who
/// has to decide whether to accept, so the preview shown to them should describe their own
/// stakes, not the initiator's: their win probability (the complement of `pct`), what they gain
/// if they win (always the nominal `bet`, capped just like the initiator's own potential loss),
/// and what they stand to lose if they lose (the initiator's scaled win amount).
fn skewed_odds_for_acceptor(bet: u16, pct: f64) -> (f64, u16, u16) {
    // `100.0 - pct` is shown to the user verbatim (no `{:.N}` truncation at the call site, unlike
    // the post-battle text), so any f64 subtraction noise - e.g. 100.0 - 99.9999 landing on
    // 0.00009999999998636 instead of a clean 0.0001 - would otherwise leak straight into the
    // message as a string of meaningless decimals. Rounded to the same 4-decimal precision the
    // probability syntax itself advertises (see `parse_percentage`), same fix as `rate_to_pct`.
    let acceptor_probability = ((100.0 - pct) * 10000.0).round() / 10000.0;
    (acceptor_probability, bet, skewed_win_amount(bet, pct))
}

fn choose_winner<T>(initiator: T, acceptor: T, p_initiator_wins: f64) -> (T, T) {
    if OsRng.gen_bool(p_initiator_wins) {
        (initiator, acceptor)
    } else {
        (acceptor, initiator)
    }
}

/// A player who hasn't fought yet is treated as having a neutral (50%) win rate,
/// rather than the literal 0% their empty stats would otherwise yield.
fn effective_win_rate(stats: &repo::UserStats) -> f64 {
    if stats.battles_total == 0 {
        0.5
    } else {
        stats.win_rate_percentage() / 100.0
    }
}

/// Combines both players' win rates into the initiator's probability of winning this particular
/// battle: each player's chance is proportional to "1 - their own win rate" (the worse your
/// historical record, the better your odds), normalized so the two probabilities sum to 1.
fn win_probability(initiator_stats: &repo::UserStats, acceptor_stats: &repo::UserStats) -> f64 {
    let p_initiator = 1.0 - effective_win_rate(initiator_stats);
    let p_acceptor = 1.0 - effective_win_rate(acceptor_stats);
    let sum = p_initiator + p_acceptor;
    if sum <= 0.0 { 0.5 } else { p_initiator / sum }
}

pub(crate) async fn get_user_info(users: &repo::Users, user_uid: UserId, acceptor: &UserInfo) -> anyhow::Result<UserInfo> {
    let user = if user_uid == acceptor.uid {
        acceptor.clone()
    } else {
        users.get(user_uid).await?
            .ok_or(anyhow!("pvp participant must present in the database!"))?
            .into()
    };
    Ok(user)
}


pub fn new_short_timestamp() -> NewLayoutValue<i64> {
    NewLayoutValue::Some(chrono::Utc::now().timestamp_millis() - TIMESTAMP_MILLIS_SINCE_2024)
}

#[cfg(test)]
mod test_skewed_win_amount {
    use super::skewed_win_amount;

    #[test]
    fn fifty_fifty_reduces_to_the_classic_bet() {
        assert_eq!(skewed_win_amount(20, 50.0), 20);
        assert_eq!(skewed_win_amount(1, 50.0), 1);
    }

    #[test]
    fn favored_proposer_wins_a_fraction() {
        assert_eq!(skewed_win_amount(20, 90.0), 2);
        assert_eq!(skewed_win_amount(20, 95.0), 1);
    }

    #[test]
    fn underdog_proposer_wins_a_multiple() {
        assert_eq!(skewed_win_amount(20, 10.0), 180);
        assert_eq!(skewed_win_amount(20, 5.0), 380);
    }
}

#[cfg(test)]
mod test_skewed_odds_for_acceptor {
    use super::skewed_odds_for_acceptor;

    #[test]
    fn underdog_initiator_means_favored_acceptor() {
        // initiator self-handicapped at 20%: the acceptor is favored at 80%, and stands to
        // win only the nominal bet but risks the initiator's (much larger) scaled win amount
        assert_eq!(skewed_odds_for_acceptor(10, 20.0), (80.0, 10, 40));
    }

    #[test]
    fn favored_initiator_means_underdog_acceptor() {
        assert_eq!(skewed_odds_for_acceptor(20, 95.0), (5.0, 20, 1));
    }

    #[test]
    fn fifty_fifty_is_symmetric() {
        assert_eq!(skewed_odds_for_acceptor(20, 50.0), (50.0, 20, 20));
    }
}

