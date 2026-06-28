use anyhow::Context;
use teloxide::types::UserId;

use crate::config;
use crate::repo::{ChatIdKind, ChatIdPartiality, Chats, ensure_only_one_row_updated};

/// One of `uid`'s active tax-debt obligations - unlike a `P2PLoanObligation`, this has no single
/// named creditor: each installment collected against it is redistributed to the chat's bottom-N
/// players at the moment it's collected, not credited to a fixed counterparty (see
/// `crate::handlers::debt_settlement`). `payout_ratio` is shared with P2P loans (the same
/// configured `P2P_LOAN_PAYOUT_RATIO`), not its own per-row value.
#[derive(Debug)]
pub struct TaxDebtObligation {
    pub id: i32,
    pub amount_owed: u16,
    pub payout_ratio: f32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

struct TaxDebtObligationEntity {
    id: i32,
    amount_owed: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl TaxDebtObligationEntity {
    /// `payout_ratio` isn't a column on this row (see the struct doc on `TaxDebtObligation`), so
    /// it's threaded in by the caller (`LoanInterestTaxDebts::get_active`) rather than going
    /// through a plain `TryFrom`.
    fn try_into_obligation(self, payout_ratio: f32) -> Result<TaxDebtObligation, std::num::TryFromIntError> {
        Ok(TaxDebtObligation {
            id: self.id,
            amount_owed: self.amount_owed.try_into()?,
            payout_ratio,
            created_at: self.created_at,
        })
    }
}

/// Read-only display counterpart to `TaxDebtObligation` - see `get_active_with_origin`.
#[derive(Debug)]
pub struct TaxDebtStatus {
    pub amount_owed: i32,
    pub original_debt: i32,
    pub origin_counterparty_name: Option<String>,
}

#[derive(Clone)]
pub struct LoanInterestTaxDebts {
    pool: sqlx::Pool<sqlx::Postgres>,
    chats: Chats,
    payout_ratio: f32,
}

impl LoanInterestTaxDebts {
    pub fn new(pool: sqlx::Pool<sqlx::Postgres>, cfg: &config::AppConfig) -> Self {
        Self {
            chats: Chats::new(pool.clone(), cfg.features),
            pool,
            // deliberately the same ratio as P2P loans (see the struct doc) rather than a
            // dedicated config knob - simpler, and there's no stated reason for a tax debt to
            // drain at a different pace than a regular P2P loan.
            payout_ratio: cfg.p2p_loan_payout_ratio,
        }
    }

    /// Creates a new tax-debt obligation for `payer_uid`: `amount` ghei, to be collected
    /// gradually out of `payer_uid`'s future gains and redistributed to the chat's poorest
    /// players as it's collected (see `crate::handlers::debt_settlement`). `source_loan_id` is
    /// purely for traceability (which p2p loan's interest produced this tax) - see the migration
    /// that created this table.
    pub async fn create(&self, chat_id: &ChatIdPartiality, payer_uid: UserId, amount: u16, source_loan_id: Option<i32>) -> anyhow::Result<()> {
        let chat_internal_id = self.chats.upsert_chat(chat_id).await?;
        sqlx::query!(
            "INSERT INTO Loan_Interest_Tax_Debts (chat_id, payer_uid, source_loan_id, debt, original_debt) VALUES ($1, $2, $3, $4, $4)",
            chat_internal_id, payer_uid.0 as i64, source_loan_id, amount as i32)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .context(format!("couldn't create a loan interest tax debt for {chat_id}: {payer_uid}, amount = {amount}"))
    }

    /// `payer_uid`'s active tax-debt obligations in `chat_id`, oldest first - same ordering
    /// convention as `P2PLoans::get_active_loans`, for use in the unified repayment-priority
    /// queue (see `crate::handlers::debt_settlement`).
    pub async fn get_active(&self, payer_uid: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Vec<TaxDebtObligation>> {
        let entities = sqlx::query_as!(TaxDebtObligationEntity,
            r#"SELECT id, debt AS "amount_owed!", created_at
                FROM Loan_Interest_Tax_Debts
                WHERE payer_uid = $1
                    AND chat_id = (SELECT id FROM Chats WHERE chat_id = $2::bigint OR chat_instance = $2::text)
                    AND repaid_at IS NULL
                ORDER BY created_at ASC"#,
                payer_uid.0 as i64, chat_id.value() as String)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the active loan interest tax debts for {chat_id} and {payer_uid}"))?;
        Ok(entities.into_iter()
            .filter_map(|entity| {
                let id = entity.id;
                entity.try_into_obligation(self.payout_ratio)
                    .inspect_err(|e| log::error!("skipping a corrupt loan interest tax debt #{id} for {chat_id} and {payer_uid}: {e}"))
                    .ok()
            })
            .collect())
    }

    /// `payer_uid`'s active tax-debt obligations in `chat_id`, oldest first, alongside
    /// `original_debt` (so `/debiti` can show how much has been repaid so far - see the migration
    /// that added it) and the name of the loan's *other* party, if `source_loan_id` still points
    /// to a resolvable loan and user (e.g. not a legacy debt predating that column). Read-only
    /// display counterpart to `get_active`, mirroring `P2PLoans::get_active_loans_as_borrower`.
    pub async fn get_active_with_origin(&self, payer_uid: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Vec<TaxDebtStatus>> {
        sqlx::query_as!(TaxDebtStatus,
            r#"SELECT ltd.debt AS amount_owed, ltd.original_debt,
                    u.name AS "origin_counterparty_name?"
                FROM Loan_Interest_Tax_Debts ltd
                LEFT JOIN P2P_Loans pl ON pl.id = ltd.source_loan_id
                LEFT JOIN Users u ON u.uid = CASE WHEN pl.lender_uid = ltd.payer_uid THEN pl.borrower_uid ELSE pl.lender_uid END
                WHERE ltd.payer_uid = $1
                    AND ltd.chat_id = (SELECT id FROM Chats WHERE chat_id = $2::bigint OR chat_instance = $2::text)
                    AND ltd.repaid_at IS NULL
                ORDER BY ltd.created_at ASC"#,
                payer_uid.0 as i64, chat_id.value() as String)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the active loan interest tax debts (with origin) for {chat_id} and {payer_uid}"))
    }

    /// Moves tax debt `id`'s debt `magnitude` closer to zero. No principal/interest split to
    /// worry about here, unlike `P2PLoans::pay` - the whole thing is interest by construction
    /// (see the migration that created this table), so a plain subtraction is enough.
    pub async fn pay(&self, id: i32, magnitude: u16) -> anyhow::Result<()> {
        sqlx::query!("UPDATE Loan_Interest_Tax_Debts SET debt = debt - $2 WHERE id = $1",
                id, magnitude as i32)
            .execute(&self.pool)
            .await
            .map_err(Into::into)
            .and_then(ensure_only_one_row_updated)
            .context(format!("couldn't pay for the loan interest tax debt #{id}: {magnitude}"))
    }
}
