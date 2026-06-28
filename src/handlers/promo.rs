use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use once_cell::sync::Lazy;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::macros::BotCommands;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::prelude::{Dialogue, InlineQuery, Requester};
use teloxide::types::{InlineQueryResultsButton, InlineQueryResultsButtonKind, Message, User};
use crate::handlers::{HandlerResult, reply_html};
use crate::handlers::debt_settlement::settle_gain_against_debts;
use crate::{metrics, reply_html, repo};
use crate::config::AppConfig;
use crate::domain::LanguageCode;
use crate::repo::ActivationError;

pub(crate) const PROMO_START_PARAM_PREFIX: &str = "promo-";

static PROMO_CODE_FORMAT_REGEXP: Lazy<regex::Regex> = Lazy::new(||
    regex::Regex::new("^[a-zA-Z0-9_\\-]{4,16}$")
        .expect("promo code format regular expression must be valid")
);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum PromoCommands {
    #[command(description = "promo")]
    Promo(String),
}

#[derive(Clone, Default)]
pub enum PromoCommandState {
    #[default]
    Start,
    Requested,
}

pub type PromoCodeDialogue = Dialogue<PromoCommandState, InMemStorage<PromoCommandState>>;

pub async fn promo_cmd_handler(bot: Bot, msg: Message, cmd: PromoCommands, dialogue: PromoCodeDialogue,
                               repos: repo::Repositories, config: AppConfig) -> HandlerResult {
    metrics::CMD_PROMO.invoked_by_command.inc();
    let user = msg.from.as_ref().ok_or("no from user")?;
    let answer = match cmd {
        PromoCommands::Promo(code) if code.is_empty() => {
            dialogue.update(PromoCommandState::Requested).await?;

            let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
            t!("commands.promo.request", locale = &lang_code).to_string()
        }
        PromoCommands::Promo(code) => {
            dialogue.exit().await?;

            promo_activation_impl(&repos, &config, user, &code).await?
        },
    };
    reply_html!(bot, msg, answer);
    Ok(())
}

pub async fn promo_requested_handler(bot: Bot, msg: Message, dialogue: PromoCodeDialogue,
                                     repos: repo::Repositories, config: AppConfig) -> HandlerResult {
    let answer = match msg.text() {
        Some(code) => {
            dialogue.exit().await?;

            let user = msg.from.as_ref().ok_or("no from user")?;
            promo_activation_impl(&repos, &config, user, code).await?
        },
        None => {
            let lang_code = LanguageCode::from_maybe_user(msg.from.as_ref());
            t!("commands.promo.request", locale = &lang_code).to_string()
        }
    };
    reply_html!(bot, msg, answer);
    Ok(())
}

pub fn promo_inline_filter(InlineQuery { query, .. }: InlineQuery) -> bool {
    PROMO_CODE_FORMAT_REGEXP.is_match(&query)
}

pub async fn promo_inline_handler(bot: Bot, query: InlineQuery) -> HandlerResult {
    metrics::INLINE_COUNTER.invoked();

    let lang_code = LanguageCode::from_user(&query.from);
    let promo_code = query.query;
    let button_text = t!("commands.promo.inline.switch_button", locale = &lang_code, code = promo_code);
    let encoded_query = URL_SAFE_NO_PAD.encode(promo_code.as_bytes());
    let deeplink_start_param = format!("{}{}", PROMO_START_PARAM_PREFIX, encoded_query);
    let button = InlineQueryResultsButton {
        text: button_text.to_string(),
        kind: InlineQueryResultsButtonKind::StartParameter(deeplink_start_param)
    };
    let mut answer = bot.answer_inline_query(query.id, Vec::default())
        .is_personal(true)
        .button(button);
    if cfg!(debug_assertions) {
        answer.cache_time.replace(1);
    }
    answer.await?;
    Ok(())
}

pub(crate) async fn promo_activation_impl(repos: &repo::Repositories, config: &AppConfig, user: &User, promo_code: &str) -> anyhow::Result<String> {
    let lang_code = LanguageCode::from_user(user);
    let answer = match repos.promo.activate(user.id, promo_code).await {
        Ok(res) => {
            metrics::CMD_PROMO.finished.inc();

            // unlike every other gain, a promo bonus lands in ALL of the user's chats at once,
            // but debt settlement is inherently per-chat (see
            // `crate::handlers::debt_settlement::settle_gain_against_debts`) - so each affected
            // chat is settled independently here. The displayed bonus always stays the gross
            // value below, even though some chats may silently withhold part of it for debts -
            // see the project's plan notes for why a per-chat breakdown isn't shown.
            for chat_internal_id in &res.affected_chat_internal_ids {
                let Some(chat) = repos.chats.get_chat_by_internal_id(*chat_internal_id).await? else {
                    log::error!("couldn't resolve chat #{chat_internal_id} after a promo activation for {}", user.id);
                    continue
                };
                let chat_id_partiality: repo::ChatIdPartiality = match chat.try_into() {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("couldn't resolve a usable chat id for chat #{chat_internal_id} after a promo activation for {}: {e}", user.id);
                        continue
                    }
                };
                let chat_id_kind = chat_id_partiality.kind();
                if let Err(e) = repos.ledger.record_for_chat_kind(&chat_id_kind, user.id, repo::LedgerCategory::Grow, res.bonus_length, None).await {
                    log::error!("couldn't record a ledger entry for a promo bonus ({}) in chat #{chat_internal_id}: {e}", user.id);
                }
                let settlement = settle_gain_against_debts(repos, user.id, &chat_id_kind, res.bonus_length, config.tax.bottom_ranks, true).await
                    .inspect_err(|e| log::error!("couldn't settle {}'s debts from a promo bonus in chat #{chat_internal_id}: {e}", user.id))
                    .unwrap_or_default();
                if settlement.total_withheld > 0 {
                    if let Err(e) = repos.dicks.grow_no_attempts_check(&chat_id_kind, user.id, -settlement.total_withheld).await {
                        log::error!("couldn't claw back {}'s own debt settlement after a promo bonus in chat #{chat_internal_id}: {e}", user.id);
                    }
                }
            }

            let suffix = if res.chats_affected > 1 {
                "plural"
            } else {
                "singular"
            };
            t!("commands.promo.success.template", locale = &lang_code,
                ending = t!(&format!("commands.promo.success.{suffix}"), locale = &lang_code,
                    growth = res.bonus_length, affected_chats = res.chats_affected))
                .to_string()
        },
        Err(e) => {
            let suffix = match e {
                ActivationError::Other(e) => Err(e)?,
                e => format!("{e}")
            };
            let t_key = format!("commands.promo.errors.{suffix}");
            t!(&t_key, locale = &lang_code).to_string()
        }
    };
    Ok(answer)
}

#[cfg(test)]
mod test {
    use crate::handlers::promo::PROMO_CODE_FORMAT_REGEXP;

    #[test]
    fn test_regex() {
        assert!(PROMO_CODE_FORMAT_REGEXP.is_match("TESTPROMO"));
        assert!(PROMO_CODE_FORMAT_REGEXP.is_match("test-11_1"));

        assert!(!PROMO_CODE_FORMAT_REGEXP.is_match("T34"));
        assert!(!PROMO_CODE_FORMAT_REGEXP.is_match("PROMO!"));
        assert!(!PROMO_CODE_FORMAT_REGEXP.is_match("VERYVERYLONGLONGPROMOCODE"));
    }
}

#[cfg(test)]
mod test_debt_settlement {
    use teloxide::types::{ChatId, User, UserId};
    use crate::config::AppConfig;
    use crate::handlers::promo::promo_activation_impl;
    use crate::repo;
    use crate::repo::test::dicks::create_user;
    use crate::repo::test::{start_postgres, UID};
    use crate::repo::{ChatIdKind, ChatIdPartiality, PromoCodeParams};

    fn test_user() -> User {
        User {
            id: UserId(UID as u64), is_bot: false, first_name: "test".to_owned(), last_name: None,
            username: None, language_code: None, is_premium: false, added_to_attachment_menu: false,
        }
    }

    /// A promo bonus lands in every chat the user has a dick in at once, but debt settlement is
    /// per-chat (bug #3's promo case): a chat where the user is in debt must withhold part of
    /// the bonus, while an unrelated chat where they aren't must hand over the full amount.
    #[tokio::test]
    async fn test_promo_bonus_settles_debts_only_in_the_chat_that_has_them() {
        let (_container, db) = start_postgres().await;
        let chat_a: ChatIdPartiality = ChatIdKind::ID(ChatId(67890)).into();
        let chat_b: ChatIdPartiality = ChatIdKind::ID(ChatId(67891)).into();

        create_user(&db).await;
        let cfg = AppConfig { loan_payout_ratio: 0.5, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);
        repos.dicks.create_or_grow(UserId(UID as u64), &chat_a, 0).await.expect("couldn't create the dick in chat a");
        repos.dicks.create_or_grow(UserId(UID as u64), &chat_b, 0).await.expect("couldn't create the dick in chat b");
        repos.loans.borrow(UserId(UID as u64), &chat_a.kind(), 5).await.expect("couldn't create the loan in chat a");

        repos.promo.create(PromoCodeParams { code: "test20".to_owned(), bonus_length: 20, capacity: 1 }).await
            .expect("couldn't create the promo code");

        let user = test_user();
        promo_activation_impl(&repos, &cfg, &user, "test20").await.expect("couldn't activate the promo code");

        // chat a: the loan's own disbursement (+5) then the bonus (+20) then 50% of the bonus
        // withheld, capped at the 5-ghei debt -> 5 + 20 - 5 = 20.
        let length_a = repos.dicks.fetch_length(UserId(UID as u64), &chat_a.kind()).await.expect("couldn't fetch chat a's length");
        assert_eq!(length_a, 20);
        let loan_a = repos.loans.get_active_loan(UserId(UID as u64), &chat_a.kind()).await.expect("couldn't fetch chat a's loan");
        assert!(loan_a.is_none(), "chat a's 5-ghei loan must be fully repaid (and thus closed) by the promo bonus");

        // chat b: no debt at all, so the full bonus stays - 0 + 20 = 20.
        let length_b = repos.dicks.fetch_length(UserId(UID as u64), &chat_b.kind()).await.expect("couldn't fetch chat b's length");
        assert_eq!(length_b, 20);
    }
}
