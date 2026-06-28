use teloxide::types::UserId;
use crate::{config, repo};
use crate::repo::test::dicks::{create_another_user_and_dick, create_dick, create_user};
use crate::repo::test::{get_chat_id_and_dicks, CHAT_ID_KIND, UID, USER_ID};

/// A loan's interest tax is no longer withheld immediately at acceptance time - it becomes its
/// own gradual debt instead (see `repo::P2PLoans::lend`'s `interest_loan_id` and
/// `crate::handlers::p2p_loan::p2p_loan_impl_accept`, which creates this row). This test only
/// exercises the new table directly; the actual gradual collection/redistribution is wired up by
/// `crate::handlers::debt_settlement` (see its own tests).
#[tokio::test]
async fn test_tax_debt_is_created_and_drained_independently_of_the_loan_it_came_from() {
    let (_container, db) = crate::repo::test::start_postgres().await;
    let (chat_id, _dicks) = get_chat_id_and_dicks(&db);
    let chat_id_partiality: repo::ChatIdPartiality = chat_id.clone().into();

    create_user(&db).await;
    create_dick(&db).await; // UID - lender
    create_another_user_and_dick(&db, &chat_id_partiality, 2, "borrower", 0).await;
    let lender_uid = USER_ID;
    let borrower_uid = UserId((UID + 1) as u64);

    let cfg = config::AppConfig {
        p2p_loan_payout_ratio: 0.5,
        p2p_loan_interest_tax_rate: 0.26,
        ..Default::default()
    };
    let loans = repo::P2PLoans::new(db.clone(), &cfg);
    let tax_debts = repo::LoanInterestTaxDebts::new(db.clone(), &cfg);

    // principal = 100, rate = 10% -> interest = 10; tax = round(10 * 0.26) = 3.
    let (_, _, interest, tax, interest_loan_id) = loans.lend(&chat_id_partiality, lender_uid, borrower_uid, 100, Some(0.1)).await
        .expect("couldn't create the loan");
    assert_eq!((interest, tax), (10, 3));

    // nothing should exist yet for the borrower (they don't realize the interest on a
    // positive-rate loan) - only the lender does.
    let borrower_tax_debts = tax_debts.get_active(borrower_uid, &CHAT_ID_KIND).await.expect("couldn't fetch borrower's tax debts");
    assert!(borrower_tax_debts.is_empty());

    tax_debts.create(&chat_id_partiality, lender_uid, tax, Some(interest_loan_id)).await
        .expect("couldn't create the tax debt");

    let lender_tax_debts = tax_debts.get_active(lender_uid, &CHAT_ID_KIND).await.expect("couldn't fetch lender's tax debts");
    assert_eq!(lender_tax_debts.len(), 1);
    assert_eq!(lender_tax_debts[0].amount_owed, 3);
    // shares the same configured ratio as P2P loans (see the struct doc on `TaxDebtObligation`).
    assert_eq!(lender_tax_debts[0].payout_ratio, 0.5);

    // draining it to zero must close the obligation, exactly like a P2P loan or bank loan
    // (enforced by the trigger from the migration that created this table).
    tax_debts.pay(lender_tax_debts[0].id, 3).await.expect("couldn't pay off the tax debt");
    let lender_tax_debts = tax_debts.get_active(lender_uid, &CHAT_ID_KIND).await.expect("couldn't re-fetch lender's tax debts");
    assert!(lender_tax_debts.is_empty(), "a fully repaid tax debt must no longer be active");

    // the loan itself is a completely separate obligation, untouched by paying off the tax debt.
    let lender_loan_obligations = loans.get_active_loans(borrower_uid, &CHAT_ID_KIND).await.expect("couldn't fetch borrower's loan obligations");
    assert_eq!(lender_loan_obligations[0].amount_owed, 110);
}
