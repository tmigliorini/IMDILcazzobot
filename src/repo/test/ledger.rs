use teloxide::types::UserId;
use crate::repo;
use crate::repo::{ChatIdPartiality, LedgerCategory};
use crate::repo::test::{start_postgres, CHAT_ID_KIND, UID, USER_ID};
use crate::repo::test::dicks::create_user;

#[tokio::test]
async fn test_breakdown_per_category() {
    let (_container, db) = start_postgres().await;
    create_user(&db).await;
    let other_uid = UserId(UID as u64 + 1);
    create_other_user(&db, other_uid).await;
    let chat_id: ChatIdPartiality = CHAT_ID_KIND.into();

    let ledger = repo::Ledger::new(db.clone(), Default::default());

    // no activity yet - an empty breakdown, not an error
    let breakdown = ledger.get_breakdown(&chat_id, USER_ID).await.expect("couldn't get an empty breakdown");
    assert!(breakdown.is_empty());

    ledger.record(&chat_id, USER_ID, LedgerCategory::Grow, 5, None).await.expect("couldn't record a grow");
    ledger.record(&chat_id, USER_ID, LedgerCategory::Grow, -2, None).await.expect("couldn't record a shrink");
    ledger.record_many(&chat_id, LedgerCategory::Pvp, &[(USER_ID, 10, Some(other_uid)), (other_uid, -10, Some(USER_ID))]).await.expect("couldn't record a battle");
    // a zero amount is a no-op, not an error, and shouldn't show up in the breakdown
    ledger.record(&chat_id, USER_ID, LedgerCategory::Donate, 0, None).await.expect("a zero amount must be a no-op");

    let breakdown = ledger.get_breakdown(&chat_id, USER_ID).await.expect("couldn't get the breakdown");
    assert_eq!(breakdown.len(), 2); // only grow and pvp have any entries

    let grow = breakdown.iter().find(|b| b.category == LedgerCategory::Grow).expect("grow entry must be present");
    assert_eq!(grow.dare, 2);
    assert_eq!(grow.avere, 5);

    let pvp = breakdown.iter().find(|b| b.category == LedgerCategory::Pvp).expect("pvp entry must be present");
    assert_eq!(pvp.dare, 0);
    assert_eq!(pvp.avere, 10);

    let other_breakdown = ledger.get_breakdown(&chat_id, other_uid).await.expect("couldn't get the other user's breakdown");
    let other_pvp = other_breakdown.iter().find(|b| b.category == LedgerCategory::Pvp).expect("pvp entry must be present");
    assert_eq!(other_pvp.dare, 10);
    assert_eq!(other_pvp.avere, 0);
}

#[tokio::test]
async fn test_chat_breakdown_sums_across_every_player() {
    let (_container, db) = start_postgres().await;
    create_user(&db).await;
    let other_uid = UserId(UID as u64 + 1);
    create_other_user(&db, other_uid).await;
    let chat_id: ChatIdPartiality = CHAT_ID_KIND.into();

    let ledger = repo::Ledger::new(db.clone(), Default::default());

    let breakdown = ledger.get_chat_breakdown(&chat_id).await.expect("couldn't get an empty chat-wide breakdown");
    assert!(breakdown.is_empty());

    ledger.record(&chat_id, USER_ID, LedgerCategory::Grow, 5, None).await.expect("couldn't record a grow");
    ledger.record(&chat_id, other_uid, LedgerCategory::Grow, -2, None).await.expect("couldn't record a shrink");
    // a battle's two sides land in the same category, for two different players - the chat-wide
    // view must sum both into one row, unlike `get_breakdown`'s per-player one.
    ledger.record_many(&chat_id, LedgerCategory::Pvp, &[(USER_ID, 10, Some(other_uid)), (other_uid, -10, Some(USER_ID))]).await.expect("couldn't record a battle");

    let breakdown = ledger.get_chat_breakdown(&chat_id).await.expect("couldn't get the chat-wide breakdown");
    assert_eq!(breakdown.len(), 2); // only grow and pvp have any entries

    let grow = breakdown.iter().find(|b| b.category == LedgerCategory::Grow).expect("grow entry must be present");
    assert_eq!(grow.dare, 2); // other_uid's shrink
    assert_eq!(grow.avere, 5); // USER_ID's grow

    let pvp = breakdown.iter().find(|b| b.category == LedgerCategory::Pvp).expect("pvp entry must be present");
    assert_eq!(pvp.dare, 10); // other_uid's loss
    assert_eq!(pvp.avere, 10); // USER_ID's win
}

async fn create_other_user(db: &sqlx::Pool<sqlx::Postgres>, uid: UserId) {
    let users = repo::Users::new(db.clone());
    users.create_or_update(uid, "other")
        .await.expect("couldn't create the other user");
}

#[tokio::test]
async fn test_get_page() {
    let (_container, db) = start_postgres().await;
    create_user(&db).await;
    let other_uid = UserId(UID as u64 + 1);
    create_other_user(&db, other_uid).await;
    let chat_id: ChatIdPartiality = CHAT_ID_KIND.into();

    let ledger = repo::Ledger::new(db.clone(), Default::default());

    ledger.record(&chat_id, USER_ID, LedgerCategory::Grow, 5, None).await.expect("couldn't record a grow");
    ledger.record(&chat_id, USER_ID, LedgerCategory::Pvp, -3, Some(other_uid)).await.expect("couldn't record a battle loss");
    ledger.record(&chat_id, USER_ID, LedgerCategory::Donate, 7, Some(other_uid)).await.expect("couldn't record a donation");

    // fetch one extra row, like `dick::top_impl` does, to learn there's no further page.
    let page = ledger.get_page(&chat_id, USER_ID, 0, 10).await.expect("couldn't get a ledger page");
    assert_eq!(page.len(), 3);

    // newest first
    assert_eq!(page[0].category, LedgerCategory::Donate);
    assert_eq!(page[0].amount, 7);
    let (counterparty_uid, counterparty_name) = page[0].counterparty.as_ref().expect("the donation must have a counterparty");
    assert_eq!(*counterparty_uid, other_uid);
    assert_eq!(counterparty_name, "other");

    assert_eq!(page[1].category, LedgerCategory::Pvp);
    assert_eq!(page[1].amount, -3);
    assert!(page[1].counterparty.is_some());

    assert_eq!(page[2].category, LedgerCategory::Grow);
    assert_eq!(page[2].amount, 5);
    assert!(page[2].counterparty.is_none(), "a grow event has no counterparty");

    // pagination: page size 2, second page has only the oldest (3rd) entry
    let first_page = ledger.get_page(&chat_id, USER_ID, 0, 2).await.expect("couldn't get the first page");
    assert_eq!(first_page.len(), 2);
    let second_page = ledger.get_page(&chat_id, USER_ID, 2, 2).await.expect("couldn't get the second page");
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0].category, LedgerCategory::Grow);
}
