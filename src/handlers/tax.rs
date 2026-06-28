use anyhow::anyhow;
use rust_i18n::t;
use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::types::{Message, UserId};
use crate::handlers::{FromRefs, HandlerResult, reply_html};
use crate::handlers::debt_settlement::{settle_gain_against_debts, SettlementOutcome};
use crate::{metrics, reply_html};
use crate::config::AppConfig;
use crate::domain::{LanguageCode, Username};
use crate::repo::{ChatIdPartiality, Dick, Repositories};

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum TaxCommands {
    #[command(description = "tax")]
    Tax,
}

pub async fn cmd_handler(bot: Bot, msg: Message, repos: Repositories, config: AppConfig) -> HandlerResult {
    metrics::CMD_TAX_COUNTER.chat.inc();

    let from = msg.from.as_ref().ok_or(anyhow!("no FROM field in the tax command handler"))?;
    let chat_id: ChatIdPartiality = msg.chat.id.into();
    let text = tax_impl(&repos, &config, FromRefs(from, &chat_id)).await?;

    reply_html!(bot, msg, text);
    Ok(())
}

pub(crate) async fn tax_impl(repos: &Repositories, config: &AppConfig, from_refs: FromRefs<'_>) -> anyhow::Result<String> {
    let (from, chat_id) = (from_refs.0, from_refs.1);
    let lang_code = LanguageCode::from_user(from);

    if !config.tax.is_enabled() {
        return Ok(t!("commands.tax.errors.disabled", locale = &lang_code).to_string())
    }

    let chat_id_kind = chat_id.kind();
    let top_n = config.tax.top_ranks;
    let bottom_n = config.tax.bottom_ranks;

    // top_n players to tax, +1 for the benchmark ("next best") player just below them,
    // bottom_n players to receive the redistributed pool, +1 for THEIR benchmark (the player
    // just above them) - all four groups disjoint.
    // `get_top`'s SQL binds this as `i32` under the hood, so the sentinel "fetch everyone" value
    // must stay within `i32::MAX`, not `u32::MAX` (which would wrap negative and break LIMIT).
    let players = repos.dicks.get_top(&chat_id_kind, 0, i32::MAX as u32).await?;
    let needed = top_n + bottom_n + 2;
    if players.len() < needed {
        return Ok(t!("commands.tax.errors.not_enough_players", locale = &lang_code,
            needed = needed as u32, present = players.len() as u32).to_string())
    }

    let top = &players[..top_n];
    let top_benchmark_length = players[top_n].length;
    let bottom = &players[players.len() - bottom_n..];
    let bottom_benchmark_length = players[players.len() - bottom_n - 1].length;

    // each taxed player's "distance" above the benchmark just below the taxed group;
    // normalized across the group, so a dominant outlier ends up paying close to the full
    // max_rate while someone close to the pack pays close to nothing.
    let distances: Vec<f64> = top.iter()
        .map(|p| (p.length - top_benchmark_length).max(0) as f64)
        .collect();
    let distance_sum: f64 = distances.iter().sum();

    let mut deltas: Vec<(UserId, i32)> = Vec::with_capacity(top_n + bottom_n);
    let mut paid_lines = Vec::with_capacity(top_n);
    let mut pool: i64 = 0;
    for (player, &distance) in top.iter().zip(distances.iter()) {
        let weight = if distance_sum > 0.0 { distance / distance_sum } else { 0.0 };
        let amount = ((player.length as f64) * config.tax.max_rate * weight).floor() as i32;
        let amount = amount.clamp(0, player.length.max(0));
        pool += amount as i64;
        deltas.push((player.owner_uid.into(), -amount));
        paid_lines.push(t!("commands.tax.results.paid_line", locale = &lang_code,
            name = Username::new(player.owner_name.clone()).escaped(), amount = amount).to_string());
    }

    // symmetric to the top, via the shared bottom-redistribution formula (also used to tax
    // p2p loan interest - see `redistribute_to_bottom`).
    let bottom_deltas = redistribute_to_bottom(bottom, bottom_benchmark_length, pool)
        .expect("bottom_n and the benchmark were already validated above");
    let mut received_lines = Vec::with_capacity(bottom_n);
    for (player, &(_, share)) in bottom.iter().zip(bottom_deltas.iter()) {
        received_lines.push(t!("commands.tax.results.received_line", locale = &lang_code,
            name = Username::new(player.owner_name.clone()).escaped(), amount = share).to_string());
    }
    deltas.extend(bottom_deltas.clone());

    let applied = repos.tax.tax_chat(chat_id, &deltas).await?;
    let text = if applied {
        // top_n payers and bottom_n recipients share one pool with no 1:1 relationship between a
        // specific payer and a specific recipient (unlike a single tax-debt installment's
        // redistribution - see `crate::handlers::debt_settlement::redistribute_tax_payout`), so
        // none of these rows get a counterparty.
        let ledger_deltas: Vec<_> = deltas.iter().map(|&(uid, amount)| (uid, amount, None)).collect();
        if let Err(e) = repos.ledger.record_many(chat_id, crate::repo::LedgerCategory::Tax, &ledger_deltas).await {
            log::error!("couldn't record ledger entries for a tax event in {chat_id}: {e}");
        }

        // a redistributed share is a gain just like any other, so each bottom-N recipient's own
        // debts must be settled from it too (see
        // `crate::handlers::debt_settlement::settle_gain_against_debts`) - otherwise an indebted
        // player could dodge automatic repayment just by being poor enough to receive a tax
        // share instead of growing themselves. Not atomic with `tax_chat` above (same tradeoff
        // PVP/donations/promo already accept) - aggregated into one combined note rather than
        // one per recipient, to keep the summary message readable.
        let mut withheld = SettlementOutcome::default();
        for &(uid, share) in &bottom_deltas {
            if share <= 0 {
                continue
            }
            let settlement = settle_gain_against_debts(repos, uid, &chat_id_kind, share, bottom_n, true).await
                .inspect_err(|e| log::error!("couldn't settle {uid}'s debts from a tax redistribution in {chat_id}: {e}"))
                .unwrap_or_default();
            if settlement.total_withheld > 0 {
                if let Err(e) = repos.dicks.grow_no_attempts_check(&chat_id_kind, uid, -settlement.total_withheld).await {
                    log::error!("couldn't claw back {uid}'s own debt settlement after a tax redistribution in {chat_id}: {e}");
                    continue
                }
                withheld.total_withheld += settlement.total_withheld;
                withheld.detail_lines.extend(settlement.detail_lines);
            }
        }

        t!("commands.tax.results.summary", locale = &lang_code,
            paid = paid_lines.join("\n"), received = received_lines.join("\n"), pool = pool).to_string()
            + &withheld.message(&lang_code)
    } else {
        t!("commands.tax.errors.already_done_today", locale = &lang_code).to_string()
    };
    Ok(text)
}

/// Splits `pool` among `bottom` proportionally to how far below `benchmark_length` (the player
/// just above this group) each of them is - the neediest gets the largest share, and players
/// equally needy (including a tie across the whole group) split evenly. `None` if `bottom` is
/// empty, since there's nobody to receive a share.
pub(crate) fn redistribute_to_bottom(bottom: &[Dick], benchmark_length: i32, pool: i64) -> Option<Vec<(UserId, i32)>> {
    if bottom.is_empty() {
        return None
    }
    let need: Vec<f64> = bottom.iter()
        .map(|p| (benchmark_length - p.length).max(0) as f64)
        .collect();
    let need_sum: f64 = need.iter().sum();

    let deltas = bottom.iter().zip(need.iter())
        .map(|(player, &need)| {
            let weight = if need_sum > 0.0 { need / need_sum } else { 1.0 / bottom.len() as f64 };
            let share = ((pool as f64) * weight).floor() as i32;
            (player.owner_uid.into(), share)
        })
        .collect();
    Some(deltas)
}

#[cfg(test)]
mod test {
    use teloxide::types::UserId;
    use crate::repo::Dick;
    use super::redistribute_to_bottom;

    fn dick(uid: i64, length: i32) -> Dick {
        Dick {
            length,
            owner_uid: uid.into(),
            owner_name: uid.to_string(),
            grown_at: chrono::Utc::now(),
            position: None,
            battles_total: 0,
            battles_won: 0,
        }
    }

    #[test]
    fn no_recipients_means_nothing_to_distribute() {
        assert_eq!(redistribute_to_bottom(&[], 10, 100), None);
    }

    #[test]
    fn neediest_gets_the_largest_share() {
        let bottom = [dick(1, 0), dick(2, 5), dick(3, 8)];
        let deltas = redistribute_to_bottom(&bottom, 10, 100).expect("there are recipients");
        assert_eq!(deltas, vec![
            (UserId(1), 58), // (10-0)/17 * 100
            (UserId(2), 29), // (10-5)/17 * 100
            (UserId(3), 11), // (10-8)/17 * 100
        ]);
    }

    #[test]
    fn a_tie_splits_evenly() {
        let bottom = [dick(1, 10), dick(2, 10), dick(3, 10)];
        let deltas = redistribute_to_bottom(&bottom, 10, 90).expect("there are recipients");
        assert_eq!(deltas, vec![(UserId(1), 30), (UserId(2), 30), (UserId(3), 30)]);
    }
}

#[cfg(test)]
mod test_debt_settlement {
    use teloxide::types::{User, UserId};
    use crate::config::{AppConfig, TaxConfig};
    use crate::handlers::tax::tax_impl;
    use crate::handlers::FromRefs;
    use crate::repo;
    use crate::repo::test::dicks::create_another_user_and_dick;
    use crate::repo::test::{get_chat_id_and_dicks, start_postgres, CHAT_ID_KIND, UID};

    fn test_user(id: i64) -> User {
        User {
            id: UserId(id as u64), is_bot: false, first_name: "test".to_owned(), last_name: None,
            username: None, language_code: None, is_premium: false, added_to_attachment_menu: false,
        }
    }

    /// A `/tax` redistribution share is a gain just like any other - a poor, indebted recipient
    /// must have part of their share withheld for their own bank loan, exactly as if they'd
    /// grown it themselves (this is the literal bug #3 fix: redistribution used to bypass debt
    /// settlement entirely).
    #[tokio::test]
    async fn test_tax_redistribution_settles_a_recipients_bank_loan() {
        let (_container, db) = start_postgres().await;
        let (chat_id, dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

        // P1 = 100 (taxed), P2 = 50 (top benchmark), P3 = 10 (bottom benchmark), P4 = 5 (poorest,
        // receives the redistributed pool, and has a small bank loan to be settled from it).
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "p1", 100).await;
        create_another_user_and_dick(&db, &chat_id_partiality, 3, "p2", 50).await;
        create_another_user_and_dick(&db, &chat_id_partiality, 4, "p3", 10).await;
        create_another_user_and_dick(&db, &chat_id_partiality, 5, "p4", 0).await;
        let p4_uid = UserId((UID + 4) as u64);

        let cfg = AppConfig {
            loan_payout_ratio: 0.5,
            tax: TaxConfig { top_ranks: 1, max_rate: 1.0, bottom_ranks: 1 },
            ..Default::default()
        };
        let repos = repo::Repositories::new(&db, &cfg);
        repos.loans.borrow(p4_uid, &CHAT_ID_KIND, 5).await.expect("couldn't create p4's loan");
        // p4 is now at 5 (0 + the loan's own disbursement) - still the poorest, so the top/bottom
        // split below (100/50/10/5) is unaffected.

        let user = test_user(UID);
        let text = tax_impl(&repos, &cfg, FromRefs(&user, &chat_id_partiality)).await
            .expect("couldn't run /tax");
        assert!(!text.to_lowercase().contains("error"), "unexpected error text: {text}");

        // pool: p1's distance above p2 is 50, fully taxed at max_rate=1.0 -> pool = 100.
        // redistribution: p4's need below p3 (10) is 5 (the only bottom player) -> gets the
        // whole pool of 100, landing at 5 + 100 = 105 before settlement.
        // settlement: 50% of the 100-ghei gain would be 50, but the loan only owes 5, so exactly
        // 5 is withheld and the loan is fully repaid - p4 ends at 105 - 5 = 100.
        let p4_length = dicks.fetch_length(p4_uid, &CHAT_ID_KIND).await.expect("couldn't fetch p4's length");
        assert_eq!(p4_length, 100);
        let p4_loan = repos.loans.get_active_loan(p4_uid, &CHAT_ID_KIND).await.expect("couldn't fetch p4's loan");
        assert!(p4_loan.is_none(), "p4's 5-ghei loan must be fully repaid (and thus closed) by the redistribution");
    }
}
