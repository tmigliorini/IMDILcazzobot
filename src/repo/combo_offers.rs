use anyhow::Context;
use rand::Rng;
use rand::rngs::OsRng;
use sqlx::Postgres;
use teloxide::types::UserId;

/// One leg of a combo offer - whichever single-offer query it happened to parse as. Not tied to
/// any particular single-offer type's own wire format (a combo's two legs together don't fit in
/// Telegram's 64-byte callback_data limit, hence this table in the first place) - just the
/// handful of fields each settlement function (`pvp::pvp_core_in_tx`, `donate::donate_core_in_tx`,
/// `p2p_loan::p2p_loan_core_in_tx`) actually needs.
#[derive(Clone, Debug, PartialEq)]
pub enum ComboLeg {
    Pvp { bet: u16, probability_pct: Option<f64> },
    Donate { amount: i32 },
    P2PLoan { amount: i32, interest_rate_pct: Option<f64> },
}

#[derive(Clone, Debug)]
pub struct ComboOffer {
    pub proposer: UserId,
    pub target: Option<UserId>,
    pub leg1: ComboLeg,
    pub leg2: ComboLeg,
}

impl ComboOffer {
    pub fn new(proposer: UserId, target: Option<UserId>, leg1: ComboLeg, leg2: ComboLeg) -> Self {
        Self { proposer, target, leg1, leg2 }
    }
}

pub enum AcceptOutcome {
    NotFound,
    SamePerson,
    WrongTarget,
    Accepted(ComboOffer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "combo_offer_leg_kind")]
enum ComboLegKind {
    #[sqlx(rename = "pvp")] Pvp,
    #[sqlx(rename = "donate")] Donate,
    #[sqlx(rename = "p2ploan")] P2PLoan,
}

fn leg_to_row(leg: &ComboLeg) -> (ComboLegKind, i32, Option<f64>) {
    match *leg {
        ComboLeg::Pvp { bet, probability_pct } => (ComboLegKind::Pvp, bet as i32, probability_pct),
        ComboLeg::Donate { amount } => (ComboLegKind::Donate, amount, None),
        ComboLeg::P2PLoan { amount, interest_rate_pct } => (ComboLegKind::P2PLoan, amount, interest_rate_pct),
    }
}

fn leg_from_row(kind: ComboLegKind, amount: i32, rate_pct: Option<f64>) -> ComboLeg {
    match kind {
        ComboLegKind::Pvp => ComboLeg::Pvp { bet: amount.max(0) as u16, probability_pct: rate_pct },
        ComboLegKind::Donate => ComboLeg::Donate { amount },
        ComboLegKind::P2PLoan => ComboLeg::P2PLoan { amount, interest_rate_pct: rate_pct },
    }
}

struct ComboOfferRow {
    proposer_uid: i64,
    target_uid: Option<i64>,
    leg1_kind: ComboLegKind,
    leg1_amount: i32,
    leg1_rate_pct: Option<f64>,
    leg2_kind: ComboLegKind,
    leg2_amount: i32,
    leg2_rate_pct: Option<f64>,
}

impl ComboOfferRow {
    fn into_offer(self) -> ComboOffer {
        ComboOffer {
            proposer: UserId(self.proposer_uid as u64),
            target: self.target_uid.map(|t| UserId(t as u64)),
            leg1: leg_from_row(self.leg1_kind, self.leg1_amount, self.leg1_rate_pct),
            leg2: leg_from_row(self.leg2_kind, self.leg2_amount, self.leg2_rate_pct),
        }
    }
}

/// How long an unresolved combo offer is kept around before `insert`'s lazy sweep evicts it.
/// Generous on purpose - this only guards against truly abandoned offers, not normal usage.
const MAX_PENDING_AGE_INTERVAL: &str = "24 hours";

/// Persists pending combo offers in `ComboOffers`, so unlike every other kind of in-memory,
/// per-process state in this codebase (`crate::handlers::utils::details_store`,
/// `crate::handlers::utils::wizard_store`), a bot restart no longer silently invalidates one
/// that's still pending - those two are fine to lose (a details button or a half-filled wizard
/// session are cheap to redo), but a fully-specified two-leg offer someone else might still
/// accept is not.
#[derive(Clone)]
pub struct ComboOffers {
    pool: sqlx::Pool<Postgres>,
}

impl ComboOffers {
    pub fn new(pool: sqlx::Pool<Postgres>) -> Self {
        Self { pool }
    }

    /// Stores `offer` under a fresh token and returns it (to be embedded in the combo's
    /// callback_data). Opportunistically sweeps out anything older than `MAX_PENDING_AGE_INTERVAL`
    /// first, so abandoned offers don't accumulate forever without needing a background task.
    pub async fn insert(&self, offer: &ComboOffer) -> anyhow::Result<String> {
        sqlx::query(&format!("DELETE FROM ComboOffers WHERE created_at < current_timestamp - interval '{MAX_PENDING_AGE_INTERVAL}'"))
            .execute(&self.pool)
            .await
            .context("couldn't sweep expired combo offers")?;

        let token = format!("{:016x}", OsRng.gen::<u64>());
        let (leg1_kind, leg1_amount, leg1_rate_pct) = leg_to_row(&offer.leg1);
        let (leg2_kind, leg2_amount, leg2_rate_pct) = leg_to_row(&offer.leg2);
        sqlx::query!(
            "INSERT INTO ComboOffers (token, proposer_uid, target_uid, leg1_kind, leg1_amount, leg1_rate_pct, leg2_kind, leg2_amount, leg2_rate_pct)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            token, offer.proposer.0 as i64, offer.target.map(|t| t.0 as i64),
            leg1_kind as ComboLegKind, leg1_amount, leg1_rate_pct,
            leg2_kind as ComboLegKind, leg2_amount, leg2_rate_pct)
            .execute(&self.pool)
            .await
            .context("couldn't insert a combo offer")?;
        Ok(token)
    }

    /// Atomically checks `acceptor` against the stored offer and, only on a genuine accept,
    /// removes it - a same-person or wrong-target click leaves the row untouched, since the
    /// right person may still come along and accept it later. `NotFound` also covers "already
    /// accepted/expired by someone else a moment ago" - same ambiguity the in-memory version this
    /// replaced had, since by the time we'd know which it was, it's already gone either way.
    pub async fn try_accept(&self, token: &str, acceptor: UserId) -> anyhow::Result<AcceptOutcome> {
        let acceptor_id = acceptor.0 as i64;
        let deleted = sqlx::query_as!(ComboOfferRow,
            r#"DELETE FROM ComboOffers WHERE token = $1 AND proposer_uid != $2 AND (target_uid IS NULL OR target_uid = $2)
                RETURNING proposer_uid, target_uid,
                          leg1_kind as "leg1_kind: ComboLegKind", leg1_amount, leg1_rate_pct,
                          leg2_kind as "leg2_kind: ComboLegKind", leg2_amount, leg2_rate_pct"#,
            token, acceptor_id)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't try to accept the combo offer {token}"))?;
        if let Some(row) = deleted {
            return Ok(AcceptOutcome::Accepted(row.into_offer()));
        }

        // the delete's own WHERE clause didn't match anything - find out why, without consuming
        // the row (it may still be legitimately pending for someone else).
        let existing = sqlx::query_as!(ComboOfferRow,
            r#"SELECT proposer_uid, target_uid,
                      leg1_kind as "leg1_kind: ComboLegKind", leg1_amount, leg1_rate_pct,
                      leg2_kind as "leg2_kind: ComboLegKind", leg2_amount, leg2_rate_pct
                FROM ComboOffers WHERE token = $1"#,
            token)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't look up the combo offer {token}"))?;
        Ok(match existing {
            None => AcceptOutcome::NotFound,
            Some(row) if row.proposer_uid == acceptor_id => AcceptOutcome::SamePerson,
            Some(_) => AcceptOutcome::WrongTarget,
        })
    }

    /// Same atomic check-then-remove as `try_accept`, but for the proposer retracting their own
    /// offer. Returns `None` (leaving the row untouched) if it's already gone or `proposer`
    /// isn't who created it.
    pub async fn try_cancel(&self, token: &str, proposer: UserId) -> anyhow::Result<Option<ComboOffer>> {
        let row = sqlx::query_as!(ComboOfferRow,
            r#"DELETE FROM ComboOffers WHERE token = $1 AND proposer_uid = $2
                RETURNING proposer_uid, target_uid,
                          leg1_kind as "leg1_kind: ComboLegKind", leg1_amount, leg1_rate_pct,
                          leg2_kind as "leg2_kind: ComboLegKind", leg2_amount, leg2_rate_pct"#,
            token, proposer.0 as i64)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't try to cancel the combo offer {token}"))?;
        Ok(row.map(ComboOfferRow::into_offer))
    }

    /// Same atomic check-then-remove as `try_cancel`, but for the target explicitly declining a
    /// *targeted* offer (an open one has no single target who could reject it this way).
    pub async fn try_reject(&self, token: &str, target: UserId) -> anyhow::Result<Option<ComboOffer>> {
        let row = sqlx::query_as!(ComboOfferRow,
            r#"DELETE FROM ComboOffers WHERE token = $1 AND target_uid = $2
                RETURNING proposer_uid, target_uid,
                          leg1_kind as "leg1_kind: ComboLegKind", leg1_amount, leg1_rate_pct,
                          leg2_kind as "leg2_kind: ComboLegKind", leg2_amount, leg2_rate_pct"#,
            token, target.0 as i64)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't try to reject the combo offer {token}"))?;
        Ok(row.map(ComboOfferRow::into_offer))
    }
}
