use anyhow::{anyhow, Context};
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, ChosenInlineResult, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery, InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputMessageContentText, Message, ParseMode, ReplyMarkup, UserId};
use crate::handlers::{details, CallbackResult, HandlerResult, offer_keyboard, reply_html, send_error_callback_answer, utils};
use crate::handlers::debt_settlement::settle_gain_against_debts;
use crate::{metrics, reply_html, repo};
use crate::config::AppConfig;
use crate::domain::{LanguageCode, Username};
use crate::handlers::pvp::{build_inline_target_error_result, get_user_info, new_short_timestamp, UserInfo};
use crate::handlers::utils::callbacks;
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackDataBuilder, NewLayoutValue};
use crate::handlers::utils::details_store::DetailsStore;
use crate::handlers::utils::locks::LockCallbackServiceFacade;
use crate::repo::{ChatIdKind, ChatIdPartiality, Dicks, Repositories};

#[derive(BotCommands, Clone, Copy)]
#[command(rename_rule = "lowercase")]
pub enum DonateCommands {
    // a negative amount means a "pull": a request instead of a gift (still requires the
    // other side to accept - see `donate_impl_accept`).
    #[command(description = "donate")]
    Donate(i32),
}

impl DonateCommands {
    fn amount(&self) -> i32 {
        match *self {
            Self::Donate(amount) => amount,
        }
    }
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum DonateCommandsNoArgs {
    Donate,
}

#[derive(derive_more::Display)]
#[display("{proposer}:{amount}:{timestamp}:{target}")]
pub(crate) struct DonateCallbackData {
    // whoever ran /donate; NOT necessarily the donor - for a "pull" (negative amount) they end
    // up as the receiver instead, once someone accepts (see `donate_impl_accept`)
    proposer: UserId,

    // negative means a "pull": the proposer is requesting, not gifting - accepting swaps the
    // donor/receiver roles
    amount: i32,
    timestamp: NewLayoutValue<i64>,

    // set when the donation was started as a reply to a specific player's message;
    // only that player may then accept it
    target: NewLayoutValue<UserId>,
}

impl DonateCallbackData {
    pub(super) fn new(proposer: UserId, amount: i32, target: Option<UserId>) -> Self {
        Self {
            proposer, amount,
            timestamp: new_short_timestamp(),
            target: target.into(),
        }
    }
}

impl CallbackDataWithPrefix for DonateCallbackData {
    fn prefix() -> &'static str {
        "donate"
    }
}

impl TryFrom<String> for DonateCallbackData {
    type Error = callbacks::InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let proposer = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let amount: i32 = callbacks::parse_part(&mut parts, &err, "amount")?;
        let timestamp = callbacks::parse_optional_part(&mut parts, &err)?;
        let target = callbacks::parse_optional_part::<_, u64>(&mut parts, &err)?.map(UserId);
        Ok(Self { proposer, amount, timestamp, target })
    }
}

/// The absolute value of the requested transfer, and whether it represents a "pull" (a request -
/// the proposer wants to RECEIVE, swapping the usual donor/receiver roles). Shared with p2p loans,
/// which have the exact same negative-amount-means-a-pull convention.
pub(crate) fn split_amount(amount: i32) -> (u16, bool) {
    (amount.unsigned_abs().min(u16::MAX as u32) as u16, amount < 0)
}

pub async fn cmd_handler(bot: Bot, msg: Message, cmd: DonateCommands,
                         repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::CMD_DONATE_COUNTER.chat.inc();

    let proposer: UserInfo = msg.from.as_ref().ok_or(anyhow!("no FROM field in the donate command handler"))?.into();
    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    let target: Option<UserInfo> = msg.reply_to_message().and_then(|m| m.from.clone()).map(UserInfo::from);
    if let Some(target) = &target {
        if target.uid == proposer.uid {
            reply_html!(bot, msg, t!("commands.donate.errors.same_person", locale = &lang_code));
            return Ok(());
        }
    }

    let params = DonateParams {
        repos,
        chat_id: msg.chat.id.into(),
        lang_code,
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let (text, keyboard) = donate_impl_start(params, proposer, cmd.amount(), target).await?;

    let mut answer = reply_html(bot, &msg, text);
    answer.reply_markup = keyboard.map(ReplyMarkup::InlineKeyboard);
    answer.await?;
    Ok(())
}

pub async fn cmd_handler_no_args(bot: Bot, msg: Message) -> HandlerResult {
    metrics::CMD_DONATE_COUNTER.chat.inc();

    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    reply_html!(bot, msg, t!("commands.donate.errors.no_args", locale = &lang_code));
    Ok(())
}

pub fn inline_filter(query: InlineQuery) -> bool {
    utils::inline_target::parse_donate_inline_query(&query.query).is_some()
}

pub fn chosen_inline_result_filter(result: ChosenInlineResult) -> bool {
    utils::inline_target::parse_donate_inline_query(&result.query).is_some()
}

pub async fn inline_handler(bot: Bot, query: InlineQuery, repos: Repositories) -> HandlerResult {
    metrics::INLINE_COUNTER.invoked();

    let (amount, maybe_target_name) = utils::inline_target::parse_donate_inline_query(&query.query)
        .ok_or_else(|| anyhow!("inline query '{}' couldn't be parsed by the donate handler", query.query))?;
    let lang_code = LanguageCode::from_user(&query.from);
    let name = utils::get_full_name(&query.from);

    let res = match maybe_target_name {
        None => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, amount, None),
        Some(target_name) => match repos.users.find_by_exact_name(&target_name).await?.as_slice() {
            [] => build_inline_target_error_result("commands.donate.errors.target_not_found", &lang_code, &target_name),
            [target] => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, amount, Some(target)),
            _ => build_inline_target_error_result("commands.donate.errors.target_ambiguous", &lang_code, &target_name),
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

pub async fn inline_chosen_handler() -> HandlerResult {
    metrics::INLINE_COUNTER.finished();
    Ok(())
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    DonateCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories, config: AppConfig,
                              mut donate_locker: LockCallbackServiceFacade, details_store: DetailsStore) -> HandlerResult {
    let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);

    let callback_data = DonateCallbackData::parse(&query)?;
    if callback_data.proposer == query.from.id {
        return send_error_callback_answer(bot, query, "commands.donate.errors.same_person").await;
    }
    if let NewLayoutValue::Some(target) = callback_data.target {
        if target != query.from.id {
            return send_error_callback_answer(bot, query, "commands.donate.errors.not_target").await;
        }
    }
    let _donate_guard = match donate_locker.try_lock(&callback_data) {
        Some(lock) => lock,
        None => return send_error_callback_answer(bot, query, "commands.donate.errors.already_in_progress").await
    };

    let params = DonateParams {
        repos,
        chat_id: chat_id.clone(),
        lang_code: LanguageCode::from_user(&query.from),
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let result = donate_impl_accept(params, callback_data.proposer, query.from.clone().into(), callback_data.amount, &details_store).await?;
    result.apply(bot, query).await?;

    metrics::CMD_DONATE_COUNTER.inline.inc();
    Ok(())
}

pub(crate) struct DonateParams {
    pub(crate) repos: Repositories,
    pub(crate) chat_id: ChatIdPartiality,
    pub(crate) lang_code: LanguageCode,
    pub(crate) tax_bottom_ranks: usize,
}

/// The free-form description of a donate offer (no button), shared by the slash-command path,
/// the inline-query path, and combo offers - so all three read identically for the same
/// parameters.
pub(crate) fn donate_offer_text(name: &Username, target_name: Option<&Username>, amount: i32, lang_code: &LanguageCode) -> String {
    let (abs_amount, is_pull) = split_amount(amount);
    let text_key = match (target_name.is_some(), is_pull) {
        (true, true) => "commands.donate.results.request_targeted",
        (false, true) => "commands.donate.results.request",
        (true, false) => "commands.donate.results.start_targeted",
        (false, false) => "commands.donate.results.start",
    };
    match target_name {
        Some(target_name) => t!(text_key, locale = lang_code,
            name = name.escaped(), target_name = target_name.escaped(), amount = abs_amount).to_string(),
        None => t!(text_key, locale = lang_code, name = name.escaped(), amount = abs_amount).to_string(),
    }
}

pub(crate) async fn donate_impl_start(p: DonateParams, proposer: UserInfo, amount: i32, target: Option<UserInfo>) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let (abs_amount, is_pull) = split_amount(amount);
    // pushing (a gift) requires the proposer to have enough right now; pulling (a request)
    // doesn't, since the proposer isn't the one paying - whoever accepts is checked then.
    let enough = if is_pull {
        true
    } else {
        p.repos.dicks.check_dick(&p.chat_id.kind(), proposer.uid, abs_amount).await?
    };
    log::debug!("Starting a donation from {} in the chat with id = {} (amount = {amount}, enough = {enough})...", proposer.uid, p.chat_id);

    let data = if enough {
        let text = donate_offer_text(&proposer.name, target.as_ref().map(|t| &t.name), amount, &p.lang_code);
        let btn_label_key = if is_pull { "commands.donate.button_pull" } else { "commands.donate.button" };
        let btn_label = t!(btn_label_key, locale = &p.lang_code);
        let target_uid = target.map(|t| t.uid);
        let btn_data = DonateCallbackData::new(proposer.uid, amount, target_uid).to_data_string();
        let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
        let keyboard = offer_keyboard(accept_btn, proposer.uid, target_uid, &p.lang_code);
        (text, Some(keyboard))
    } else {
        (t!("commands.donate.errors.not_enough", locale = &p.lang_code).to_string(), None)
    };
    Ok(data)
}

/// What `donate_core_in_tx` actually did, carried forward to `donate_finish` so it doesn't have
/// to re-derive the donor/receiver direction or re-read the lengths it already has in hand.
pub(crate) struct DonateCoreResult {
    donor_id: UserId,
    receiver_id: UserId,
    abs_amount: u16,
    donor_len: i32,
    receiver_len: i32,
}

/// The core of accepting a donation - resolving the pull/push direction and (if the donor can
/// currently afford it) transferring the length - against an externally owned `tx` that this
/// never commits, exactly like `Dicks::move_length_in_tx`. `None` means the donor can't
/// currently afford it (nothing applied) - the caller decides how to report that.
pub(crate) async fn donate_core_in_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, chat_id_kind: &ChatIdKind, internal_chat_id: i64, proposer_id: UserId, acceptor_id: UserId, amount: i32) -> anyhow::Result<Option<DonateCoreResult>> {
    let (abs_amount, is_pull) = split_amount(amount);
    // a "pull" means the proposer was requesting, not gifting: accepting swaps the roles, so
    // the acceptor becomes the donor and the proposer becomes the receiver.
    let (donor_id, receiver_id) = if is_pull { (acceptor_id, proposer_id) } else { (proposer_id, acceptor_id) };

    let enough_donor = Dicks::check_dick_with(&mut **tx, chat_id_kind, donor_id, abs_amount).await?;
    log::debug!("Executing the donation: donor = {donor_id} (enough = {enough_donor}), receiver = {receiver_id}, amount = {abs_amount}...");
    if !enough_donor {
        return Ok(None);
    }

    let (donor_len, receiver_len) = Dicks::move_length_in_tx(tx, internal_chat_id, donor_id, receiver_id, abs_amount).await?;
    Ok(Some(DonateCoreResult { donor_id, receiver_id, abs_amount, donor_len, receiver_len }))
}

/// Everything after the transfer itself is applied and committed: ledger, debt settlement, and
/// building the result text - all best-effort/display, exactly as today for a standalone
/// donation (a failure here is logged, never rolled back) - so none of it needs to share the
/// transaction `donate_core_in_tx` used.
pub(crate) async fn donate_finish(p: &DonateParams, acceptor: &UserInfo, internal_chat_id: i64, core: DonateCoreResult) -> anyhow::Result<(String, Option<String>)> {
    let chat_id_kind = p.chat_id.kind();
    let donor_res = p.repos.dicks.growth_result_after(internal_chat_id, core.donor_id, core.donor_len).await?;
    let receiver_res = p.repos.dicks.growth_result_after(internal_chat_id, core.receiver_id, core.receiver_len).await?;
    let amount_i32 = core.abs_amount as i32;
    if let Err(e) = p.repos.ledger.record_many(&p.chat_id, repo::LedgerCategory::Donate, &[(core.donor_id, -amount_i32, Some(core.receiver_id)), (core.receiver_id, amount_i32, Some(core.donor_id))]).await {
        log::error!("couldn't record ledger entries for a donation ({} -> {}, {}): {e}", core.donor_id, core.receiver_id, core.abs_amount);
    }

    // a donation received is a gain just like any other, so it must settle the receiver's
    // debts too (see `crate::handlers::debt_settlement::settle_gain_against_debts`) -
    // otherwise a player could dodge automatic repayment by having someone donate to them
    // instead of growing themselves. The donor's side is a debit, not a gain, so it never
    // settles anything.
    let settlement = settle_gain_against_debts(&p.repos, core.receiver_id, &chat_id_kind, amount_i32, p.tax_bottom_ranks, true).await
        .inspect_err(|e| log::error!("couldn't settle the receiver's debts from a donation: {e}"))
        .unwrap_or_default();
    let withheld_part = settlement.message(&p.lang_code);
    let receiver_res = if settlement.total_withheld > 0 {
        p.repos.dicks.grow_no_attempts_check(&chat_id_kind, core.receiver_id, -settlement.total_withheld).await?
    } else {
        receiver_res
    };

    let donor_info = get_user_info(&p.repos.users, core.donor_id, acceptor).await?;
    let receiver_info = get_user_info(&p.repos.users, core.receiver_id, acceptor).await?;
    let main_part = t!("commands.donate.results.finish", locale = &p.lang_code,
        donor_name = donor_info.name.escaped(), receiver_name = receiver_info.name.escaped(), amount = core.abs_amount,
        donor_length = donor_res.new_length, receiver_length = receiver_res.new_length);
    // the transfer's own outcome and any debt automatically withheld from it stay visible by
    // default; the leaderboard positions are returned separately so the caller can defer them
    // behind a Dettagli button (see `details::maybe_deferred`) - standalone callers defer
    // per-donation, while a combo leg (see `crate::handlers::combo::callback_handler`) folds them
    // into one combined details blob covering both legs.
    let short_text = format!("{main_part}{withheld_part}");
    let details = if let (Some(donor_pos), Some(receiver_pos)) = (donor_res.pos_in_top, receiver_res.pos_in_top) {
        let donor_pos = t!("commands.donate.results.position.donor", locale = &p.lang_code, name = donor_info.name.escaped(), pos = donor_pos);
        let receiver_pos = t!("commands.donate.results.position.receiver", locale = &p.lang_code, name = receiver_info.name.escaped(), pos = receiver_pos);
        Some(format!("{donor_pos}\n{receiver_pos}"))
    } else {
        None
    };
    Ok((short_text, details))
}

pub(crate) async fn donate_impl_accept(p: DonateParams, proposer_id: UserId, acceptor: UserInfo, amount: i32,
                                       details_store: &DetailsStore) -> anyhow::Result<CallbackResult> {
    let chat_id_kind = p.chat_id.kind();
    let internal_chat_id = p.repos.dicks.resolve_chat(&p.chat_id).await?;
    let mut tx = p.repos.dicks.begin_tx().await?;
    let core = donate_core_in_tx(&mut tx, &chat_id_kind, internal_chat_id, proposer_id, acceptor.uid, amount).await?;
    let result = match core {
        None => CallbackResult::ShowError(t!("commands.donate.errors.not_enough", locale = &p.lang_code).to_string()),
        Some(core) => {
            tx.commit().await?;
            let (short_text, details) = donate_finish(&p, &acceptor, internal_chat_id, core).await?;
            let (text, keyboard) = details::maybe_deferred(short_text, details, None, Some(details_store), &p.lang_code);
            CallbackResult::EditMessage(text, keyboard)
        }
    };
    Ok(result)
}

pub(super) fn build_inline_keyboard_article_result(uid: UserId, lang_code: &LanguageCode, name: &Username, amount: i32, target: Option<&repo::User>) -> InlineQueryResult {
    log::debug!("Offering a donation from {uid} (amount = {amount}, target = {target:?})...");

    let (abs_amount, is_pull) = split_amount(amount);
    let title_key = if is_pull { "inline.results.titles.donate_pull" } else { "inline.results.titles.donate" };
    let title = t!(title_key, locale = lang_code, amount = abs_amount);
    let text = donate_offer_text(name, target.map(|t| &t.name), amount, lang_code);
    let content = InputMessageContent::Text(InputMessageContentText::new(text).parse_mode(ParseMode::Html));
    let btn_label_key = if is_pull { "commands.donate.button_pull" } else { "commands.donate.button" };
    let btn_label = t!(btn_label_key, locale = lang_code);
    let target_uid = target.map(|t| UserId(t.uid as u64));
    let btn_data = DonateCallbackData::new(uid, amount, target_uid).to_data_string();
    let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
    InlineQueryResultArticle::new("donate", title, content)
        .reply_markup(offer_keyboard(accept_btn, uid, target_uid, lang_code))
        .into()
}

#[cfg(test)]
mod test_debt_settlement {
    use teloxide::types::UserId;
    use crate::config::AppConfig;
    use crate::domain::LanguageCode;
    use crate::handlers::donate::{donate_impl_accept, DonateParams};
    use crate::handlers::pvp::UserInfo;
    use crate::repo;
    use crate::repo::test::dicks::{create_another_user_and_dick, create_user};
    use crate::repo::test::{get_chat_id_and_dicks, start_postgres, CHAT_ID_KIND, UID};

    /// A donation received is a gain just like any other - an indebted receiver must have part
    /// of it withheld for their bank loan, exactly as if they'd grown it themselves. The donor's
    /// own side is a debit, not a gain, so nothing of theirs is ever withheld.
    #[tokio::test]
    async fn test_donation_received_settles_the_receivers_bank_loan() {
        let (_container, db) = start_postgres().await;
        let (chat_id, dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

        create_user(&db).await; // UID - the donor
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "receiver", 0).await;
        let donor_uid = UserId(UID as u64);
        let receiver_uid = UserId((UID + 1) as u64);

        let cfg = AppConfig { loan_payout_ratio: 0.5, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);
        repos.loans.borrow(receiver_uid, &CHAT_ID_KIND, 10).await.expect("couldn't create a loan");
        dicks.create_or_grow(donor_uid, &chat_id_partiality, 100).await.expect("couldn't fund the donor");

        let params = DonateParams {
            repos: repos.clone(),
            chat_id: chat_id_partiality.clone(),
            lang_code: LanguageCode::new("lmo".to_owned()),
            tax_bottom_ranks: 0,
        };
        let donor = UserInfo { uid: donor_uid, name: "donor".to_owned().into() };
        let acceptor = UserInfo { uid: receiver_uid, name: "receiver".to_owned().into() };
        let details_store = crate::handlers::utils::details_store::DetailsStore::default();
        donate_impl_accept(params, donor.uid, acceptor, 20, &details_store).await.expect("couldn't accept the donation");

        // receiver: borrow() already credited +10 upfront, then +20 from the donation, then -10
        // (50% of 20, capped at the 10-ghei debt) withheld -> 10 + 20 - 10 = 20.
        let receiver_length = dicks.fetch_length(receiver_uid, &CHAT_ID_KIND).await.expect("couldn't fetch the receiver's length");
        assert_eq!(receiver_length, 20);
        let active_loan = repos.loans.get_active_loan(receiver_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch the active loan");
        assert!(active_loan.is_none(), "the 10-ghei loan must be fully repaid (and thus closed) by the withheld half of the donation");

        let donor_length = dicks.fetch_length(donor_uid, &CHAT_ID_KIND).await.expect("couldn't fetch the donor's length");
        assert_eq!(donor_length, 80, "the donor's own side is a debit, never subject to debt settlement");
    }
}
