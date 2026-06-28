use teloxide::types::UserId;
use crate::repo;
use crate::repo::test::dicks::{create_another_user_and_dick, create_user};
use crate::repo::test::{get_chat_id_and_dicks, start_postgres, NAME, UID, USER_ID};

#[tokio::test]
async fn test_tax_chat() {
    let (_container, db) = start_postgres().await;
    let (chat_id, dicks) = get_chat_id_and_dicks(&db);
    let chat_id_partiality = chat_id.clone().into();
    let tax = repo::TaxRepo::new(db.clone(), Default::default());

    create_user(&db).await;
    // a single create_or_grow call (the initial INSERT) rather than create_dick's length-0
    // INSERT followed by a second create_or_grow UPDATE - two writes on the same day trip the
    // real "already grown today" trigger, same as a player legitimately would.
    dicks.create_or_grow(USER_ID, &chat_id_partiality, 100)
        .await.expect("couldn't grow the first dick");
    create_another_user_and_dick(&db, &chat_id_partiality, 2, "second", 10).await;
    let second_uid = UserId((UID + 1) as u64);

    let deltas = [(USER_ID, -20), (second_uid, 20)];
    let applied = tax.tax_chat(&chat_id_partiality, &deltas)
        .await.expect("couldn't apply the tax");
    assert!(applied);
    assert!(tax.was_taxed_today(&chat_id).await.expect("couldn't check the tax log"));

    let top = dicks.get_top(&chat_id, 0, 2).await.expect("couldn't fetch the top");
    let first = top.iter().find(|d| d.owner_name == NAME).expect("the first dick is missing");
    assert_eq!(first.length, 80);
    let second = top.iter().find(|d| d.owner_name == "second").expect("the second dick is missing");
    assert_eq!(second.length, 30);

    // a second attempt on the same day must be a no-op
    let applied_again = tax.tax_chat(&chat_id_partiality, &[(USER_ID, -1000)])
        .await.expect("the second tax attempt shouldn't error");
    assert!(!applied_again);

    let top = dicks.get_top(&chat_id, 0, 2).await.expect("couldn't fetch the top again");
    let first = top.iter().find(|d| d.owner_name == NAME).expect("the first dick is missing");
    assert_eq!(first.length, 80, "the length must not change on a repeated same-day tax attempt");
}
