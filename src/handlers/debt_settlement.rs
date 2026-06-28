use anyhow::Context;
use futures::join;
use rust_i18n::t;
use teloxide::types::UserId;
use crate::domain::LanguageCode;
use crate::handlers::tax::redistribute_to_bottom;
use crate::repo::{ChatIdKind, LedgerCategory, Repositories};
use crate::repo;

/// What `settle_gain_against_debts` actually did with the gain: `total_withheld` is how much of
/// it went to obligations instead of staying with the player (the caller still has to apply
/// `gain - total_withheld`, or equivalently credit `gain` then debit `total_withheld`, to the
/// player's own length - see the function's docs for why this isn't done here). `detail_lines`
/// names every recipient who actually got a non-zero share - P2P creditors and tax-debt
/// redistribution recipients alike - so the caller can tell the player exactly where their
/// growth went, mirroring `Incrementor`'s own `detail_lines`. Each triple is (name, payout, the
/// remaining debt owed to that name afterwards) - the third element is `None` for a tax-debt
/// redistribution recipient, who isn't a creditor `uid` owes anything to specifically.
#[derive(Default)]
pub struct SettlementOutcome {
    pub total_withheld: i32,
    pub detail_lines: Vec<(String, u16, Option<u16>)>,
}

impl SettlementOutcome {
    /// The "X ghei went to your debts[, namely: ...]" message fragment, shared by every gain-
    /// producing flow (PVP, donations received, `/tax` redistribution, promo bonuses) so a
    /// player always sees where part of their gain went, regardless of which flow produced it.
    /// Empty string if nothing was withheld. Leads with a blank line so callers can just append
    /// it to their own message.
    pub fn message(&self, lang_code: &LanguageCode) -> String {
        format_withheld_message(self.total_withheld, &self.detail_lines, lang_code)
    }
}

/// Shared by [`SettlementOutcome::message`] (PVP, donations, `/tax`, promo bonuses - which credit
/// the gross gain and then debit back what's owed) and `Incrementor::perks_part_of_answer`'s
/// `debt-payout` perk (`/grow`, `/dod` - which only ever credit the net), so a player sees the
/// exact same "X ghei withheld[, to: ...]" wording no matter which flow withheld it - this is the
/// line meant to be impossible to miss, unlike a generic perk delta. `total_withheld` must be the
/// full amount withheld; `detail_lines` only the *named* subset of it (P2P creditors, tax-debt
/// redistribution recipients) - the remainder, if any, is attributed to a bank loan, which has no
/// single recipient to name.
pub fn format_withheld_message(total_withheld: i32, detail_lines: &[(String, u16, Option<u16>)], lang_code: &LanguageCode) -> String {
    if total_withheld <= 0 {
        return String::default()
    }
    let named_total: i32 = detail_lines.iter().map(|(_, payout, _)| i32::from(*payout)).sum();
    let unnamed = total_withheld - named_total;
    let unnamed_part = if unnamed > 0 {
        format!("\n\n{}", t!("titles.debt_withheld", locale = lang_code, payout = unnamed))
    } else {
        String::default()
    };
    let named_part = if detail_lines.is_empty() {
        String::default()
    } else {
        let lines = detail_lines.iter()
            .map(|(name, payout, remaining)| match remaining {
                // "remaining" lets the player tell at a glance how much of THIS specific debt is
                // still left after this payment, without having to check `/debiti` separately.
                Some(remaining) => t!("titles.recipient_line_with_remaining", locale = lang_code, name = name, payout = payout, remaining = remaining).to_string(),
                None => t!("titles.recipient_line", locale = lang_code, name = name, payout = payout).to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\n{}\n{lines}", t!("titles.debt_withheld_named", locale = lang_code, payout = named_total))
    };
    format!("{unnamed_part}{named_part}")
}

/// One of `uid`'s active obligations, regardless of underlying source - a bank loan, a P2P loan,
/// or a loan-interest tax debt (see `repo::Loans`, `repo::P2PLoans`, `repo::LoanInterestTaxDebts`
/// respectively) - unified purely in memory so `settle_gain_against_debts` can allocate a single
/// gain across all of a player's debts at once, oldest first, rather than letting each kind
/// independently claim its own share of the *same* gain (a latent bug the old, kind-specific
/// `LoanPayoutPerk`/`P2PLoanPayoutPerk` had: a player with both a bank loan and a P2P loan got
/// each one's `payout_ratio` withheld from the same base increment, effectively double-dipping).
enum Obligation {
    Bank(repo::Loan),
    P2P(repo::P2PLoanObligation),
    Tax(repo::TaxDebtObligation),
}

impl Obligation {
    fn amount_owed(&self) -> u16 {
        match self {
            Self::Bank(loan) => loan.debt,
            Self::P2P(o) => o.amount_owed,
            Self::Tax(o) => o.amount_owed,
        }
    }

    fn payout_ratio(&self) -> f32 {
        match self {
            Self::Bank(loan) => loan.payout_ratio,
            Self::P2P(o) => o.payout_ratio,
            Self::Tax(o) => o.payout_ratio,
        }
    }

    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            Self::Bank(loan) => loan.created_at,
            Self::P2P(o) => o.created_at,
            Self::Tax(o) => o.created_at,
        }
    }
}

/// Generalizes `P2PLoans`' old, kind-specific `allocate_loan_payouts` to all three obligation
/// kinds: for each obligation, in the order given (oldest first - see
/// `settle_gain_against_debts`), how much to withhold from `gain` this round - `min(round(gain *
/// payout_ratio), amount_owed, remaining pool)`. The pool starts at `gain` and shrinks as each
/// obligation takes its share in turn, so the total can never exceed `gain` itself, and the
/// oldest obligation always gets first claim on it when there isn't enough to satisfy every
/// obligation in full.
fn allocate_payouts(gain: i32, obligations: &[Obligation]) -> Vec<u16> {
    let gain_f = gain as f32;
    let mut remaining_pool = gain;
    obligations.iter()
        .map(|o| {
            if remaining_pool <= 0 {
                return 0
            }
            let desired = (gain_f * o.payout_ratio()).round() as i32;
            let payout = desired.min(o.amount_owed() as i32).min(remaining_pool).max(0) as u16;
            remaining_pool -= i32::from(payout);
            payout
        })
        .collect()
}

/// THE single place every gain-producing flow must settle `uid`'s active debts against a `gain`
/// of `chat_id`-local length before (or after) crediting it - bank loan, P2P loans, and
/// loan-interest tax debts, oldest obligation first across all three (see `Obligation`), sharing
/// one pool the way `P2PLoans::settle_from_award` used to do only for P2P loans alone. Does NOT
/// credit or debit `uid`'s own length for the net amount - only the withheld portion's
/// recipients - so it composes with both shapes already in use across the codebase: "compute the
/// net, credit only that" (the `Incrementor`/`Perk` path for `/grow` and `/dod` - see
/// `crate::handlers::perks::DebtPayoutPerk`) and "credit the gross, then debit back what's owed"
/// (PVP, donations received, `/tax` redistribution, promo code bonuses). Returns immediately
/// with an empty outcome for a non-positive `gain`.
///
/// `allow_redistribution_settlement` guards against an unbounded chain: a tax debt's installment
/// is redistributed to the chat's bottom-`bottom_n` players (recomputed dynamically right now,
/// like `/tax` itself - see `redistribute_to_bottom`), and - exactly once, when this flag is
/// `true` - each recipient's OWN debts are settled from their share too (so a tax-debt repayment
/// can't let its recipients dodge their own obligations, consistent with every other gain). That
/// inner settlement is always called with the flag `false`, so if it produces yet another
/// redistribution, that one's recipients are credited plainly - no second level of chasing.
/// Top-level callers should always pass `true`.
pub async fn settle_gain_against_debts(repos: &Repositories, uid: UserId, chat_id: &ChatIdKind, gain: i32, bottom_n: usize, allow_redistribution_settlement: bool) -> anyhow::Result<SettlementOutcome> {
    if gain <= 0 {
        return Ok(SettlementOutcome::default())
    }

    let (bank, p2p, tax) = join!(
        repos.loans.get_active_loan(uid, chat_id),
        repos.p2p_loans.get_active_loans(uid, chat_id),
        repos.loan_interest_tax_debts.get_active(uid, chat_id),
    );
    let mut obligations: Vec<Obligation> = Vec::new();
    if let Some(loan) = bank? {
        obligations.push(Obligation::Bank(loan));
    }
    obligations.extend(p2p?.into_iter().map(Obligation::P2P));
    obligations.extend(tax?.into_iter().map(Obligation::Tax));
    obligations.sort_by_key(Obligation::created_at);

    let payouts = allocate_payouts(gain, &obligations);
    let mut outcome = SettlementOutcome::default();
    for (obligation, &payout) in obligations.iter().zip(payouts.iter()) {
        if payout == 0 {
            continue
        }
        match obligation {
            Obligation::Bank(_) => {
                repos.loans.pay(uid, chat_id, payout).await
                    .context(format!("couldn't pay down {uid}'s bank loan in {chat_id}"))?;
                if let Err(e) = repos.ledger.record_for_chat_kind(chat_id, uid, LedgerCategory::LoanPrincipal, -i32::from(payout), None).await {
                    log::error!("couldn't record a loan principal ledger entry for a bank loan repayment ({uid}, {chat_id}): {e}");
                }
            }
            Obligation::P2P(o) => {
                let split = repos.p2p_loans.pay(o.id, o.remaining_principal, o.remaining_interest, payout).await
                    .context(format!("couldn't pay down p2p loan #{} for {uid} in {chat_id}", o.id))?;
                repos.dicks.grow_no_attempts_check(chat_id, o.creditor_uid, payout.into()).await
                    .context(format!("couldn't credit the creditor ({}) for a p2p loan repayment (#{})", o.creditor_uid, o.id))?;
                // mirrors `P2PLoans::lend`'s doc: the interest is deterministic at loan creation,
                // but only logged to the Ledger as it's actually collected, one round at a time.
                if let Err(e) = repos.ledger.record_for_chat_kind(chat_id, uid, LedgerCategory::LoanInterest, -split.interest, Some(o.creditor_uid)).await {
                    log::error!("couldn't record the payer's loan interest ledger entry for a p2p loan repayment ({chat_id}, #{}): {e}", o.id);
                }
                if let Err(e) = repos.ledger.record_for_chat_kind(chat_id, o.creditor_uid, LedgerCategory::LoanInterest, split.interest, Some(uid)).await {
                    log::error!("couldn't record the creditor's loan interest ledger entry for a p2p loan repayment ({chat_id}, #{}): {e}", o.id);
                }
                if let Err(e) = repos.ledger.record_for_chat_kind(chat_id, uid, LedgerCategory::LoanPrincipal, -split.principal, Some(o.creditor_uid)).await {
                    log::error!("couldn't record the payer's loan principal ledger entry for a p2p loan repayment ({chat_id}, #{}): {e}", o.id);
                }
                if let Err(e) = repos.ledger.record_for_chat_kind(chat_id, o.creditor_uid, LedgerCategory::LoanPrincipal, split.principal, Some(uid)).await {
                    log::error!("couldn't record the creditor's loan principal ledger entry for a p2p loan repayment ({chat_id}, #{}): {e}", o.id);
                }
                let creditor_name = repos.users.get(o.creditor_uid).await
                    .context(format!("couldn't resolve the creditor's ({}) name for a p2p loan repayment (#{})", o.creditor_uid, o.id))?
                    .map(|u| u.name.escaped())
                    .unwrap_or_else(|| o.creditor_uid.0.to_string());
                // `o.amount_owed` is this specific obligation's debt *before* this payout, so the
                // remainder afterwards is a plain subtraction - no extra query needed.
                let remaining = o.amount_owed - payout;
                outcome.detail_lines.push((creditor_name, payout, Some(remaining)));
            }
            Obligation::Tax(o) => {
                repos.loan_interest_tax_debts.pay(o.id, payout).await
                    .context(format!("couldn't pay down tax debt #{} for {uid} in {chat_id}", o.id))?;
                let recipients = redistribute_tax_payout(repos, chat_id, uid, payout, bottom_n).await?;
                for (recipient_uid, name, share) in &recipients {
                    // unlike the P2P case above, a tax-debt redistribution recipient isn't a
                    // creditor `uid` owes anything to specifically - there's no single "debt to
                    // this name" to report a remainder for.
                    outcome.detail_lines.push((name.clone(), *share, None));
                    if allow_redistribution_settlement && *share > 0 {
                        // boxed since this is a recursive async call (capped at one level deep
                        // by the `false` below - see the function's own docs) - the future's
                        // type can't otherwise contain itself unboxed.
                        let nested = Box::pin(settle_gain_against_debts(repos, *recipient_uid, chat_id, *share as i32, bottom_n, false)).await?;
                        if nested.total_withheld > 0 {
                            repos.dicks.grow_no_attempts_check(chat_id, *recipient_uid, -nested.total_withheld).await
                                .context(format!("couldn't claw back {recipient_uid}'s own debt settlement after a tax-debt redistribution"))?;
                        }
                    }
                }
            }
        }
        outcome.total_withheld += i32::from(payout);
    }
    Ok(outcome)
}

/// Redistributes a just-collected tax-debt installment of `payout` ghei to the chat's bottom
/// `bottom_n` players, using the exact same formula as `/tax` (`redistribute_to_bottom`) - the
/// neediest gets the largest share. Records the payer's debit and every recipient's credit as
/// `Tax`-category Ledger entries (the same category `/tax` itself uses - this is, structurally,
/// just a smaller and more frequent `/tax` run). Returns each recipient who actually got a
/// non-zero share, for both display (the caller's `detail_lines`) and the optional nested
/// settlement in `settle_gain_against_debts`. A failure to find enough players is logged and
/// otherwise ignored - the loan repayment itself must still go through either way.
async fn redistribute_tax_payout(repos: &Repositories, chat_id: &ChatIdKind, payer_uid: UserId, payout: u16, bottom_n: usize) -> anyhow::Result<Vec<(UserId, String, u16)>> {
    if bottom_n == 0 {
        return Ok(Vec::new())
    }
    // `get_top`'s SQL binds this as `i32` under the hood, so the sentinel "fetch everyone" value
    // must stay within `i32::MAX`, not `u32::MAX` (which would wrap negative and break LIMIT).
    let players = repos.dicks.get_top(chat_id, 0, i32::MAX as u32).await?;
    // unlike `/tax` itself (a many-payers-to-many-recipients pool with no 1:1 relationship), this
    // installment always has exactly one payer, so each recipient's share can be attributed to
    // them precisely: one paired (payer debit, recipient credit) per recipient, both carrying the
    // other side as their counterparty. Any leftover from `redistribute_to_bottom`'s rounding
    // (floored shares may sum to less than `payout`) gets its own counterparty-less row, so the
    // payer's total debit still adds up to exactly `-payout`.
    let mut ledger_deltas: Vec<(UserId, i32, Option<UserId>)> = Vec::new();
    let mut distributed: i32 = 0;
    let mut recipients = Vec::new();

    let bottom = if players.len() > bottom_n { &players[players.len() - bottom_n..] } else { &[] };
    let benchmark_length = if players.len() > bottom_n { players[players.len() - bottom_n - 1].length } else { 0 };
    match redistribute_to_bottom(bottom, benchmark_length, payout as i64) {
        Some(deltas) => for (player, (recipient_uid, delta)) in bottom.iter().zip(deltas) {
            if let Err(e) = repos.dicks.grow_no_attempts_check(chat_id, recipient_uid, delta).await {
                log::error!("couldn't redistribute a tax-debt installment share to {recipient_uid} in {chat_id}: {e}");
            }
            if delta > 0 {
                ledger_deltas.push((payer_uid, -delta, Some(recipient_uid)));
                ledger_deltas.push((recipient_uid, delta, Some(payer_uid)));
                distributed += delta;
                recipients.push((recipient_uid, player.owner_name.clone(), delta as u16));
            }
        },
        None => log::warn!("not enough players in {chat_id} to redistribute a tax-debt installment of {payout}"),
    }
    let undistributed = i32::from(payout) - distributed;
    if undistributed > 0 {
        ledger_deltas.push((payer_uid, -undistributed, None));
    }
    if let Err(e) = repos.ledger.record_many_for_chat_kind(chat_id, LedgerCategory::Tax, &ledger_deltas).await {
        log::error!("couldn't record ledger entries for a tax-debt installment redistribution in {chat_id}: {e}");
    }
    Ok(recipients)
}

#[cfg(test)]
mod test_allocate_payouts {
    use teloxide::types::UserId;
    use super::{allocate_payouts, Obligation};
    use crate::repo;

    fn bank(amount_owed: u16, payout_ratio: f32) -> Obligation {
        Obligation::Bank(repo::Loan { debt: amount_owed, payout_ratio, created_at: chrono::Utc::now() })
    }

    fn p2p(id: i32, amount_owed: u16, payout_ratio: f32) -> Obligation {
        Obligation::P2P(repo::P2PLoanObligation {
            id, creditor_uid: UserId(id as u64), amount_owed, payout_ratio,
            remaining_principal: amount_owed as i32, remaining_interest: 0,
            created_at: chrono::Utc::now(),
        })
    }

    #[test]
    fn a_single_obligation_gets_its_full_share() {
        let obligations = vec![bank(100, 0.1)];
        assert_eq!(allocate_payouts(10, &obligations), vec![1]);
    }

    #[test]
    fn mixed_kinds_share_one_pool_oldest_first() {
        // this is the regression case for the old double-dipping bug: a bank loan and a p2p
        // loan must compete for shares of the SAME gain, not each independently claim their own
        // payout_ratio of the full amount.
        let obligations = vec![bank(100, 0.6), p2p(1, 100, 0.6)];
        assert_eq!(allocate_payouts(10, &obligations), vec![6, 4]);
    }

    #[test]
    fn a_fully_exhausted_pool_leaves_nothing_for_later_obligations() {
        let obligations = vec![bank(100, 1.0), p2p(1, 100, 0.5)];
        assert_eq!(allocate_payouts(10, &obligations), vec![10, 0]);
    }

    #[test]
    fn payout_never_exceeds_the_remaining_debt() {
        let obligations = vec![bank(3, 1.0)];
        assert_eq!(allocate_payouts(10, &obligations), vec![3]);
    }

    #[test]
    fn a_negative_or_zero_gain_pays_nothing() {
        let obligations = vec![bank(100, 0.5)];
        assert_eq!(allocate_payouts(0, &obligations), vec![0]);
        assert_eq!(allocate_payouts(-5, &obligations), vec![0]);
    }
}

#[cfg(test)]
mod test_settle_gain_against_debts {
    use teloxide::types::UserId;
    use crate::{config, repo};
    use crate::repo::LedgerCategory;
    use crate::repo::test::dicks::{create_another_user_and_dick, create_dick, create_user};
    use crate::repo::test::{get_chat_id_and_dicks, start_postgres, CHAT_ID_KIND, UID, USER_ID};
    use super::settle_gain_against_debts;

    /// `lend()` computes the interest deterministically but must not log it to the Ledger right
    /// away - only `settle_gain_against_debts` does, as each round actually realizes some of it
    /// (see `repo::P2PLoans::lend`'s docs). Otherwise `/stats` would show interest as
    /// "earned"/"owed" before the borrower ever paid a single ghei of it.
    #[tokio::test]
    async fn test_loan_interest_is_logged_gradually_not_upfront() {
        let (_container, db) = start_postgres().await;
        let (chat_id, _dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

        create_user(&db).await;
        create_dick(&db).await; // UID - lender
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "borrower", 0).await;
        let lender_uid = USER_ID;
        let borrower_uid = UserId((UID + 1) as u64);

        let cfg = config::AppConfig { p2p_loan_payout_ratio: 0.5, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);

        // principal = 100, rate = 10% -> interest = 10, fully deterministic right away, but not
        // collected yet - nothing should appear in the Ledger.
        repos.p2p_loans.lend(&chat_id_partiality, lender_uid, borrower_uid, 100, Some(0.1)).await
            .expect("couldn't create the loan");

        let lender_breakdown = repos.ledger.get_breakdown(&chat_id_partiality, lender_uid).await.expect("couldn't fetch lender's breakdown");
        assert!(lender_breakdown.iter().all(|b| b.category != LedgerCategory::LoanInterest),
            "no interest has been collected yet, so nothing should be logged");

        // the borrower grows by 5: allocate_payouts withholds round(5 * 50%) = 3, interest-first
        // (the loan's interest pool is 10, so all 3 ghei of this payout are interest).
        settle_gain_against_debts(&repos, borrower_uid, &CHAT_ID_KIND, 5, 0, true).await.expect("couldn't settle the first gain");

        let lender_breakdown = repos.ledger.get_breakdown(&chat_id_partiality, lender_uid).await.expect("couldn't fetch lender's breakdown");
        let lender_interest = lender_breakdown.iter().find(|b| b.category == LedgerCategory::LoanInterest)
            .expect("the lender's first realized interest chunk must now be logged");
        assert_eq!((lender_interest.dare, lender_interest.avere), (0, 3));
        let borrower_breakdown = repos.ledger.get_breakdown(&chat_id_partiality, borrower_uid).await.expect("couldn't fetch borrower's breakdown");
        let borrower_interest = borrower_breakdown.iter().find(|b| b.category == LedgerCategory::LoanInterest)
            .expect("the borrower's first realized interest chunk must now be logged");
        assert_eq!((borrower_interest.dare, borrower_interest.avere), (3, 0));

        // the borrower grows by another 50: withholds round(50 * 50%) = 25 (well within the 107
        // ghei still owed); interest-first drains the remaining 7 of interest, then 18 of principal.
        settle_gain_against_debts(&repos, borrower_uid, &CHAT_ID_KIND, 50, 0, true).await.expect("couldn't settle the second gain");

        let lender_breakdown = repos.ledger.get_breakdown(&chat_id_partiality, lender_uid).await.expect("couldn't fetch lender's breakdown");
        let lender_interest = lender_breakdown.iter().find(|b| b.category == LedgerCategory::LoanInterest).unwrap();
        // cumulative across both rounds: 3 + 7 = 10, exactly the original gross interest - never
        // more, since `split_payment` never drains `remaining_interest` past zero.
        assert_eq!((lender_interest.dare, lender_interest.avere), (0, 10));
    }

    /// A negative rate produces *two* fully independent obligations, not a discount on one: the
    /// borrower still owes the full principal back (funded by the borrower's own growth), while
    /// the lender *separately* commits to paying the borrower the interest's magnitude back
    /// (funded by the *lender's* own growth) - see `repo::P2PLoans::lend`. A single player can be
    /// obligated both ways at once (a borrower on one loan, a negative-rate lender on another),
    /// and `settle_gain_against_debts` must settle both, oldest first, from a single gain - but a
    /// gain from the *other* side of a given loan must never touch an obligation that isn't theirs.
    #[tokio::test]
    async fn test_negative_rate_creates_two_independent_obligations() {
        let (_container, db) = start_postgres().await;
        let (chat_id, dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality = chat_id.clone().into();

        create_user(&db).await;
        create_dick(&db).await; // UID, length 0 - lender on loan #1, borrower on loan #2
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "second", 0).await; // borrower on loan #1
        create_another_user_and_dick(&db, &chat_id_partiality, 3, "third", 0).await; // lender on loan #2
        let middle_uid = USER_ID;
        let second_uid = UserId((UID + 1) as u64);
        let third_uid = UserId((UID + 2) as u64);

        let cfg = config::AppConfig { p2p_loan_payout_ratio: 0.5, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);

        // loan #1: middle lends 50 to second at -50% - interest = -25, so on top of second still
        // owing the full 50 principal back, middle (the lender) separately commits to paying
        // second 25 back out of middle's *own* future growth.
        repos.p2p_loans.lend(&chat_id_partiality, middle_uid, second_uid, 50, Some(-0.5)).await
            .expect("couldn't create the negative-rate loan");

        let middle_obligations = repos.p2p_loans.get_active_loans(middle_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch middle's obligations after loan #1");
        assert_eq!(middle_obligations.len(), 1, "middle owes the interest discount, separately from the principal");
        assert_eq!(middle_obligations[0].amount_owed, 25);

        // loan #2: third lends 10 to middle at the usual +10% - interest = 1, the normal
        // direction (middle, the borrower, is obligated for principal + interest, as always).
        repos.p2p_loans.lend(&chat_id_partiality, third_uid, middle_uid, 10, Some(0.1)).await
            .expect("couldn't create the positive-rate loan");

        let middle_obligations = repos.p2p_loans.get_active_loans(middle_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch middle's obligations after loan #2");
        assert_eq!(middle_obligations.len(), 2, "middle is obligated both as a negative-rate lender (loan #1) and a borrower (loan #2)");
        assert_eq!(middle_obligations[1].amount_owed, 11);

        // middle gains 20 ghei (e.g. from /grow): allocate_payouts gives the oldest obligation
        // (loan #1's reciprocal row, 50% ratio) min(10, 25, 20) = 10 first, leaving 10 for loan
        // #2's min(10, 11, 10) = 10. Crucially, this must NOT touch second's own obligation on
        // loan #1 (still 50) - that's funded by second's growth, not middle's.
        let outcome = settle_gain_against_debts(&repos, middle_uid, &CHAT_ID_KIND, 20, 0, true).await
            .expect("couldn't settle middle's gain");
        // names come back through `Username::escaped()`, which wraps them in U+200E marks
        let paid_names: Vec<_> = outcome.detail_lines.iter().map(|(name, payout, _)| (name.trim_matches('\u{200E}'), *payout)).collect();
        assert_eq!(paid_names, vec![("second", 10), ("third", 10)]);
        assert_eq!(outcome.total_withheld, 20);

        let middle_obligations = repos.p2p_loans.get_active_loans(middle_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch middle's obligations after settling");
        assert_eq!(middle_obligations[0].amount_owed, 15, "loan #1's reciprocal debt moved from 25 towards 0 by 10");
        assert_eq!(middle_obligations[1].amount_owed, 1, "loan #2's debt moved from 11 towards 0 by 10");

        let second_obligations = repos.p2p_loans.get_active_loans(second_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch second's obligations after middle's settling");
        assert_eq!(second_obligations[0].amount_owed, 50, "middle's gain must not pay down second's own principal obligation");

        // now second grows too: their OWN obligation (the 50 principal on loan #1) gets paid
        // down from their OWN growth, completely independently of middle's reciprocal obligation.
        let outcome = settle_gain_against_debts(&repos, second_uid, &CHAT_ID_KIND, 20, 0, true).await
            .expect("couldn't settle second's gain");
        let paid_by_second_names: Vec<_> = outcome.detail_lines.iter().map(|(name, payout, _)| (name.trim_matches('\u{200E}'), *payout)).collect();
        // middle was registered via `create_user`, which uses the shared `NAME` constant ("test")
        assert_eq!(paid_by_second_names, vec![(crate::repo::test::NAME, 10)]);

        let second_obligations = repos.p2p_loans.get_active_loans(second_uid, &CHAT_ID_KIND).await
            .expect("couldn't fetch second's obligations after settling");
        assert_eq!(second_obligations[0].amount_owed, 40, "second's own growth paid down their own principal obligation");
        let middle_obligations = repos.p2p_loans.get_active_loans(middle_uid, &CHAT_ID_KIND).await
            .expect("couldn't re-fetch middle's obligations");
        assert_eq!(middle_obligations[0].amount_owed, 15, "second's growth must not touch middle's reciprocal obligation");

        // `settle_gain_against_debts` only credits the creditors - it's the caller's job to
        // apply the net gain to the obligated player itself (e.g. via the `Incrementor` or
        // PVP's own debit), so neither middle's nor second's own length is touched by their own
        // settlement calls above; each creditor's length reflects both the principal they
        // received/gave as part of `lend()` *and* every repayment credited to them since.
        let middle_length = dicks.fetch_length(middle_uid, &CHAT_ID_KIND).await.expect("couldn't fetch middle's length");
        // middle: -50 (gave the loan #1 principal) +10 (received the loan #2 principal) +10 (second's repayment)
        assert_eq!(middle_length, -30);
        let second_length = dicks.fetch_length(second_uid, &CHAT_ID_KIND).await.expect("couldn't fetch second's length");
        // second: +50 (received the loan #1 principal) +10 (middle's reciprocal repayment)
        assert_eq!(second_length, 60);
        let third_length = dicks.fetch_length(third_uid, &CHAT_ID_KIND).await.expect("couldn't fetch third's length");
        // third: -10 (gave the loan #2 principal) +10 (middle's repayment)
        assert_eq!(third_length, 0);
    }

    /// The whole point of extending the Ledger with `LoanPrincipal` and `counterparty_uid` (see
    /// the "estratto conto" personal statement feature) is that summing every Ledger row for a
    /// player in a chat reconciles exactly with their actual `Dicks.length` - this is the
    /// regression test for that invariant across a bank loan and a P2P loan, both originated and
    /// then partially repaid through debt settlement.
    #[tokio::test]
    async fn test_ledger_reconciles_with_actual_length_across_loans() {
        use crate::domain::LanguageCode;
        use crate::handlers::p2p_loan::{p2p_loan_impl_accept, P2PLoanParams};
        use crate::handlers::pvp::UserInfo;

        let (_container, db) = start_postgres().await;
        let (chat_id, dicks) = get_chat_id_and_dicks(&db);
        let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

        create_user(&db).await; // UID - the lender
        create_another_user_and_dick(&db, &chat_id_partiality, 2, "borrower", 0).await;
        let lender_uid = USER_ID;
        let borrower_uid = UserId((UID + 1) as u64);

        let cfg = config::AppConfig { loan_payout_ratio: 0.5, p2p_loan_payout_ratio: 0.5, ..Default::default() };
        let repos = repo::Repositories::new(&db, &cfg);

        // the lender needs enough length on hand before lending it out - give them a logged
        // 100-ghei grow first (`check_dick` requires `length >= amount` to lend).
        repos.dicks.create_or_grow(lender_uid, &chat_id_partiality, 100).await.expect("couldn't fund the lender");
        repos.ledger.record(&chat_id_partiality, lender_uid, LedgerCategory::Grow, 100, None).await
            .expect("couldn't record the lender's funding grow");

        // 1) a bank loan: `Loans::borrow` plus the matching Ledger entry that `loan::callback_handler`
        // records in production (it can't be invoked directly here since it also needs a live `Bot`).
        repos.loans.borrow(borrower_uid, &CHAT_ID_KIND, 20).await.expect("couldn't create the bank loan");
        repos.ledger.record_for_chat_kind(&CHAT_ID_KIND, borrower_uid, LedgerCategory::LoanPrincipal, 20, None).await
            .expect("couldn't record the bank loan's principal");

        // 2) a P2P loan, lender -> borrower, 100 principal at 10% interest - through the real
        // production entry point, so its own Ledger logging (added in `p2p_loan_impl_accept`) is
        // actually exercised here, not just mimicked.
        let borrower_info = UserInfo { uid: borrower_uid, name: "borrower".to_owned().into() };
        let params = P2PLoanParams {
            repos: repos.clone(),
            chat_id: chat_id_partiality.clone(),
            lang_code: LanguageCode::new("lmo".to_owned()),
            interest_rate: 0.1,
            tax_bottom_ranks: 0,
        };
        let details_store = crate::handlers::utils::details_store::DetailsStore::default();
        p2p_loan_impl_accept(params, lender_uid, borrower_info, 100, Some(10.0), &details_store).await
            .expect("couldn't accept the p2p loan");

        // 3) the borrower "grows" by 50 (gross) - mimicking `dick::grow_impl`'s own bookkeeping
        // (the gross is logged as `Grow`, debts are settled out of the base increment, and only
        // the net actually lands on the borrower's own length) without needing a full
        // `Incrementor`/`Perk` stack here. This exercises both obligations (bank + P2P, oldest
        // first) competing for shares of the same gain through `settle_gain_against_debts`.
        repos.ledger.record_for_chat_kind(&CHAT_ID_KIND, borrower_uid, LedgerCategory::Grow, 50, None).await
            .expect("couldn't record the grow event");
        let settlement = settle_gain_against_debts(&repos, borrower_uid, &CHAT_ID_KIND, 50, 0, true).await
            .expect("couldn't settle the borrower's gain");
        repos.dicks.grow_no_attempts_check(&CHAT_ID_KIND, borrower_uid, 50 - settlement.total_withheld).await
            .expect("couldn't credit the borrower's net growth");

        // reconcile: the sum of every Ledger row for each player in this chat must equal their
        // actual length - exactly the invariant the personal statement relies on.
        for (uid, label) in [(borrower_uid, "borrower"), (lender_uid, "lender")] {
            let breakdown = repos.ledger.get_breakdown(&chat_id_partiality, uid).await
                .unwrap_or_else(|e| panic!("couldn't fetch {label}'s breakdown: {e}"));
            let ledger_sum: i64 = breakdown.iter().map(|b| b.avere - b.dare).sum();
            let actual_length = dicks.fetch_length(uid, &CHAT_ID_KIND).await
                .unwrap_or_else(|e| panic!("couldn't fetch {label}'s length: {e}"));
            assert_eq!(ledger_sum, actual_length as i64, "{label}'s ledger doesn't reconcile with their actual length");
        }

        // and the borrower's `/estratto` page shows every row with the right counterparty.
        let page = repos.ledger.get_page(&chat_id_partiality, borrower_uid, 0, 10).await
            .expect("couldn't get the borrower's statement page");
        // bank loan + p2p principal (both originated) + grow + p2p interest repayment + p2p principal repayment + bank repayment
        assert_eq!(page.len(), 6);
        let bank_entry = page.iter().find(|e| e.category == LedgerCategory::LoanPrincipal && e.amount == 20)
            .expect("the bank loan's origination must appear");
        assert!(bank_entry.counterparty.is_none(), "a bank loan has no human counterparty");
        let p2p_principal_entry = page.iter().find(|e| e.category == LedgerCategory::LoanPrincipal && e.amount == 100)
            .expect("the p2p loan's principal must appear");
        assert_eq!(p2p_principal_entry.counterparty.as_ref().expect("must have a counterparty").0, lender_uid);
    }
}
