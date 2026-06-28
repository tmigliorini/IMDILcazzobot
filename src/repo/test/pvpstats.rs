use teloxide::prelude::{ChatId, UserId};
use crate::repo;
use crate::repo::{ChatIdKind, ChatIdPartiality, WinRateAware};
use crate::repo::test::dicks::{create_dick, create_user, create_user_and_dick_2};
use crate::repo::test::{CHAT_ID, start_postgres, UID};

#[tokio::test]
async fn test_all() {
    let (_container, db) = start_postgres().await;
    let pvp_stats = repo::BattleStatsRepo::new(db.clone(), Default::default());

    let chat_id = ChatIdKind::ID(ChatId(CHAT_ID));
    let bet = 42;

    // create user and dick #1
    create_user(&db).await;
    create_dick(&db).await;
    let uid_1 = UserId(UID as u64);
    // create user and dick #2
    create_user_and_dick_2(&db, &ChatIdPartiality::Specific(chat_id.clone()), "User-2").await;
    let uid_2 = UserId(UID as u64 + 1);
    
    // get stats when no rows
    let stats = pvp_stats.get_stats(&chat_id, uid_1).await
        .expect("couldn't fetch stats");
    assert_eq!(stats.battles_total, 0);
    assert_eq!(stats.battles_won, 0);
    assert_eq!(stats.win_streak_current, 0);
    assert_eq!(stats.win_streak_max, 0);
    assert_eq!(stats.lose_streak_current, 0);
    assert_eq!(stats.lose_streak_max, 0);
    assert_eq!(stats.win_rate_percentage(), 0.00);

    // send the first battle to check insertions: uid_1 beats uid_2.
    let stats = pvp_stats.send_battle_result(&chat_id, uid_1, uid_2, bet).await
        .expect("couldn't send result of the first battle");
    assert_eq!(stats.winner.battles_total, 1);
    assert_eq!(stats.winner.battles_won, 1);
    assert_eq!(stats.winner.win_streak_current, 1);
    assert_eq!(stats.winner.win_streak_max, 1);
    assert_eq!(stats.winner.prev_lose_streak, 0, "uid_1 never lost before, so there's no streak to snap");
    assert_eq!(stats.winner.win_rate_percentage(), 100.0);
    assert_eq!(stats.winner.win_rate_formatted(), "100.00%");
    assert_eq!(stats.loser.win_rate_percentage, 0.00);
    assert_eq!(stats.loser.prev_win_streak, 0);
    assert_eq!(stats.loser.lose_streak_current, 1, "uid_2's first loss starts a 1-battle lose streak");

    // send the second battle to check updates: uid_2 beats uid_1, snapping uid_1's win streak
    // and (separately) its own 1-battle lose streak from the first battle.
    let stats = pvp_stats.send_battle_result(&chat_id, uid_2, uid_1, bet).await
        .expect("couldn't send result of the first battle");
    assert_eq!(stats.winner.battles_total, 2);
    assert_eq!(stats.winner.battles_won, 1);
    assert_eq!(stats.winner.win_streak_current, 1);
    assert_eq!(stats.winner.win_streak_max, 1);
    assert_eq!(stats.winner.prev_lose_streak, 1, "uid_2's win must report the 1-battle lose streak it just snapped");
    assert_eq!(stats.winner.win_rate_percentage(), 50.0);
    assert_eq!(stats.winner.win_rate_formatted(), "50.00%");
    assert_eq!(stats.loser.win_rate_percentage, 50.0);
    assert_eq!(stats.loser.prev_win_streak, 1);
    assert_eq!(stats.loser.lose_streak_current, 1, "uid_1's first loss starts its own 1-battle lose streak");

    // send the third battle to test the getter again and check percentage rounding: uid_2 beats
    // uid_1 again, extending uid_1's lose streak to 2 (uid_2 itself never lost since, so its own
    // lose streak stays snapped at 0, with nothing left to report).
    let stats = pvp_stats.send_battle_result(&chat_id, uid_2, uid_1, bet).await
        .expect("couldn't send result of the third battle");
    assert_eq!(stats.winner.prev_lose_streak, 0, "uid_2 wasn't on a lose streak going into this battle");
    assert_eq!(stats.loser.lose_streak_current, 2, "uid_1's second loss in a row extends its lose streak to 2");

    let stats = pvp_stats.get_stats(&chat_id, uid_1).await
        .expect("couldn't fetch stats");
    assert_eq!(stats.battles_total, 3);
    assert_eq!(stats.battles_won, 1);
    assert_eq!(stats.win_rate_formatted(), "33.33%");
    assert_eq!(stats.acquired_length, bet as u32);
    assert_eq!(stats.lost_length, bet as u32 * 2);
    assert_eq!(stats.lose_streak_current, 2);
    assert_eq!(stats.lose_streak_max, 2);
}
