use anyhow::{anyhow, Context};
use sqlx::Postgres;
use teloxide::types::UserId;

use crate::config;
use crate::repo::{ChatIdKind, ChatIdPartiality, Chats, Dicks, GrowthResult, ensure_only_one_row_updated};

/// One of `uid`'s active obligations to pay someone else - always literally "`uid` is the
/// borrower of this row", but that row isn't necessarily the original loan itself: a negative-
/// rate loan produces a *second*, separate row with the lender and borrower swapped, so the
/// lender's own future growth funds paying the borrower back (see `P2PLoans::lend`). `uid` may
/// have several such rows at once, e.g. a borrower on one loan and a negative-rate lender on
/// another. `remaining_principal`/`remaining_interest` mirror the row's own columns (see the
/// migration that introduced them) and are exposed so `crate::handlers::debt_settlement` can
/// split each repayment without a second query - see `split_payment`. `created_at` is exposed
/// for the
/// unified, oldest-first repayment queue shared with the bank loan and any other obligation kind.
#[derive(Debug)]
pub struct P2PLoanObligation {
    pub id: i32,
    pub creditor_uid: UserId,
    pub amount_owed: u16,
    pub payout_ratio: f32,
    pub remaining_principal: i32,
    pub remaining_interest: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

struct P2PLoanObligationEntity {
    id: i32,
    creditor_uid: i64,
    amount_owed: i32,
    payout_ratio: f32,
    remaining_principal: i32,
    remaining_interest: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl TryFrom<P2PLoanObligationEntity> for P2PLoanObligation {
    type Error = std::num::TryFromIntError;

    fn try_from(value: P2PLoanObligationEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            creditor_uid: UserId(value.creditor_uid as u64),
            amount_owed: value.amount_owed.try_into()?,
            payout_ratio: value.payout_ratio,
            remaining_principal: value.remaining_principal,
            remaining_interest: value.remaining_interest,
            created_at: value.created_at,
        })
    }
}

/// A loan from either side's perspective, with the *other* party's name - for read-only display
/// (see `crate::handlers::p2p_loan::p2p_loan_status_impl`). Unlike `P2PLoan`, `debt` is kept as
/// `i32` straight from the column: nothing here does arithmetic with it, so there's no need to
/// constrain it to (and risk a conversion failure against) the `u16` that's only required where
/// the repayment math actually happens. `original_principal`/`original_interest` are the
/// immutable snapshots taken at creation (see the migration that added them) - unlike `debt`,
/// these never change, so `/debiti` can show how much of the loan has been repaid so far.
#[derive(Debug)]
pub struct P2PLoanStatus {
    pub counterparty_uid: i64,
    pub counterparty_name: String,
    pub debt: i32,
    pub payout_ratio: f32,
    pub original_principal: i32,
    pub original_interest: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

struct P2PLoanStatusEntity {
    counterparty_uid: i64,
    counterparty_name: String,
    debt: i32,
    payout_ratio: f32,
    original_principal: i32,
    original_interest: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<P2PLoanStatusEntity> for P2PLoanStatus {
    fn from(value: P2PLoanStatusEntity) -> Self {
        Self {
            counterparty_uid: value.counterparty_uid,
            counterparty_name: value.counterparty_name,
            debt: value.debt,
            payout_ratio: value.payout_ratio,
            original_principal: value.original_principal,
            original_interest: value.original_interest,
            created_at: value.created_at,
        }
    }
}

#[derive(Clone)]
pub struct P2PLoans {
    pool: sqlx::Pool<Postgres>,
    chats: Chats,
    dicks: Dicks,
    interest_rate: f32,
    payout_ratio: f32,
    interest_tax_rate: f32,
}

impl P2PLoans {
    pub fn new(pool: sqlx::Pool<Postgres>, cfg: &config::AppConfig) -> Self {
        Self {
            chats: Chats::new(pool.clone(), cfg.features),
            dicks: Dicks::new(pool.clone(), cfg.features),
            pool,
            interest_rate: cfg.p2p_loan_interest_rate,
            payout_ratio: cfg.p2p_loan_payout_ratio,
            interest_tax_rate: cfg.p2p_loan_interest_tax_rate,
        }
    }

    /// All of `uid`'s active obligations in `chat_id` - every loan row where `uid` is the
    /// borrower, oldest first. A single `uid` may have several at once: a normal loan they
    /// borrowed, and/or the reciprocal row of a negative-rate loan they themselves lent (see
    /// `lend`). When a single growth event isn't enough to cover every creditor, the oldest
    /// obligation (first in the returned list) is prioritized for repayment (see
    /// `crate::handlers::perks::P2PLoanPayoutPerk`).
    pub async fn get_active_loans(&self, uid: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Vec<P2PLoanObligation>> {
        let entities = sqlx::query_as!(P2PLoanObligationEntity,
            r#"SELECT id, lender_uid AS "creditor_uid!", debt AS "amount_owed!", payout_ratio,
                    remaining_principal, remaining_interest, created_at
                FROM P2P_Loans
                WHERE borrower_uid = $1
                    AND chat_id = (SELECT id FROM Chats WHERE chat_id = $2::bigint OR chat_instance = $2::text)
                    AND repaid_at IS NULL
                ORDER BY created_at ASC"#,
                uid.0 as i64, chat_id.value() as String)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the active p2p loan obligations for {chat_id} and {uid}"))?;
        // a single malformed row (e.g. legacy data) shouldn't block repayment of this uid's
        // other, perfectly fine obligations - skip and log it instead of failing the whole batch.
        Ok(entities.into_iter()
            .filter_map(|entity| {
                let id = entity.id;
                P2PLoanObligation::try_from(entity)
                    .inspect_err(|e| log::error!("skipping a corrupt p2p loan #{id} for {chat_id} and {uid}: {e}"))
                    .ok()
            })
            .collect())
    }

    /// `borrower`'s active loans in `chat_id`, grouped by lender name (oldest first within a
    /// group) - a read-only counterpart to `get_active_loans` for showing "what do I still owe"
    /// (see `crate::handlers::p2p_loan::p2p_loan_status_impl`). Unlike `get_active_loans`, this is
    /// purely for display, so grouping by counterparty name takes priority over chronological
    /// order.
    pub async fn get_active_loans_as_borrower(&self, borrower: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Vec<P2PLoanStatus>> {
        sqlx::query_as!(P2PLoanStatusEntity,
            r#"SELECT pl.lender_uid AS "counterparty_uid!", u.name AS counterparty_name, pl.debt, pl.payout_ratio, pl.original_principal, pl.original_interest, pl.created_at
                    FROM P2P_Loans pl
                    JOIN Users u ON u.uid = pl.lender_uid
                    WHERE pl.borrower_uid = $1 AND
                    pl.chat_id = (SELECT id FROM Chats WHERE chat_id = $2::bigint OR chat_instance = $2::text)
                    AND pl.repaid_at IS NULL
                    ORDER BY u.name ASC, pl.created_at ASC"#,
                borrower.0 as i64, chat_id.value() as String)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the active p2p loans (as borrower) for {chat_id} and {borrower}"))
            .map(|rows| rows.into_iter().map(P2PLoanStatus::from).collect())
    }

    /// Symmetric to `get_active_loans_as_borrower`: `lender`'s active loans in `chat_id`,
    /// grouped by borrower name (oldest first within a group) - "what's still owed to me".
    pub async fn get_active_loans_as_lender(&self, lender: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Vec<P2PLoanStatus>> {
        sqlx::query_as!(P2PLoanStatusEntity,
            r#"SELECT pl.borrower_uid AS "counterparty_uid!", u.name AS counterparty_name, pl.debt, pl.payout_ratio, pl.original_principal, pl.original_interest, pl.created_at
                    FROM P2P_Loans pl
                    JOIN Users u ON u.uid = pl.borrower_uid
                    WHERE pl.lender_uid = $1 AND
                    pl.chat_id = (SELECT id FROM Chats WHERE chat_id = $2::bigint OR chat_instance = $2::text)
                    AND pl.repaid_at IS NULL
                    ORDER BY u.name ASC, pl.created_at ASC"#,
                lender.0 as i64, chat_id.value() as String)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the active p2p loans (as lender) for {chat_id} and {lender}"))
            .map(|rows| rows.into_iter().map(P2PLoanStatus::from).collect())
    }

    /// Transfers `principal` from the lender to the borrower right now, and records a debt of
    /// `principal + max(interest, 0)`: the borrower always owes the full principal back,
    /// regardless of how negative `interest_rate` is, plus any *positive* interest on top (the
    /// usual case), repaid automatically out of the *borrower's* future growth (see
    /// `crate::handlers::perks::P2PLoanPayoutPerk`). If `interest` is negative, a *second*, fully
    /// independent loan row is created with the lender and borrower swapped: the lender commits
    /// to paying the borrower `interest`'s magnitude back out of their *own* future growth - never
    /// netted into the row above, since that would incorrectly fund the discount out of the
    /// borrower's growth instead of the lender's. `custom_interest_rate` overrides the configured
    /// default rate for this loan specifically (e.g. when either side negotiated a different one
    /// via the inline syntax). A borrower (or a lender on a negative-rate loan) may have any
    /// number of simultaneous obligations, including several to/from the same counterparty.
    ///
    /// Also returns the signed `interest` (so the caller knows whether the lender or the borrower
    /// is the one realizing it, and thus who owes tax on it), that tax itself (`|interest| *
    /// interest_tax_rate`, rounded), and the id of whichever row actually carries the realized
    /// interest (the normal row if `interest >= 0`, the reciprocal one otherwise) - since that
    /// portion is deterministic right away (both rates are fixed at creation time), the caller
    /// is responsible for actually levying it: not by withholding it immediately, but by turning
    /// it into its own gradual tax-debt obligation (see
    /// `crate::handlers::p2p_loan::p2p_loan_impl_accept`, `repo::LoanInterestTaxDebts::create`),
    /// traced back to the returned loan id purely for display/debugging. Unlike the tax, the
    /// interest itself is *not* logged to the Ledger here even though it's just as
    /// deterministic: it's only realized gradually, as `settle_from_award` actually collects it,
    /// so that's where it's logged - see that function's docs.
    pub async fn lend(&self, chat_id: &ChatIdPartiality, lender: UserId, borrower: UserId, principal: u16, custom_interest_rate: Option<f32>) -> anyhow::Result<(GrowthResult, GrowthResult, i32, u16, i32)> {
        // validated *before* any money moves, so a rate that's out of range for this principal
        // never leaves the system in a half-applied state (principal transferred but loan
        // rejected) - see `crate::handlers::p2p_loan` for the user-facing rejection that normally
        // catches this first; this is the last-resort backstop that actually protects the database.
        let (interest, tax) = self.interest_and_tax(principal, custom_interest_rate)
            .ok_or_else(|| anyhow!("interest rate is out of range for a principal of {principal} ghei in {chat_id}: {lender} -> {borrower}"))?;

        let chat_internal_id = self.dicks.resolve_chat(chat_id).await?;
        let mut tx = self.pool.begin().await?;
        let (length_lender, length_borrower, interest_row_id) =
            self.lend_in_tx(&mut tx, chat_internal_id, lender, borrower, principal, interest).await?;
        tx.commit().await?;

        let lender_res = self.dicks.growth_result_after(chat_internal_id, lender, length_lender).await?;
        let borrower_res = self.dicks.growth_result_after(chat_internal_id, borrower, length_borrower).await?;
        Ok((lender_res, borrower_res, interest, tax, interest_row_id))
    }

    /// `interest_and_tax` resolved against this `P2PLoans`' own configured defaults (rate, tax
    /// rate) - exposed so a caller that needs to validate a rate *before* committing to a
    /// transaction (`crate::handlers::combo`, or `lend` itself above) doesn't have to know those
    /// defaults itself. `None` if the resolved rate is out of range for `principal` - see
    /// `interest_and_tax`'s own docs.
    pub(crate) fn interest_and_tax(&self, principal: u16, custom_interest_rate: Option<f32>) -> Option<(i32, u16)> {
        let interest_rate = custom_interest_rate.unwrap_or(self.interest_rate);
        interest_and_tax(principal, interest_rate, self.interest_tax_rate)
    }

    /// The actual work behind `lend` - the principal transfer plus the loan row(s) it creates -
    /// against an externally owned `tx` that this never commits, exactly like
    /// `Dicks::move_length_in_tx`. Both now share one transaction instead of `lend`'s previous
    /// two separate ones (a pre-existing, unrelated-to-combo gap this also happens to close: the
    /// principal transfer and the loan row used to each get their own transaction, so a crash
    /// between them could have left the principal moved with no loan row to ever collect it
    /// back). `interest` must already be `interest_and_tax(...)`'s first element - callers reject
    /// the loan outright (an error message, never this function) when that's `None`.
    pub(crate) async fn lend_in_tx(&self, tx: &mut sqlx::Transaction<'_, Postgres>, chat_internal_id: i64, lender: UserId, borrower: UserId, principal: u16, interest: i32) -> anyhow::Result<(i32, i32, i32)> {
        let (length_lender, length_borrower) = Dicks::move_length_in_tx(tx, chat_internal_id, lender, borrower, principal).await?;

        // the normal row: the borrower's full principal, plus any positive interest on top -
        // `remaining_principal`/`remaining_interest` split it the same way for gradual Ledger
        // recognition (see `split_payment`).
        let principal_owed = principal as i32 + interest.max(0);
        let normal_row_id = sqlx::query_scalar!(
            "INSERT INTO P2P_Loans (chat_id, lender_uid, borrower_uid, debt, remaining_principal, remaining_interest, payout_ratio, original_principal, original_interest)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $5, $6) RETURNING id",
            chat_internal_id, lender.0 as i64, borrower.0 as i64, principal_owed, principal as i32, interest.max(0), self.payout_ratio)
            .fetch_one(&mut **tx)
            .await
            .context(format!("couldn't create a p2p loan for chat #{chat_internal_id}: {lender} -> {borrower}, principal = {principal}"))?;
        let interest_row_id = if interest < 0 {
            // the reciprocal row: purely the lender's discount obligation, no principal of its
            // own (see the struct doc on `P2PLoanObligation`).
            let lender_owed = interest.unsigned_abs() as i32;
            sqlx::query_scalar!(
                "INSERT INTO P2P_Loans (chat_id, lender_uid, borrower_uid, debt, remaining_principal, remaining_interest, payout_ratio, original_principal, original_interest)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $5, $6) RETURNING id",
                chat_internal_id, borrower.0 as i64, lender.0 as i64, lender_owed, 0, lender_owed, self.payout_ratio)
                .fetch_one(&mut **tx)
                .await
                .context(format!("couldn't create the reciprocal p2p loan for chat #{chat_internal_id}: {borrower} -> {lender}, interest = {interest}"))?
        } else {
            normal_row_id
        };
        Ok((length_lender, length_borrower, interest_row_id))
    }

    /// Moves loan `id`'s debt `magnitude` closer to zero, splitting the payment across the
    /// `remaining_principal`/`remaining_interest` pools (see `split_payment`) so the caller
    /// (`settle_from_award`) can log only the interest actually collected this round to the
    /// Ledger. Takes the pools as `(remaining_principal, remaining_interest)` since the caller
    /// already has them from `get_active_loans`, rather than re-querying. Every row's debt is a
    /// plain, non-negative amount owed by that row's borrower (see `get_active_loans`), so this
    /// is a simple subtraction - no direction or sign to resolve. A borrower (or lender) may have
    /// several loans at once, so the generic uid+chat lookup used elsewhere would be ambiguous
    /// here.
    pub async fn pay(&self, loan_id: i32, remaining_principal: i32, remaining_interest: i32, magnitude: u16) -> anyhow::Result<PaymentSplit> {
        let split = split_payment(remaining_principal, remaining_interest, magnitude);
        sqlx::query!(
            "UPDATE P2P_Loans SET
                debt = debt - $2,
                remaining_principal = remaining_principal - $3,
                remaining_interest = remaining_interest - $4
            WHERE id = $1",
                loan_id, magnitude as i32, split.principal, split.interest)
            .execute(&self.pool)
            .await
            .map_err(Into::into)
            .and_then(ensure_only_one_row_updated)
            .context(format!("couldn't pay for the p2p loan #{loan_id}: {magnitude}"))?;
        Ok(split)
    }

    /// Cancels mutual debt between two players in a chat: when each owes the other, the overlapping
    /// amount is settled directly (FIFO, oldest loan on each side first), leaving only the net
    /// difference - so two friends who keep lending back and forth don't accumulate a pile of
    /// gross obligations that would just net out at repayment anyway. Returns how many ghei were
    /// cancelled (0 if there was nothing to net).
    ///
    /// This is economically neutral and moves no length: the principal of every loan already
    /// changed hands at creation, and the eventual equilibrium is identical whether the loans are
    /// repaid gross or net (the larger debtor ends up paying exactly the difference either way).
    /// So nothing is credited/debited and nothing is logged to the Ledger - this only rewrites the
    /// outstanding `debt`/`remaining_*` of the affected rows (interest drained first, mirroring
    /// `split_payment`), marking a row repaid once its debt reaches 0. Interest-tax debts
    /// (`LoanInterestTaxDebts`) are deliberately left untouched: they're a separate obligation to
    /// the chat's poorest, already in flight, not a debt between these two players.
    pub async fn net_mutual_debts(&self, chat_internal_id: i64, uid1: UserId, uid2: UserId) -> anyhow::Result<u32> {
        let mut tx = self.pool.begin().await?;
        // "uid1 owes uid2" = rows where uid1 is the borrower and uid2 the lender, and vice versa.
        let mut owed_1_to_2 = Self::fetch_directed_active_loans(&mut tx, chat_internal_id, uid1, uid2).await?;
        let mut owed_2_to_1 = Self::fetch_directed_active_loans(&mut tx, chat_internal_id, uid2, uid1).await?;

        let mut cancelled: u32 = 0;
        let (mut i, mut j) = (0usize, 0usize);
        while i < owed_1_to_2.len() && j < owed_2_to_1.len() {
            let amount = owed_1_to_2[i].debt.min(owed_2_to_1[j].debt);
            if amount <= 0 {
                // a defensive guard against a zero/negative-debt active row (shouldn't exist) -
                // skip whichever side is exhausted so the loop can still terminate.
                if owed_1_to_2[i].debt <= 0 { i += 1 }
                if owed_2_to_1[j].debt <= 0 { j += 1 }
                continue
            }
            owed_1_to_2[i].drain(amount);
            owed_2_to_1[j].drain(amount);
            cancelled += amount as u32;
            if owed_1_to_2[i].debt == 0 { i += 1 }
            if owed_2_to_1[j].debt == 0 { j += 1 }
        }

        for loan in owed_1_to_2.iter().chain(owed_2_to_1.iter()).filter(|l| l.dirty) {
            sqlx::query!(
                "UPDATE P2P_Loans SET debt = $2, remaining_principal = $3, remaining_interest = $4,
                    repaid_at = CASE WHEN $2 = 0 THEN now() ELSE repaid_at END
                 WHERE id = $1",
                loan.id, loan.debt, loan.remaining_principal, loan.remaining_interest)
                .execute(&mut *tx)
                .await
                .map_err(Into::into)
                .and_then(ensure_only_one_row_updated)
                .context(format!("couldn't apply netting to p2p loan #{}", loan.id))?;
        }
        tx.commit().await?;
        Ok(cancelled)
    }

    async fn fetch_directed_active_loans(tx: &mut sqlx::Transaction<'_, Postgres>, chat_internal_id: i64, borrower: UserId, lender: UserId) -> anyhow::Result<Vec<NettableLoan>> {
        sqlx::query_as!(NettableLoan,
            r#"SELECT id, debt, remaining_principal, remaining_interest, false AS "dirty!"
                FROM P2P_Loans
                WHERE chat_id = $1 AND borrower_uid = $2 AND lender_uid = $3 AND repaid_at IS NULL
                ORDER BY created_at ASC, id ASC"#,
            chat_internal_id, borrower.0 as i64, lender.0 as i64)
            .fetch_all(&mut **tx)
            .await
            .context(format!("couldn't fetch active loans from {borrower} to {lender} in chat #{chat_internal_id}"))
    }

    /// Every distinct unordered pair of players who currently owe each other something in some
    /// chat - the input to a one-off rationalization that nets all pre-existing mutual debt down
    /// (see `net_mutual_debts`). Each pair is returned once, with `uid1 < uid2`.
    pub async fn mutual_debt_pairs(&self) -> anyhow::Result<Vec<(i64, UserId, UserId)>> {
        let rows = sqlx::query!(
            r#"SELECT DISTINCT a.chat_id AS "chat_id!", a.borrower_uid AS "low!", a.lender_uid AS "high!"
                FROM P2P_Loans a
                JOIN P2P_Loans b
                    ON a.chat_id = b.chat_id
                    AND a.borrower_uid = b.lender_uid
                    AND a.lender_uid = b.borrower_uid
                    AND a.repaid_at IS NULL AND b.repaid_at IS NULL
                WHERE a.borrower_uid < a.lender_uid"#)
            .fetch_all(&self.pool)
            .await
            .context("couldn't enumerate mutual debt pairs")?;
        Ok(rows.into_iter()
            .map(|r| (r.chat_id, UserId(r.low as u64), UserId(r.high as u64)))
            .collect())
    }

    /// One-off cleanup that nets every pre-existing mutual-debt pair across all chats down to its
    /// difference (see `net_mutual_debts`) - for retrofitting the on-creation netting onto debts
    /// that were taken on before it existed. Idempotent: once everything's netted there are no
    /// mutual pairs left, so a second run is a no-op. Returns `(pairs_netted, ghei_cancelled)`.
    pub async fn rationalize_all_mutual_debts(&self) -> anyhow::Result<(u32, u32)> {
        let pairs = self.mutual_debt_pairs().await?;
        let mut pairs_netted = 0;
        let mut total_cancelled = 0;
        for (chat_internal_id, uid1, uid2) in pairs {
            let cancelled = self.net_mutual_debts(chat_internal_id, uid1, uid2).await?;
            if cancelled > 0 {
                pairs_netted += 1;
                total_cancelled += cancelled;
            }
        }
        Ok((pairs_netted, total_cancelled))
    }

}

/// A loan row being mutated in memory during `net_mutual_debts`, before the change is flushed.
struct NettableLoan {
    id: i32,
    debt: i32,
    remaining_principal: i32,
    remaining_interest: i32,
    /// whether `drain` has actually touched this row, so only changed rows get an UPDATE.
    dirty: bool,
}

impl NettableLoan {
    /// Cancels `amount` of this loan's debt, draining interest before principal (mirroring
    /// `split_payment`), keeping the `debt = remaining_principal + remaining_interest` invariant.
    fn drain(&mut self, amount: i32) {
        let interest_cut = amount.min(self.remaining_interest);
        self.remaining_interest -= interest_cut;
        self.remaining_principal -= amount - interest_cut;
        self.debt -= amount;
        self.dirty = true;
    }
}

#[cfg(test)]
mod test_nettable_loan {
    use super::NettableLoan;

    fn loan(principal: i32, interest: i32) -> NettableLoan {
        NettableLoan { id: 1, debt: principal + interest, remaining_principal: principal, remaining_interest: interest, dirty: false }
    }

    #[test]
    fn draining_takes_interest_first_then_principal() {
        let mut l = loan(47, 3);
        l.drain(30); // 3 of interest, then 27 of principal
        assert_eq!((l.debt, l.remaining_principal, l.remaining_interest), (20, 20, 0));
        assert!(l.dirty);
        // the invariant debt == principal + interest holds throughout.
        assert_eq!(l.debt, l.remaining_principal + l.remaining_interest);
    }

    #[test]
    fn fully_draining_zeroes_everything() {
        let mut l = loan(40, 10);
        l.drain(50);
        assert_eq!((l.debt, l.remaining_principal, l.remaining_interest), (0, 0, 0));
    }

    #[test]
    fn a_chunk_within_interest_leaves_principal_untouched() {
        let mut l = loan(100, 12);
        l.drain(5);
        assert_eq!((l.debt, l.remaining_principal, l.remaining_interest), (107, 100, 7));
    }
}

/// How a single repayment chunk of `magnitude` splits across a loan row's two pools, draining
/// `remaining_interest` first: it's the simplest rule (no rounding needed, unlike a pro-rata
/// split) and lets `settle_from_award` recognize the lender's yield in the Ledger as soon as
/// it's actually collected, before any principal repayment - see `P2PLoans::pay`. Relies on the
/// caller never passing a `magnitude` larger than `remaining_principal + remaining_interest`
/// (guaranteed by `allocate_loan_payouts`, which caps it at the row's own `amount_owed`).
pub struct PaymentSplit {
    pub principal: i32,
    pub interest: i32,
}

fn split_payment(remaining_principal: i32, remaining_interest: i32, magnitude: u16) -> PaymentSplit {
    let magnitude = magnitude as i32;
    let interest = magnitude.min(remaining_interest);
    let principal = magnitude - interest;
    debug_assert!(principal <= remaining_principal, "split_payment was given a magnitude larger than the row's own debt");
    PaymentSplit { principal, interest }
}

/// A flat ceiling on any interest rate, custom or configured, regardless of principal: 1000%
/// (10.0 as a fraction). `compute_interest`'s overflow check alone still permits, say, a
/// 1,000,000% rate on a 1-ghei loan (the resulting interest fits comfortably) - harmless in that
/// one instance, but exactly the kind of value a fat-fingered or careless custom rate produces,
/// so it's rejected outright here rather than relying solely on principal happening to be small.
pub const MAX_INTEREST_RATE: f32 = 10.0;

/// The symmetric floor: a negative rate means the *lender* commits to paying the borrower back
/// too (see `lend`), so the same flat-ceiling reasoning applies in reverse - capped at the same
/// magnitude as `MAX_INTEREST_RATE` rather than a separate, harder-to-justify number.
pub const MIN_INTEREST_RATE: f32 = -MAX_INTEREST_RATE;

/// The interest on a loan of `principal` at `interest_rate`: `principal * interest_rate`,
/// rounded - computed once here so neither the bot's display code nor the database layer can
/// disagree about it. Positive: the borrower owes it on top of the principal, as usual. Negative:
/// the *lender* ends up owing its magnitude *to* the borrower instead, as a fully separate
/// obligation paid out of the lender's own future growth - the borrower still owes the principal
/// in full either way (see `P2PLoans::lend`). `None` if `interest_rate` is outside
/// `[MIN_INTEREST_RATE, MAX_INTEREST_RATE]`, or if the result wouldn't fit in a `u16`'s
/// magnitude - callers must reject the loan outright in either case rather than letting a
/// saturated or wrapped value through: a custom rate of 9999999% on a 1-ghei loan once overflowed
/// `debt` past `i32::MAX` (`f32 as i32` saturates on overflow) and wrapped the resulting `tax`
/// past `u16::MAX` (`i32 as u16` truncates instead), corrupting real balances.
pub fn compute_interest(principal: u16, interest_rate: f32) -> Option<i32> {
    if !(MIN_INTEREST_RATE..=MAX_INTEREST_RATE).contains(&interest_rate) {
        return None
    }
    let interest = (principal as f64 * interest_rate as f64).round();
    let bound = u16::MAX as f64;
    (interest.is_finite() && (-bound..=bound).contains(&interest)).then(|| interest as i32)
}

/// `interest` is `compute_interest(principal, interest_rate)`; `tax` is `interest_tax_rate` of
/// `interest`'s magnitude, rounded and never exceeding it regardless of how `interest_tax_rate`
/// is configured - whichever side actually realizes a gain pays it: the lender for a non-negative
/// rate (the usual case), the borrower for a negative one, since that's the side who comes out
/// ahead relative to a plain 0% loan (see `crate::handlers::p2p_loan::p2p_loan_impl_accept`).
/// `None` if `compute_interest` rejects the rate, or if the borrower's own row (`principal +
/// max(interest, 0)`) wouldn't fit a `u16` - `principal` and a positive `interest` are each
/// individually bounded by `u16::MAX`, but their *sum* can still exceed it.
fn interest_and_tax(principal: u16, interest_rate: f32, interest_tax_rate: f32) -> Option<(i32, u16)> {
    let interest = compute_interest(principal, interest_rate)?;
    if principal as i64 + interest.max(0) as i64 > u16::MAX as i64 {
        return None
    }
    let tax_magnitude = (interest.unsigned_abs() as f32 * interest_tax_rate).round() as u32;
    let tax = tax_magnitude.min(interest.unsigned_abs());
    Some((interest, tax as u16))
}

#[cfg(test)]
mod test {
    use super::interest_and_tax;

    #[test]
    fn no_tax_configured_means_no_tax() {
        assert_eq!(interest_and_tax(10, 0.1, 0.0), Some((1, 0)));
    }

    #[test]
    fn tax_is_a_share_of_the_interest_not_of_the_principal() {
        // principal = 100, rate = 10% -> interest = 10; tax = 26% of that interest = 2.6 -> 3
        assert_eq!(interest_and_tax(100, 0.1, 0.26), Some((10, 3)));
    }

    #[test]
    fn tax_never_exceeds_the_interest_itself() {
        assert_eq!(interest_and_tax(100, 0.1, 1.5), Some((10, 10)));
    }

    #[test]
    fn rounding_can_make_a_small_loans_tax_negligible() {
        // interest = 1, 26% of 1 rounds down to 0 - no tax is levied on tiny loans
        assert_eq!(interest_and_tax(10, 0.1, 0.26), Some((1, 0)));
    }

    #[test]
    fn an_excessive_rate_is_rejected_instead_of_overflowing() {
        // 9999999% on a 1-ghei loan: the exact input that once overflowed `debt` past `i32::MAX`
        // and wrapped `tax` past `u16::MAX`, corrupting real balances.
        assert_eq!(interest_and_tax(1, 99999.99, 0.26), None);
    }

    #[test]
    fn a_large_principal_can_overflow_even_a_modest_rate() {
        // interest = 6554 fits on its own, but principal + interest = 72089 doesn't fit the
        // borrower's own row.
        assert_eq!(interest_and_tax(u16::MAX, 0.1, 0.0), None);
    }

    #[test]
    fn a_negative_rate_makes_the_lender_owe_the_borrower() {
        // principal = 100, rate = -30% -> interest = -30 (the lender owes it - the borrower
        // still separately owes the full 100 principal regardless); tax = 26% of 30 = 7.8 -> 8,
        // paid by the lender (the side actually realizing the -30).
        assert_eq!(interest_and_tax(100, -0.3, 0.26), Some((-30, 8)));
    }

    #[test]
    fn a_rate_below_the_floor_is_rejected() {
        assert_eq!(interest_and_tax(100, -10.01, 0.26), None);
    }

    #[test]
    fn a_steeply_negative_rate_can_still_fit_even_with_a_large_principal() {
        // unlike the positive case, a negative interest is never added to the principal in the
        // same row - it's a fully independent obligation (see `P2PLoans::lend`), so only its own
        // magnitude needs to fit, not principal + interest together.
        assert_eq!(interest_and_tax(u16::MAX, -1.0, 0.0), Some((-65535, 0)));
    }
}

#[cfg(test)]
mod test_compute_interest {
    use super::compute_interest;

    #[test]
    fn fits_comfortably() {
        assert_eq!(compute_interest(10, 0.1), Some(1));
    }

    #[test]
    fn rejects_an_excessive_rate() {
        assert_eq!(compute_interest(1, 99999.99), None);
    }

    #[test]
    fn rejects_non_finite_results() {
        assert_eq!(compute_interest(1, f32::NAN), None);
        assert_eq!(compute_interest(1, f32::INFINITY), None);
        assert_eq!(compute_interest(1, f32::NEG_INFINITY), None);
    }

    #[test]
    fn the_boundary_is_inclusive() {
        assert_eq!(compute_interest(u16::MAX, 1.0), Some(u16::MAX as i32));
        assert_eq!(compute_interest(u16::MAX, -1.0), Some(-(u16::MAX as i32)));
    }

    #[test]
    fn an_interest_magnitude_past_the_boundary_is_rejected() {
        // 10,000 * 1000% = 100,000: the rate itself is within the allowed cap, but the
        // resulting interest doesn't fit its own u16 magnitude.
        assert_eq!(compute_interest(10000, super::MAX_INTEREST_RATE), None);
    }

    #[test]
    fn a_negative_rate_means_the_lender_owes_the_borrower() {
        assert_eq!(compute_interest(100, -0.3), Some(-30));
    }

    #[test]
    fn minus_100_percent_means_the_lender_owes_back_the_full_principal() {
        assert_eq!(compute_interest(100, -1.0), Some(-100));
    }

    #[test]
    fn a_rate_below_the_floor_is_rejected() {
        assert_eq!(compute_interest(1, super::MIN_INTEREST_RATE - 0.01), None);
    }

    #[test]
    fn a_flat_1000_percent_cap_applies_in_either_direction() {
        assert_eq!(compute_interest(1, 10000.0), None);
        assert_eq!(compute_interest(1, super::MAX_INTEREST_RATE), Some(10));
        assert_eq!(compute_interest(1, super::MAX_INTEREST_RATE + 0.01), None);
        assert_eq!(compute_interest(1, super::MIN_INTEREST_RATE), Some(-10));
    }
}

#[cfg(test)]
mod test_split_payment {
    use super::{split_payment, PaymentSplit};

    #[test]
    fn a_magnitude_smaller_than_the_interest_pool_is_pure_interest() {
        let split = split_payment(100, 30, 10);
        assert_eq!((split.principal, split.interest), (0, 10));
    }

    #[test]
    fn draining_the_interest_pool_exactly_leaves_principal_untouched() {
        let split = split_payment(100, 30, 30);
        assert_eq!((split.principal, split.interest), (0, 30));
    }

    #[test]
    fn a_magnitude_larger_than_the_interest_pool_overflows_into_principal() {
        let split = split_payment(100, 30, 50);
        assert_eq!((split.principal, split.interest), (20, 30));
    }

    #[test]
    fn a_pure_reciprocal_row_has_no_principal_to_drain() {
        // the reciprocal row of a negative-rate loan (see `P2PLoans::lend`) starts with
        // remaining_principal = 0 - the whole debt is interest.
        let split = split_payment(0, 25, 25);
        assert_eq!((split.principal, split.interest), (0, 25));
    }

    #[test]
    fn a_fully_repaid_loan_drains_nothing() {
        let PaymentSplit { principal, interest } = split_payment(0, 0, 0);
        assert_eq!((principal, interest), (0, 0));
    }
}

