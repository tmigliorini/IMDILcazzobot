use anyhow::{anyhow, Context};
use futures::join;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, ChosenInlineResult, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery, InlineQueryResult, InlineQueryResultArticle, InputMessageContent, InputMessageContentText, Message, ParseMode, ReplyMarkup, UserId};
use crate::handlers::{details, CallbackResult, FromRefs, HandlerResult, offer_keyboard, reply_html, send_error_callback_answer, utils};
use crate::{check_invoked_by_owner_and_get_answer_params, metrics, reply_html, repo};
use crate::config::AppConfig;
use crate::domain::{LanguageCode, Username};
use crate::handlers::donate::split_amount;
use crate::handlers::pvp::{build_inline_target_error_result, get_user_info, new_short_timestamp, UserInfo};
use crate::handlers::utils::callbacks;
use crate::handlers::utils::callbacks::{CallbackDataWithPrefix, InvalidCallbackDataBuilder, NewLayoutValue};
use crate::handlers::utils::details_store::DetailsStore;
use crate::handlers::utils::locks::LockCallbackServiceFacade;
use crate::handlers::utils::page::Page;
use crate::repo::{compute_interest, ChatIdKind, ChatIdPartiality, Dicks, NoChatIdError, Repositories};

#[derive(BotCommands, Clone, Copy)]
#[command(rename_rule = "lowercase")]
pub enum P2PLoanCommands {
    // a negative amount means a "pull": a request to BORROW, not to lend (still requires the
    // other side to accept - see `p2p_loan_impl_accept`).
    #[command(description = "presta")]
    Presta(i32),
}

impl P2PLoanCommands {
    fn amount(&self) -> i32 {
        match *self {
            Self::Presta(amount) => amount,
        }
    }
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum P2PLoanCommandsNoArgs {
    Presta,
}

#[derive(BotCommands, Clone, Copy)]
#[command(rename_rule = "lowercase")]
pub enum P2PLoanStatusCommands {
    #[command(description = "debiti")]
    Debiti,
}

/// Entries are richer than a plain ledger line (each is up to two lines: the loan/tax-debt
/// itself, plus an optional capital/interest breakdown), so the page is kept smaller than
/// `statement`'s - comfortably under Telegram's 4096-character message limit even if every entry
/// on a page happens to be the longer, two-line kind.
const DEBITI_PAGE_SIZE: usize = 12;

pub async fn status_cmd_handler(bot: Bot, msg: Message, repos: Repositories) -> HandlerResult {
    metrics::CMD_P2P_LOAN_STATUS_COUNTER.chat.inc();

    let from = msg.from.as_ref().ok_or(anyhow!("no FROM field in the p2p loan status command handler"))?;
    let lang_code = LanguageCode::from_user(from);
    let chat_id: ChatIdPartiality = msg.chat.id.into();
    let status = p2p_loan_status_impl(&repos, FromRefs(from, &chat_id), Page::first()).await?;

    let mut request = reply_html(bot, &msg, status.lines);
    if status.has_more_pages {
        let keyboard = build_debiti_pagination_keyboard(from.id, Page::first(), status.has_more_pages, &lang_code);
        request.reply_markup.replace(ReplyMarkup::InlineKeyboard(keyboard));
    }
    request.await.context(format!("failed for {msg:?}"))?;
    Ok(())
}

pub(crate) struct DebitiStatus {
    pub(crate) lines: String,
    pub(crate) has_more_pages: bool,
}

/// The status of every active loan `from_refs`' user has in this chat, on both sides: what they
/// still owe (as borrower, including the cut of every future growth it costs them) and what's
/// still owed to them (as lender) - the "amortization schedule" view requested for the inline
/// "debiti" button. The grand totals are always shown in full (computed over *every* active
/// obligation, regardless of `page`); the individual loan/tax-debt entries below them are paged,
/// since someone with many active loans could otherwise blow past Telegram's message-length limit.
pub(crate) async fn p2p_loan_status_impl(repos: &Repositories, from_refs: FromRefs<'_>, page: Page) -> anyhow::Result<DebitiStatus> {
    let (from, chat_id) = (from_refs.0, from_refs.1);
    let lang_code = LanguageCode::from_user(from);
    let chat_id_kind = chat_id.kind();

    let (as_borrower, as_lender, tax_debts) = join!(
        repos.p2p_loans.get_active_loans_as_borrower(from.id, &chat_id_kind),
        repos.p2p_loans.get_active_loans_as_lender(from.id, &chat_id_kind),
        repos.loan_interest_tax_debts.get_active_with_origin(from.id, &chat_id_kind),
    );
    let (as_borrower, as_lender, tax_debts) = (as_borrower?, as_lender?, tax_debts?);

    if as_borrower.is_empty() && as_lender.is_empty() && tax_debts.is_empty() {
        let lines = t!("commands.debiti.no_loans", locale = &lang_code).to_string();
        return Ok(DebitiStatus { lines, has_more_pages: false })
    }

    // every row's `debt` is non-negative and unambiguous now (see `repo::P2PLoans::lend`), so
    // the grand totals are a plain sum - no sign-flip bookkeeping needed. Tax debts only ever
    // count against `from` (there's no lender side to one), so they only add to what's owed.
    // Computed over the *full* data, before pagination splits it up below.
    let total_owed_by_me: i32 = as_borrower.iter().map(|l| l.debt).sum::<i32>()
        + tax_debts.iter().map(|d| d.amount_owed).sum::<i32>();
    let total_owed_to_me: i32 = as_lender.iter().map(|l| l.debt).sum();
    let net = total_owed_to_me - total_owed_by_me;
    let net_clause = match net.cmp(&0) {
        std::cmp::Ordering::Greater => t!("commands.debiti.totals.net.creditor", locale = &lang_code, net = net).to_string(),
        std::cmp::Ordering::Less => t!("commands.debiti.totals.net.debtor", locale = &lang_code, net = net.abs()).to_string(),
        std::cmp::Ordering::Equal => t!("commands.debiti.totals.net.even", locale = &lang_code).to_string(),
    };
    let totals_title = t!("commands.debiti.totals.title", locale = &lang_code);
    let totals_line = t!("commands.debiti.totals.line", locale = &lang_code,
        owed_by_me = total_owed_by_me, owed_to_me = total_owed_to_me);
    let totals_block = format!("{totals_title}\n{totals_line}\n{net_clause}");

    // every row is unambiguous now: a "borrower" row is genuinely owed by `from`, and a "lender"
    // row is genuinely owed to `from` - including the reciprocal row of a negative-rate loan
    // `from` themselves lent (see `repo::P2PLoans::lend`), which simply shows up here as an
    // ordinary "borrower" row. Loans are grouped per counterparty (one header naming the person
    // and the total still owed to/by them, then one indented line per individual loan), so the
    // same name isn't repeated for every separate loan. Each group is its own pageable block; a
    // section's title rides along with its first group only.
    let mut blocks = Vec::with_capacity(as_borrower.len() + as_lender.len() + tax_debts.len());
    for (i, group) in group_by_counterparty(&as_borrower).into_iter().enumerate() {
        let block = format_loan_group(&lang_code, "as_borrower", group);
        blocks.push(if i == 0 {
            format!("{}\n{block}", t!("commands.debiti.as_borrower.title", locale = &lang_code))
        } else {
            block
        });
    }
    for (i, group) in group_by_counterparty(&as_lender).into_iter().enumerate() {
        let block = format_loan_group(&lang_code, "as_lender", group);
        blocks.push(if i == 0 {
            format!("{}\n{block}", t!("commands.debiti.as_lender.title", locale = &lang_code))
        } else {
            block
        });
    }
    // a tax debt has no single creditor (it's redistributed to whoever's at the bottom of the
    // chat's ranking at collection time - see `repo::LoanInterestTaxDebts`), but it does have a
    // traceable *origin*: the p2p loan whose interest produced it (`source_loan_id`), shown here
    // via the loan's other party's name when it's still resolvable.
    for (i, d) in tax_debts.iter().enumerate() {
        let pct = repaid_pct(d.amount_owed, d.original_debt);
        let line = match &d.origin_counterparty_name {
            Some(name) => t!("commands.debiti.tax_debt.line_with_origin", locale = &lang_code,
                debt = d.amount_owed, original_debt = d.original_debt, pct = pct, name = Username::new(name.clone()).escaped()).to_string(),
            None => t!("commands.debiti.tax_debt.line", locale = &lang_code,
                debt = d.amount_owed, original_debt = d.original_debt, pct = pct).to_string(),
        };
        blocks.push(if i == 0 {
            format!("{}\n{line}", t!("commands.debiti.tax_debt.title", locale = &lang_code))
        } else {
            line
        });
    }

    let offset = page.0 as usize * DEBITI_PAGE_SIZE;
    let has_more_pages = offset + DEBITI_PAGE_SIZE < blocks.len();
    let page_body = blocks.into_iter().skip(offset).take(DEBITI_PAGE_SIZE).collect::<Vec<_>>().join("\n\n");
    let lines = format!("{totals_block}\n\n{page_body}");
    Ok(DebitiStatus { lines, has_more_pages })
}

/// Pagination for `p2p_loan_status_impl`'s entries, mirroring `statement`'s own callback data -
/// personal data, so it carries `uid` to reject anyone else's clicks.
#[derive(derive_more::Display)]
#[display("{uid}:{page}")]
struct DebitiCallbackData {
    uid: UserId,
    page: u32,
}

impl CallbackDataWithPrefix for DebitiCallbackData {
    fn prefix() -> &'static str {
        "debiti"
    }
}

impl TryFrom<String> for DebitiCallbackData {
    type Error = callbacks::InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let uid = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let page = callbacks::parse_part(&mut parts, &err, "page")?;
        Ok(Self { uid, page })
    }
}

/// Always carries a second row back to the Info menu (see `info::back_button`) underneath the
/// pagination row, on *every* page - not just the first one reached from that menu - so paging
/// through many loans never strands the player without a way back.
pub(crate) fn build_debiti_pagination_keyboard(uid: UserId, page: Page, has_more_pages: bool, lang_code: &LanguageCode) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();
    if page > 0 {
        let data = DebitiCallbackData { uid, page: page.0 - 1 }.to_data_string();
        buttons.push(InlineKeyboardButton::callback("⬅️", data));
    }
    // re-requests this exact page as-is, for a fresher loan list without losing position.
    let refresh_data = DebitiCallbackData { uid, page: page.0 }.to_data_string();
    buttons.push(InlineKeyboardButton::callback("🔄", refresh_data));
    if has_more_pages {
        let data = DebitiCallbackData { uid, page: page.0 + 1 }.to_data_string();
        buttons.push(InlineKeyboardButton::callback("➡️", data));
    }
    InlineKeyboardMarkup::new(vec![buttons]).append_row(vec![crate::handlers::info::back_button(uid, lang_code)])
}

#[inline]
pub fn debiti_callback_filter(query: CallbackQuery) -> bool {
    DebitiCallbackData::check_prefix(query)
}

pub async fn debiti_callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories) -> HandlerResult {
    let data = DebitiCallbackData::parse(&query)?;
    let (answer, lang_code) = check_invoked_by_owner_and_get_answer_params!(bot, query, data.uid);

    let edit_msg_req_params = callbacks::get_params_for_message_edit(&query)?;
    let chat_id_kind = edit_msg_req_params.clone().into();
    let chat_id_partiality = ChatIdPartiality::Specific(chat_id_kind);
    let from_refs = FromRefs(&query.from, &chat_id_partiality);
    let page = Page(data.page);
    let status = p2p_loan_status_impl(&repos, from_refs, page).await?;
    let keyboard = build_debiti_pagination_keyboard(data.uid, page, status.has_more_pages, &lang_code);

    match edit_msg_req_params {
        callbacks::EditMessageReqParamsKind::Chat(chat_id, message_id) => {
            let mut req = bot.edit_message_text(chat_id, message_id, status.lines);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup.replace(keyboard);
            req.await?;
        }
        callbacks::EditMessageReqParamsKind::Inline { inline_message_id, .. } => {
            let mut req = bot.edit_message_text_inline(inline_message_id, status.lines);
            req.parse_mode.replace(ParseMode::Html);
            req.reply_markup.replace(keyboard);
            req.await?;
        }
    }
    answer.await?;
    Ok(())
}

#[derive(derive_more::Display)]
#[display("{proposer}:{amount}:{timestamp}:{target}:{interest_rate}")]
pub(crate) struct P2PLoanCallbackData {
    // whoever ran /presta; NOT necessarily the lender - for a "pull" (negative amount) they end
    // up as the borrower instead, once someone accepts (see `p2p_loan_impl_accept`)
    proposer: UserId,

    // negative means a "pull": the proposer is requesting to borrow, not offering to lend -
    // accepting swaps the lender/borrower roles
    amount: i32,
    timestamp: NewLayoutValue<i64>,

    // set when the loan was offered as a reply to a specific player's message;
    // only that player may then accept it
    target: NewLayoutValue<UserId>,

    // an explicit interest rate (as a percentage, e.g. 40.0), overriding the configured default
    // either way (lending or requesting)
    interest_rate: NewLayoutValue<f64>,
}

impl P2PLoanCallbackData {
    pub(super) fn new(proposer: UserId, amount: i32, target: Option<UserId>, interest_rate_pct: Option<f64>) -> Self {
        Self {
            proposer, amount,
            timestamp: new_short_timestamp(),
            target: target.into(),
            interest_rate: interest_rate_pct.into(),
        }
    }
}

impl CallbackDataWithPrefix for P2PLoanCallbackData {
    fn prefix() -> &'static str {
        "p2ploan"
    }
}

impl TryFrom<String> for P2PLoanCallbackData {
    type Error = callbacks::InvalidCallbackData;

    fn try_from(data: String) -> Result<Self, Self::Error> {
        let err = InvalidCallbackDataBuilder(&data);
        let mut parts = data.split(':');
        let proposer = callbacks::parse_part(&mut parts, &err, "uid").map(UserId)?;
        let amount: i32 = callbacks::parse_part(&mut parts, &err, "amount")?;
        let timestamp = callbacks::parse_optional_part(&mut parts, &err)?;
        let target = callbacks::parse_optional_part::<_, u64>(&mut parts, &err)?.map(UserId);
        let interest_rate = callbacks::parse_optional_part(&mut parts, &err)?;
        Ok(Self { proposer, amount, timestamp, target, interest_rate })
    }
}

pub async fn cmd_handler(bot: Bot, msg: Message, cmd: P2PLoanCommands,
                         repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::CMD_P2P_LOAN_COUNTER.chat.inc();

    let proposer: UserInfo = msg.from.as_ref().ok_or(anyhow!("no FROM field in the p2p loan command handler"))?.into();
    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    let target: Option<UserInfo> = msg.reply_to_message().and_then(|m| m.from.clone()).map(UserInfo::from);
    if let Some(target) = &target {
        if target.uid == proposer.uid {
            reply_html!(bot, msg, t!("commands.presta.errors.same_person", locale = &lang_code));
            return Ok(());
        }
    }

    let params = P2PLoanParams {
        repos,
        chat_id: msg.chat.id.into(),
        lang_code,
        interest_rate: config.p2p_loan_interest_rate,
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let (text, keyboard) = p2p_loan_impl_start(params, proposer, cmd.amount(), target, None).await?;

    let mut answer = reply_html(bot, &msg, text);
    answer.reply_markup = keyboard.map(ReplyMarkup::InlineKeyboard);
    answer.await?;
    Ok(())
}

pub async fn cmd_handler_no_args(bot: Bot, msg: Message) -> HandlerResult {
    metrics::CMD_P2P_LOAN_COUNTER.chat.inc();

    let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
    reply_html!(bot, msg, t!("commands.presta.errors.no_args", locale = &lang_code));
    Ok(())
}

pub fn inline_filter(query: InlineQuery) -> bool {
    utils::inline_target::parse_p2p_loan_inline_query(&query.query).is_some()
}

pub fn chosen_inline_result_filter(result: ChosenInlineResult) -> bool {
    utils::inline_target::parse_p2p_loan_inline_query(&result.query).is_some()
}

pub async fn inline_handler(bot: Bot, query: InlineQuery, repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::INLINE_COUNTER.invoked();

    let parsed = utils::inline_target::parse_p2p_loan_inline_query(&query.query)
        .ok_or_else(|| anyhow!("inline query '{}' couldn't be parsed by the p2p loan handler", query.query))?;
    let lang_code = LanguageCode::from_user(&query.from);
    let name = utils::get_full_name(&query.from);

    let (abs_amount, _) = split_amount(parsed.amount);
    let rate_pct = parsed.interest_rate_pct.unwrap_or(rate_to_pct(config.p2p_loan_interest_rate));
    let res = match compute_interest(abs_amount, (rate_pct / 100.0) as f32) {
        None => {
            let text = t!("commands.presta.errors.rate_too_high", locale = &lang_code, rate = rate_pct, amount = abs_amount).to_string();
            let content = InputMessageContent::Text(InputMessageContentText::new(&text));
            InlineQueryResultArticle::new("presta-rate-too-high", text, content).into()
        },
        Some(interest) => match parsed.target_name {
            None => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, parsed.amount, rate_pct, interest, parsed.interest_rate_pct, None),
            Some(target_name) => match repos.users.find_by_exact_name(&target_name).await?.as_slice() {
                [] => build_inline_target_error_result("commands.presta.errors.target_not_found", &lang_code, &target_name),
                [target] => build_inline_keyboard_article_result(query.from.id, &lang_code, &name, parsed.amount, rate_pct, interest, parsed.interest_rate_pct, Some(target)),
                _ => build_inline_target_error_result("commands.presta.errors.target_ambiguous", &lang_code, &target_name),
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

/// An inline query carries no chat context (Telegram only reveals which chat a result actually
/// landed in once it's chosen and sent - via `inline_message_id` below), so `inline_handler`
/// can't check whether the proposer can actually afford to lend; the offer is shown
/// optimistically. This is the earliest point that chat context exists, so it's the earliest
/// `check_dick` can run - if the proposer can't afford their own offer (e.g. they already spent
/// it on an earlier, identical-looking offer), the message is edited to say so instead of
/// leaving a live "accept" button that can only ever fail.
pub async fn inline_chosen_handler(bot: Bot, result: ChosenInlineResult, repos: Repositories) -> HandlerResult {
    metrics::INLINE_COUNTER.finished();

    let parsed = utils::inline_target::parse_p2p_loan_inline_query(&result.query)
        .ok_or_else(|| anyhow!("chosen p2p loan inline result '{}' couldn't be re-parsed", result.query))?;
    let (abs_amount, is_pull) = split_amount(parsed.amount);
    if is_pull {
        // a "pull" is a request to borrow, not an offer to lend - the proposer doesn't need
        // anything right now; whoever accepts becomes the lender and is checked then.
        return Ok(())
    }

    let maybe_chat = match result.inline_message_id.as_ref().and_then(crate::handlers::inline::try_resolve_chat_id) {
        Some(chat_id) => repos.chats.get_chat(chat_id.into()).await?
            .filter(|c| c.chat_id.is_some() && c.chat_instance.is_some()),
        None => None,
    };
    let Some(chat) = maybe_chat else { return Ok(()) };
    let chat_id_partiality: ChatIdPartiality = chat.try_into().map_err(|e: NoChatIdError| anyhow!(e))?;

    let enough = repos.dicks.check_dick(&chat_id_partiality.kind(), result.from.id, abs_amount).await?;
    if !enough {
        let lang_code = LanguageCode::from_user(&result.from);
        // a dedicated message, distinct from the generic "not enough" shown when the command
        // itself is typed: here the offer was already shown to the chat, then invalidated by
        // something that happened in between (e.g. the same offer accepted/spent elsewhere) -
        // worth calling out explicitly rather than leaving the player wondering why an offer
        // they could afford a moment ago suddenly says otherwise.
        let text = t!("commands.presta.errors.not_enough_anymore", locale = &lang_code).to_string();
        let inline_message_id = result.inline_message_id
            .ok_or("inline_message_id must be set if a chat was resolved from it")?;
        bot.edit_message_text_inline(inline_message_id, &text).await
            .inspect_err(|e| log::error!("couldn't edit an unaffordable p2p loan offer: {e}"))?;
    }
    Ok(())
}

#[inline]
pub fn callback_filter(query: CallbackQuery) -> bool {
    P2PLoanCallbackData::check_prefix(query)
}

pub async fn callback_handler(bot: Bot, query: CallbackQuery, repos: Repositories, config: AppConfig,
                              mut locker: LockCallbackServiceFacade, details_store: DetailsStore) -> HandlerResult {
    let chat_id = utils::resolve_callback_chat_id(&query, config.features.chats_merging);

    let callback_data = P2PLoanCallbackData::parse(&query)?;
    if callback_data.proposer == query.from.id {
        return send_error_callback_answer(bot, query, "commands.presta.errors.same_person").await;
    }
    if let NewLayoutValue::Some(target) = callback_data.target {
        if target != query.from.id {
            return send_error_callback_answer(bot, query, "commands.presta.errors.not_target").await;
        }
    }
    let _guard = match locker.try_lock(&callback_data) {
        Some(lock) => lock,
        None => return send_error_callback_answer(bot, query, "commands.presta.errors.already_in_progress").await
    };

    let interest_rate_pct = match callback_data.interest_rate {
        NewLayoutValue::Some(pct) => Some(pct),
        NewLayoutValue::None => None,
    };
    let params = P2PLoanParams {
        repos,
        chat_id: chat_id.clone(),
        lang_code: LanguageCode::from_user(&query.from),
        interest_rate: config.p2p_loan_interest_rate,
        tax_bottom_ranks: config.tax.bottom_ranks,
    };
    let result = p2p_loan_impl_accept(params, callback_data.proposer, query.from.clone().into(), callback_data.amount, interest_rate_pct, &details_store).await?;
    result.apply(bot, query).await?;

    metrics::CMD_P2P_LOAN_COUNTER.inline.inc();
    Ok(())
}

pub(crate) struct P2PLoanParams {
    pub(crate) repos: Repositories,
    pub(crate) chat_id: ChatIdPartiality,
    pub(crate) lang_code: LanguageCode,
    pub(crate) interest_rate: f32,
    pub(crate) tax_bottom_ranks: usize,
}

/// The configured (fractional) `interest_rate` as a clean percentage for display. Widening f32
/// to f64 and multiplying by 100 can otherwise surface the f32's own rounding noise (e.g.
/// 0.1f32 becomes 10.000000149011612% instead of 10%), so this rounds it away while still
/// keeping up to 4 decimals for genuinely fractional rates.
pub(super) fn rate_to_pct(interest_rate: f32) -> f64 {
    (interest_rate as f64 * 100.0 * 10000.0).round() / 10000.0
}

/// The free-form description of a p2p loan offer (no button), shared by the slash-command path,
/// the inline-query path, and combo offers - so all three read identically for the same
/// parameters. `interest` must already be `compute_interest(amount, rate)` - callers reject the
/// offer outright (an error message, never this function) when that's `None`.
pub(crate) fn p2p_loan_offer_text(name: &Username, target_name: Option<&Username>, amount: i32, rate_pct: f64, interest: i32, lang_code: &LanguageCode) -> String {
    let (abs_amount, is_pull) = split_amount(amount);
    let text_key = match (target_name.is_some(), is_pull) {
        (true, true) => "commands.presta.results.request_targeted",
        (false, true) => "commands.presta.results.request",
        (true, false) => "commands.presta.results.start_targeted",
        (false, false) => "commands.presta.results.start",
    };
    let debt_clause_text = debt_clause(lang_code, abs_amount, interest);
    match target_name {
        Some(target_name) => t!(text_key, locale = lang_code,
            name = name.escaped(), target_name = target_name.escaped(), amount = abs_amount, rate = rate_pct, debt_clause = &debt_clause_text).to_string(),
        None => t!(text_key, locale = lang_code, name = name.escaped(), amount = abs_amount, rate = rate_pct, debt_clause = &debt_clause_text).to_string(),
    }
}

pub(crate) async fn p2p_loan_impl_start(p: P2PLoanParams, proposer: UserInfo, amount: i32, target: Option<UserInfo>, interest_rate_pct: Option<f64>) -> anyhow::Result<(String, Option<InlineKeyboardMarkup>)> {
    let (abs_amount, is_pull) = split_amount(amount);
    // lending requires the proposer to have enough right now; requesting to borrow doesn't,
    // since the proposer isn't the one lending - whoever accepts is checked then.
    let enough = if is_pull {
        true
    } else {
        p.repos.dicks.check_dick(&p.chat_id.kind(), proposer.uid, abs_amount).await?
    };
    log::debug!("Offering a p2p loan from {} in the chat with id = {} (amount = {amount}, rate = {interest_rate_pct:?}, enough = {enough})...", proposer.uid, p.chat_id);

    let rate_pct = interest_rate_pct.unwrap_or(rate_to_pct(p.interest_rate));
    let data = if !enough {
        (t!("commands.presta.errors.not_enough", locale = &p.lang_code).to_string(), None)
    } else {
        match compute_interest(abs_amount, (rate_pct / 100.0) as f32) {
            None => (t!("commands.presta.errors.rate_too_high", locale = &p.lang_code, rate = rate_pct, amount = abs_amount).to_string(), None),
            Some(interest) => {
                let text = p2p_loan_offer_text(&proposer.name, target.as_ref().map(|t| &t.name), amount, rate_pct, interest, &p.lang_code);
                let btn_label_key = if is_pull { "commands.presta.button_pull" } else { "commands.presta.button" };
                let btn_label = t!(btn_label_key, locale = &p.lang_code);
                let target_uid = target.map(|t| t.uid);
                let btn_data = P2PLoanCallbackData::new(proposer.uid, amount, target_uid, interest_rate_pct).to_data_string();
                let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
                let keyboard = offer_keyboard(accept_btn, proposer.uid, target_uid, &p.lang_code);
                (text, Some(keyboard))
            }
        }
    };
    Ok(data)
}

/// What `p2p_loan_core_in_tx` actually did, carried forward to `p2p_loan_finish` so it doesn't
/// have to re-derive the lender/borrower direction or re-read the lengths it already has in hand.
pub(crate) struct P2PLoanCoreResult {
    lender_id: UserId,
    borrower_id: UserId,
    abs_amount: u16,
    rate_pct: f64,
    interest: i32,
    tax: u16,
    interest_loan_id: i32,
    lender_len: i32,
    borrower_len: i32,
}

pub(crate) enum P2PLoanAffordability {
    Ok(P2PLoanCoreResult),
    NotEnough,
    RateTooHigh { rate_pct: f64, abs_amount: u16 },
}

/// The core of accepting a p2p loan - resolving the pull/push direction, validating the rate,
/// and (if the lender can currently afford it) transferring the principal and creating the loan
/// row(s) - against an externally owned `tx` that this never commits, exactly like
/// `Dicks::move_length_in_tx`. The rate is checked *before* the affordability check, mirroring
/// the original match's priority (`(enough_lender, interest_check)`: lender's own affordability
/// always wins the report if both are wrong).
pub(crate) async fn p2p_loan_core_in_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, repos: &Repositories, chat_id_kind: &ChatIdKind, internal_chat_id: i64, proposer_id: UserId, acceptor_id: UserId, amount: i32, interest_rate_pct: Option<f64>, default_rate: f32) -> anyhow::Result<P2PLoanAffordability> {
    let (abs_amount, is_pull) = split_amount(amount);
    // a "pull" means the proposer was requesting to borrow: accepting swaps the roles, so the
    // acceptor becomes the lender and the proposer becomes the borrower.
    let (lender_id, borrower_id) = if is_pull { (acceptor_id, proposer_id) } else { (proposer_id, acceptor_id) };

    let enough_lender = Dicks::check_dick_with(&mut **tx, chat_id_kind, lender_id, abs_amount).await?;
    let rate_pct = interest_rate_pct.unwrap_or(rate_to_pct(default_rate));
    let custom_rate = interest_rate_pct.map(|pct| (pct / 100.0) as f32);
    let interest_and_tax = repos.p2p_loans.interest_and_tax(abs_amount, custom_rate);

    log::debug!("Executing a p2p loan: lender = {lender_id} (enough = {enough_lender}), borrower = {borrower_id}, amount = {abs_amount}, rate = {interest_rate_pct:?}...");

    if !enough_lender {
        return Ok(P2PLoanAffordability::NotEnough);
    }
    let Some((interest, tax)) = interest_and_tax else {
        return Ok(P2PLoanAffordability::RateTooHigh { rate_pct, abs_amount });
    };

    let (lender_len, borrower_len, interest_loan_id) =
        repos.p2p_loans.lend_in_tx(tx, internal_chat_id, lender_id, borrower_id, abs_amount, interest).await?;

    Ok(P2PLoanAffordability::Ok(P2PLoanCoreResult { lender_id, borrower_id, abs_amount, rate_pct, interest, tax, interest_loan_id, lender_len, borrower_len }))
}

/// Everything after the transfer itself is applied and committed: ledger, the gradual tax-debt
/// obligation (if any), and building the result text - exactly as today for a standalone loan -
/// so none of it needs to share the transaction `p2p_loan_core_in_tx` used.
pub(crate) async fn p2p_loan_finish(p: &P2PLoanParams, acceptor: &UserInfo, internal_chat_id: i64, core: P2PLoanCoreResult) -> anyhow::Result<(String, Option<String>)> {
    let lender_res = p.repos.dicks.growth_result_after(internal_chat_id, core.lender_id, core.lender_len).await?;
    let borrower_res = p.repos.dicks.growth_result_after(internal_chat_id, core.borrower_id, core.borrower_len).await?;

    let principal_entries = [(core.lender_id, -(core.abs_amount as i32), Some(core.borrower_id)), (core.borrower_id, core.abs_amount as i32, Some(core.lender_id))];
    if let Err(e) = p.repos.ledger.record_many(&p.chat_id, repo::LedgerCategory::LoanPrincipal, &principal_entries).await {
        log::error!("couldn't record ledger entries for a p2p loan principal transfer ({} -> {}, {}): {e}", core.lender_id, core.borrower_id, core.abs_amount);
    }
    // `interest`'s sign says who realizes it: positive means the lender does (the usual
    // case, taxed on them right away); negative means the borrower does instead (a
    // negative-rate loan, where the lender committed to paying them back) - see
    // `repo::P2PLoans::lend`. Rather than withholding the tax immediately (which used to
    // round small loans' tax down to nothing, see the removed `apply_interest_tax`), it
    // becomes its own gradual tax-debt obligation - see `repo::LoanInterestTaxDebts`.
    let tax_applied = core.tax > 0 && p.tax_bottom_ranks > 0 && core.interest != 0;
    if tax_applied {
        let payer_id = if core.interest > 0 { core.lender_id } else { core.borrower_id };
        p.repos.loan_interest_tax_debts.create(&p.chat_id, payer_id, core.tax, Some(core.interest_loan_id)).await?;
    }

    // if these two already owed each other, settle the overlap on the spot so debts stay net, not
    // gross (see `repo::P2PLoans::net_mutual_debts`). Best-effort: a failure leaves the loans
    // merely un-netted (gross), never blocks the loan that already committed.
    if let Err(e) = p.repos.p2p_loans.net_mutual_debts(internal_chat_id, core.lender_id, core.borrower_id).await {
        log::error!("couldn't net mutual debts between {} and {} after a p2p loan: {e}", core.lender_id, core.borrower_id);
    }

    let lender_info = get_user_info(&p.repos.users, core.lender_id, acceptor).await?;
    let borrower_info = get_user_info(&p.repos.users, core.borrower_id, acceptor).await?;
    let debt_clause = debt_clause(&p.lang_code, core.abs_amount, core.interest);
    let main_part = t!("commands.presta.results.finish", locale = &p.lang_code,
        lender_name = lender_info.name.escaped(), borrower_name = borrower_info.name.escaped(), amount = core.abs_amount, rate = core.rate_pct,
        lender_length = lender_res.new_length, borrower_length = borrower_res.new_length, debt_clause = &debt_clause);
    // the loan's own debt clause and any interest-tax obligation it creates stay visible by
    // default; the leaderboard positions are returned separately so the caller can defer them
    // behind a Dettagli button (see `details::maybe_deferred`) - standalone callers defer
    // per-loan, while a combo leg (see `crate::handlers::combo::callback_handler`) folds them
    // into one combined details blob covering both legs.
    let tax_part = if tax_applied {
        let tax_clause_key = if core.interest > 0 {
            "commands.presta.results.tax_debt_created.lender"
        } else {
            "commands.presta.results.tax_debt_created.borrower"
        };
        format!("\n\n{}", t!(tax_clause_key, locale = &p.lang_code, tax = core.tax))
    } else {
        String::default()
    };
    let short_text = format!("{main_part}{tax_part}");
    let details = if let (Some(lender_pos), Some(borrower_pos)) = (lender_res.pos_in_top, borrower_res.pos_in_top) {
        let lender_pos = t!("commands.presta.results.position.lender", locale = &p.lang_code, name = lender_info.name.escaped(), pos = lender_pos);
        let borrower_pos = t!("commands.presta.results.position.borrower", locale = &p.lang_code, name = borrower_info.name.escaped(), pos = borrower_pos);
        Some(format!("{lender_pos}\n{borrower_pos}"))
    } else {
        None
    };
    Ok((short_text, details))
}

pub(crate) async fn p2p_loan_impl_accept(p: P2PLoanParams, proposer_id: UserId, acceptor: UserInfo, amount: i32, interest_rate_pct: Option<f64>,
                                         details_store: &DetailsStore) -> anyhow::Result<CallbackResult> {
    let chat_id_kind = p.chat_id.kind();
    let internal_chat_id = p.repos.dicks.resolve_chat(&p.chat_id).await?;
    let mut tx = p.repos.dicks.begin_tx().await?;
    let affordability = p2p_loan_core_in_tx(&mut tx, &p.repos, &chat_id_kind, internal_chat_id, proposer_id, acceptor.uid, amount, interest_rate_pct, p.interest_rate).await?;
    let result = match affordability {
        P2PLoanAffordability::NotEnough => CallbackResult::ShowError(t!("commands.presta.errors.not_enough", locale = &p.lang_code).to_string()),
        P2PLoanAffordability::RateTooHigh { rate_pct, abs_amount } => {
            CallbackResult::ShowError(t!("commands.presta.errors.rate_too_high", locale = &p.lang_code, rate = rate_pct, amount = abs_amount).to_string())
        },
        P2PLoanAffordability::Ok(core) => {
            tx.commit().await?;
            let (short_text, details) = p2p_loan_finish(&p, &acceptor, internal_chat_id, core).await?;
            let (text, keyboard) = details::maybe_deferred(short_text, details, None, Some(details_store), &p.lang_code);
            CallbackResult::EditMessage(text, keyboard)
        }
    };
    Ok(result)
}

/// How much of a loan/tax debt has been repaid so far, as a whole-number percentage (e.g. `13`
/// for "ripagaa al 13%"). Replaces the old `▓▓▓░░░` progress bar, which only added visual noise.
/// `original` is assumed > 0 (always true here - see the migration that added
/// `original_principal`/`original_interest`/`original_debt`: a loan/tax debt is never created with
/// nothing owed).
fn repaid_pct(remaining: i32, original: i32) -> i32 {
    let paid_ratio = (1.0 - remaining as f64 / original as f64).clamp(0.0, 1.0);
    (paid_ratio * 100.0).round() as i32
}

/// Groups a counterparty-name-sorted slice of loans into per-counterparty runs (preserving order),
/// keyed by uid so two distinct people who happen to share a display name never silently merge.
fn group_by_counterparty(loans: &[repo::P2PLoanStatus]) -> Vec<Vec<&repo::P2PLoanStatus>> {
    let mut groups: Vec<Vec<&repo::P2PLoanStatus>> = Vec::new();
    for loan in loans {
        match groups.last_mut() {
            Some(group) if group[0].counterparty_uid == loan.counterparty_uid => group.push(loan),
            _ => groups.push(vec![loan]),
        }
    }
    groups
}

/// One `/debiti` block for all the loans between `from` and a single counterparty: a header naming
/// that person and the grand total still owed to/by them, then one indented line per individual
/// loan (remaining/original, repaid %, and when it was taken on), each optionally followed by a
/// further-indented capital/interest breakdown - skipped for a pure reciprocal-discount row with
/// no principal of its own (see `repo::P2PLoanObligation`'s docs). `side` is `as_borrower` or
/// `as_lender`, selecting the header wording.
fn format_loan_group(lang_code: &LanguageCode, side: &str, group: Vec<&repo::P2PLoanStatus>) -> String {
    let name = Username::new(group[0].counterparty_name.clone()).escaped();
    let total: i32 = group.iter().map(|l| l.debt).sum();
    let header = t!(&format!("commands.debiti.{side}.group_header"), locale = lang_code, name = name, total = total).to_string();

    let mut out = header;
    for l in group {
        let original_debt = l.original_principal + l.original_interest;
        let pct = repaid_pct(l.debt, original_debt);
        let datetime = l.created_at.with_timezone(&chrono_tz::Europe::Rome).format("%d/%m %H:%M").to_string();
        out.push('\n');
        out.push_str(&t!("commands.debiti.loan_line", locale = lang_code,
            debt = l.debt, original_debt = original_debt, pct = pct, datetime = datetime).to_string());
        if l.original_principal > 0 {
            let rate_pct = (l.original_interest as f64 / l.original_principal as f64 * 100.0).round() as i64;
            out.push('\n');
            out.push_str(&t!("commands.debiti.breakdown_line", locale = lang_code,
                principal = l.original_principal, rate = rate_pct, interest = l.original_interest).to_string());
        }
    }
    out
}

/// The "who owes what" clause for a loan offer/result message: the borrower always owes the
/// full `principal` back, plus any positive `interest` on top - the usual case. When `interest`
/// is negative, an extra sentence is appended naming the *lender's* separate, reciprocal
/// obligation to pay the borrower `interest`'s magnitude back too (see `repo::P2PLoans::lend`):
/// unlike the positive case, these two obligations are never mutually exclusive, so both clauses
/// can appear together.
fn debt_clause(lang_code: &LanguageCode, principal: u16, interest: i32) -> String {
    let borrower_owes = principal as i32 + interest.max(0);
    let positive_clause = t!("commands.presta.results.debt_clause.positive", locale = lang_code, debt = borrower_owes).to_string();
    if interest >= 0 {
        positive_clause
    } else {
        let negative_clause = t!("commands.presta.results.debt_clause.negative", locale = lang_code, debt = interest.unsigned_abs()).to_string();
        format!("{positive_clause} {negative_clause}")
    }
}

pub(super) fn build_inline_keyboard_article_result(uid: UserId, lang_code: &LanguageCode, name: &Username, amount: i32, rate_pct: f64, interest: i32, interest_rate_pct: Option<f64>, target: Option<&repo::User>) -> InlineQueryResult {
    let (abs_amount, is_pull) = split_amount(amount);
    log::debug!("Offering a p2p loan from {uid} (amount = {amount}, rate = {rate_pct}, interest = {interest}, target = {target:?})...");

    let title_key = if is_pull { "inline.results.titles.presta_pull" } else { "inline.results.titles.presta" };
    let title = t!(title_key, locale = lang_code, amount = abs_amount);
    let text = p2p_loan_offer_text(name, target.map(|t| &t.name), amount, rate_pct, interest, lang_code);
    let content = InputMessageContent::Text(InputMessageContentText::new(text).parse_mode(ParseMode::Html));
    let btn_label_key = if is_pull { "commands.presta.button_pull" } else { "commands.presta.button" };
    let btn_label = t!(btn_label_key, locale = lang_code);
    let target_uid = target.map(|t| UserId(t.uid as u64));
    let btn_data = P2PLoanCallbackData::new(uid, amount, target_uid, interest_rate_pct).to_data_string();
    let accept_btn = InlineKeyboardButton::callback(btn_label, btn_data);
    InlineQueryResultArticle::new("p2ploan", title, content)
        .reply_markup(offer_keyboard(accept_btn, uid, target_uid, lang_code))
        .into()
}

#[cfg(test)]
mod test_repaid_pct {
    use super::repaid_pct;

    #[test]
    fn an_untouched_debt_is_zero_percent() {
        assert_eq!(repaid_pct(100, 100), 0);
    }

    #[test]
    fn a_fully_repaid_debt_is_a_hundred_percent() {
        assert_eq!(repaid_pct(0, 100), 100);
    }

    #[test]
    fn a_partial_repayment_rounds_to_the_nearest_percent() {
        // 41 repaid out of 100 -> 41%.
        assert_eq!(repaid_pct(59, 100), 41);
    }
}

#[cfg(test)]
mod test_format_loan_group {
    use crate::domain::LanguageCode;
    use crate::repo::P2PLoanStatus;
    use super::{format_loan_group, group_by_counterparty};

    fn lmo() -> LanguageCode {
        LanguageCode::new("lmo".to_owned())
    }

    fn loan(counterparty_uid: i64, name: &str, debt: i32, original_principal: i32, original_interest: i32) -> P2PLoanStatus {
        P2PLoanStatus {
            counterparty_uid, counterparty_name: name.to_owned(), debt, payout_ratio: 0.1,
            original_principal, original_interest, created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_normal_loan_gets_a_breakdown_line_with_the_effective_rate() {
        let loans = vec![loan(1, "Mario", 55, 100, 10)];
        let block = format_loan_group(&lmo(), "as_borrower", group_by_counterparty(&loans).remove(0));
        assert!(block.contains("100"), "the original principal must appear: {block}");
        assert!(block.contains("10%"), "the effective rate (10/100 = 10%) must appear: {block}");
        assert!(block.contains("55"), "the remaining debt must appear: {block}");
    }

    #[test]
    fn a_pure_reciprocal_row_has_no_breakdown_line() {
        // a negative-rate loan's reciprocal row (see `repo::P2PLoanObligation`'s docs) has no
        // principal of its own - showing a "0 capital + X% rate" breakdown would be meaningless.
        let loans = vec![loan(1, "Mario", 12, 0, 35)];
        let block = format_loan_group(&lmo(), "as_lender", group_by_counterparty(&loans).remove(0));
        // just the header line and the single loan line - no further breakdown line.
        assert_eq!(block.lines().count(), 2, "a pure reciprocal row must have only header + loan line: {block}");
    }

    #[test]
    fn loans_to_the_same_person_share_one_header_with_the_grand_total() {
        let loans = vec![loan(1, "Federico", 46, 50, 0), loan(1, "Federico", 42, 47, 0)];
        let groups = group_by_counterparty(&loans);
        assert_eq!(groups.len(), 1, "two loans to the same uid must form a single group");
        let block = format_loan_group(&lmo(), "as_borrower", groups.into_iter().next().unwrap());
        assert_eq!(block.matches("Federico").count(), 1, "the name must appear only once, in the header: {block}");
        assert!(block.contains("88"), "the header must show the grand total 46+42=88: {block}");
    }

    #[test]
    fn distinct_people_are_not_merged() {
        let loans = vec![loan(1, "Federico", 10, 10, 0), loan(2, "LeoPoldo", 5, 5, 0)];
        assert_eq!(group_by_counterparty(&loans).len(), 2);
    }
}

#[cfg(test)]
mod test_rate_to_pct {
    use super::rate_to_pct;

    #[test]
    fn widening_f32_to_f64_does_not_leak_float_noise() {
        // 0.1f32 isn't exactly representable; naively doing `0.1f32 as f64 * 100.0` surfaces
        // its rounding error as 10.000000149011612 instead of a clean 10.0.
        assert_eq!(rate_to_pct(0.1), 10.0);
        assert_eq!(rate_to_pct(0.26), 26.0);
        assert_eq!(rate_to_pct(0.4), 40.0);
    }

    #[test]
    fn fractional_rates_keep_up_to_four_decimals() {
        assert_eq!(rate_to_pct(0.335), 33.5);
        assert_eq!(rate_to_pct(0.333333), 33.3333);
    }
}

#[cfg(test)]
mod test_status_totals {
    use teloxide::types::{User, UserId};
    use crate::config::AppConfig;
    use crate::handlers::p2p_loan::p2p_loan_status_impl;
    use crate::handlers::FromRefs;
    use crate::handlers::utils::page::Page;
    use crate::repo;
    use crate::repo::test::dicks::{create_another_user_and_dick, create_dick, create_user};
    use crate::repo::test::{get_chat_id_and_dicks, start_postgres, UID};

    fn test_user(id: i64) -> User {
        User {
            id: UserId(id as u64), is_bot: false, first_name: "test".to_owned(), last_name: None,
            username: None, language_code: None, is_premium: false, added_to_attachment_menu: false,
        }
    }

    /// `/debiti` must fold a tax debt (no single counterparty - see `repo::LoanInterestTaxDebts`)
    /// into the same totals as ordinary P2P loans: it only ever counts against the user, never
    /// in their favor, since there's no symmetric "lender" side to one.
    #[tokio::test]
    async fn test_totals_include_a_tax_debt_with_no_counterparty() {
        let (_container, db) = start_postgres().await;
        let (chat_id, _dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

        create_user(&db).await; // UID
        create_dick(&db).await;
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "other", 0).await;
        let other_uid = UserId((UID + 1) as u64);

        let cfg = AppConfig { p2p_loan_payout_ratio: 0.1, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);
        // UID lends 100 to other at 0% - UID is owed 100 as a lender.
        repos.p2p_loans.lend(&chat_id_partiality, UserId(UID as u64), other_uid, 100, Some(0.0)).await
            .expect("couldn't create the p2p loan");
        // UID separately owes a 7-ghei tax debt.
        repos.loan_interest_tax_debts.create(&chat_id_partiality, UserId(UID as u64), 7, None).await
            .expect("couldn't create the tax debt");

        let user = test_user(UID);
        let status = p2p_loan_status_impl(&repos, FromRefs(&user, &chat_id_partiality), Page::first()).await
            .expect("couldn't build the /debiti status");
        let text = status.lines;

        // owed by me: just the 7-ghei tax debt (no P2P borrower-side debt); owed to me: the
        // 100-ghei p2p loan as lender; net: 100 - 7 = 93, in UID's favor.
        assert!(!status.has_more_pages, "two entries must fit on a single page");
        assert!(text.contains('7'), "the tax debt amount must appear somewhere in the message: {text}");
        assert!(text.contains("93"), "the net (100 owed to me - 7 owed by me = 93) must appear in the message: {text}");
    }
}
