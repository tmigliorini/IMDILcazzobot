use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use async_trait::async_trait;
use derive_more::Display;
use downcast_rs::{Downcast, impl_downcast};
use num_traits::{PrimInt, Zero};
use rand::distributions::uniform::SampleUniform;
use rand::Rng;
use rand::rngs::OsRng;
use rust_i18n::t;
use teloxide::types::UserId;
use crate::{config, repo};
use crate::domain::LanguageCode;
use crate::handlers::debt_settlement;
use crate::repo::ChatIdKind;

#[derive(Clone)]
pub struct Incrementor {
    config: Config,
    perks: Vec<Arc<dyn Perk>>,
    dicks: repo::Dicks,
}

#[derive(Clone)]
pub struct Config {
    growth_range: RangeInclusive<i16>,
    grow_shrink_ratio: f32,
    newcomers_grace_days: u32,
    dod_bonus_range: RangeInclusive<u8>,
}

#[async_trait]
pub trait Perk: Send + Sync + Downcast {
    fn name(&self) -> &str;
    async fn apply(&self, dick_id: &DickId, change_intent: ChangeIntent) -> AdditionalChange;

    fn enabled(&self) -> bool {
        let env_key = format!("DISABLE_{}", self.name().to_uppercase().replace('-', "_"));
        !config::get_env_value_or_default(&env_key, false)
    }
}
impl_downcast!(Perk);

pub trait ConfigurablePerk: Perk {
    type Config;

    fn get_config(&self) -> Self::Config;
}

#[derive(Display, Clone, Hash, PartialEq)]
#[display("(user_id={_0}, chat_id={_1}")]
pub struct DickId(pub(crate) UserId, pub(crate) ChatIdKind);

#[derive(Copy, Clone)]
pub struct ChangeIntent {
    pub current_length: i32,
    pub base_increment: i32,
}

/// `value` is the perk's numeric contribution, as before; `details` are (recipient name, amount,
/// remaining-debt-to-that-recipient-afterwards) triples naming who else a transfer-like perk
/// (e.g. a P2P loan repayment) actually moved ghei to/from, beyond the generic "(perk-name)
/// (+/-value)" summary line - kept as raw data rather than a pre-formatted string since `apply`
/// has no access to the player's language (only `perks_part_of_answer`, further downstream,
/// does). The third element is `None` when "remaining debt to this specific name" isn't a
/// meaningful concept (e.g. a tax-debt redistribution recipient isn't a creditor - see
/// `debt_settlement::settle_gain_against_debts`). Most perks have nothing to add here and leave
/// it empty.
#[derive(Clone, Default)]
pub struct AdditionalChange {
    pub value: i32,
    pub details: Vec<(String, u16, Option<u16>)>,
}

impl AdditionalChange {
    pub fn new(value: i32) -> Self {
        Self { value, details: Vec::new() }
    }

    pub fn with_details(value: i32, details: Vec<(String, u16, Option<u16>)>) -> Self {
        Self { value, details }
    }
}

pub struct Increment<T: PrimInt + std::fmt::Display> {
    pub base: T,
    pub by_perks: HashMap<String, i32>,
    pub detail_lines: Vec<(String, u16, Option<u16>)>,
    pub total: T,
}

pub type SignedIncrement = Increment<i32>;
pub type UnsignedIncrement = Increment<u16>;

impl Config {
    pub fn growth_range_min(&self) -> i16 {
        self.growth_range.clone()
            .min()
            .unwrap_or(0)
    }

    pub fn growth_range_max(&self) -> i16 {
        self.growth_range.clone()
            .max()
            .unwrap_or(0)
    }
}

impl Incrementor {
    pub fn from_env(dicks: &repo::Dicks, perks: Vec<Box<dyn Perk>>) -> Self {
        let growth_range_min = config::get_env_value_or_default("GROWTH_MIN", -5);
        let growth_range_max = config::get_env_value_or_default("GROWTH_MAX", 10);
        let dod_max_bonus = config::get_env_value_or_default("GROWTH_DOD_BONUS_MAX", 5);
        
        let perks = perks
            .into_iter()
            .filter(|perk| perk.enabled())
            .map(Arc::from)
            .collect();

        Self {
            config: Config {
                growth_range: growth_range_min..=growth_range_max,
                grow_shrink_ratio: config::get_env_value_or_default("GROW_SHRINK_RATIO", 0.5),
                newcomers_grace_days: config::get_env_value_or_default("NEWCOMERS_GRACE_DAYS", 7),
                dod_bonus_range: 1..=dod_max_bonus,
            },
            perks,
            dicks: dicks.clone(),
        }
    }

    pub fn get_config(&self) -> Config {
        self.config.clone()
    }

    pub fn find_perk_config<P: ConfigurablePerk>(&self) -> Option<P::Config> {
        self.perks.iter()
            .find(|p| p.is::<P>())
            .and_then(|p| p.downcast_ref::<P>())
            .map(ConfigurablePerk::get_config)
    }

    #[cfg(test)]
    fn set_perks(&mut self, perks: Vec<Box<dyn Perk>>) {
        self.perks = perks.into_iter()
            .map(Arc::from)
            .collect();
    }

    pub async fn growth_increment(&self, user_id: UserId, chat_id: ChatIdKind, days_since_registration: u32) -> SignedIncrement {
        let dick_id = DickId(user_id, chat_id);
        let shrink_ratio = if days_since_registration > self.config.newcomers_grace_days {
            self.config.grow_shrink_ratio
        } else {
            0.0
        };
        let base_incr = get_base_increment(self.config.growth_range.clone(), shrink_ratio);
        self.add_additional_incr(dick_id, BaseIncrement(base_incr)).await
    }

    pub async fn dod_increment(&self, user_id: UserId, chat_id: ChatIdKind) -> UnsignedIncrement {
        let dick_id = DickId(user_id, chat_id);
        let base_incr = OsRng.gen_range(self.config.dod_bonus_range.clone());
        self.add_additional_incr(dick_id, BaseIncrement(base_incr)).await
    }
    
    async fn add_additional_incr<T, R>(&self, dick: DickId, base_increment: BaseIncrement<T>) -> Increment<R>
    where
        T: PrimInt + std::fmt::Display + Into<i16>,
        R: PrimInt + std::fmt::Display + From<T> + TryFrom<i32>,
        <R as TryFrom<i32>>::Error: std::fmt::Display
    {
        let current_length = match self.dicks.fetch_length(dick.0, &dick.1).await {
            Ok(length) => length,
            Err(e) => {
                log::error!("couldn't fetch the length of a dick: {e}");
                return base_increment.only()
            }
        };
        let change_intent = ChangeIntent {
            base_increment: base_increment.i32(),
            current_length
        };

        let mut additional_change = 0;
        let mut by_perks = HashMap::new();
        let mut detail_lines = Vec::new();
        for perk in self.perks.iter() {
            let AdditionalChange { value: ac, details } = perk.apply(&dick, change_intent).await;
            if !ac.is_zero() {
                by_perks.insert(perk.name().to_owned(), ac);
            }
            detail_lines.extend(details);
            additional_change += ac
        }

        let base = <R as From<T>>::from(base_increment.0);
        let total = change_intent.base_increment.checked_add(additional_change)
            .map(R::try_from)
            .and_then(Result::ok)
            .unwrap_or_else(|| {
                log::error!("overflow on increment calculation for {dick}: base={base}, additional={additional_change}");
                base
            });

        if base == total && !additional_change.is_zero() {
            log::info!("The following perks affected the calculation: {by_perks:?}");
            by_perks.clear();
            detail_lines.clear();
        }

        Increment { base, by_perks, detail_lines, total }
    }
}

#[derive(Copy, Clone)]
struct BaseIncrement<T: PrimInt + Copy + Into<i16>>(T);

impl <T: PrimInt + Into<i16>> BaseIncrement<T> {
    fn only<R>(self) -> Increment<R>
        where
            R: PrimInt + std::fmt::Display + From<T>
    {
        let value = <R as From<T>>::from(self.0);
        Increment {
            base: value,
            by_perks: HashMap::default(),
            detail_lines: Vec::new(),
            total: value
        }
    }

    fn i32(self) -> i32 {
        self.0.into() as i32
    }
}

impl <T: PrimInt + std::fmt::Display + Into<i32>> Increment<T> {
    pub fn perks_part_of_answer(&self, lang_code: &LanguageCode) -> String {
        if self.base == self.total {
            return String::default()
        }

        // `debt-payout` gets its own dedicated, hard-to-miss wording (shared with PVP/donations/
        // tax/promo - see `debt_settlement::format_withheld_message`) instead of being folded
        // into the generic "Perks:" block below as just another delta among others. It's the only
        // perk that ever populates `detail_lines`, so it's safe to attribute all of them to it.
        let debt_payout = self.by_perks.get("debt-payout").copied();
        let other_perks: Vec<_> = self.by_perks.iter()
            .filter(|(name, _)| name.as_str() != "debt-payout")
            .collect();

        let perks_block = if other_perks.is_empty() {
            String::default()
        } else {
            let top_line = t!("titles.perks.top_line", locale = lang_code);
            let perks = other_perks.iter()
                .map(|(perk, value)| {
                    let t_key = format!("titles.perks.{perk}");
                    let name = t!(&t_key, locale = lang_code);
                    format!("— {name} ({value:+})")
                })
                .collect::<Vec<String>>()
                .join("\n");
            format!("\n\n{top_line}:\n{perks}")
        };
        let withheld_block = match debt_payout {
            Some(value) if value < 0 => debt_settlement::format_withheld_message(-value, &self.detail_lines, lang_code),
            _ => String::default(),
        };
        format!("{perks_block}{withheld_block}")
    }
}

fn get_base_increment<T>(range: RangeInclusive<T>, shrink_ratio: f32) -> T
where
    T: PrimInt + PartialOrd + SampleUniform + From<i8>
{
    let shrink_ratio_percent = match (shrink_ratio * 100.0).round() as u32 {
        ..=0 => 0,
        100.. => 100,
        x => x
    };
    let mut rng = OsRng;
    let zero = <T as From<i8>>::from(0);
    if range.start() > &zero {
        return rng.gen_range(range)
    }
    let negative = rng.gen_ratio(shrink_ratio_percent, 100);
    if negative {
        let start = *range.start();
        let minus_one = <T as From<i8>>::from(-1);
        rng.gen_range(start..=minus_one)
    } else {
        let end = *range.end();
        let one = <T as From<i8>>::from(1);
        rng.gen_range(one..=end)
    }
}

#[cfg(test)]
mod test {
    use super::get_base_increment;

    #[test]
    fn test_gen_increment() {
        let increments: Vec<i32> = (0..100)
            .map(|_| get_base_increment(-5..=10, 0.5))
            .collect();
        assert!(increments.iter().any(|n| n > &0));
        assert!(increments.iter().any(|n| n < &0));
        assert!(increments.iter().all(|n| n != &0));
        assert!(increments.iter().all(|n| n <= &10));
        assert!(increments.iter().all(|n| n >= &-5));
    }

    #[test]
    fn test_gen_increment_shrink_ratio_direction() {
        let always_positive: Vec<i32> = (0..50).map(|_| get_base_increment(-5..=10, 0.0)).collect();
        assert!(always_positive.iter().all(|n| n > &0));

        let always_negative: Vec<i32> = (0..50).map(|_| get_base_increment(-5..=10, 1.0)).collect();
        assert!(always_negative.iter().all(|n| n < &0));
    }

    #[test]
    fn test_gen_increment_with_positive_range() {
        let increments: Vec<i32> = (0..100)
            .map(|_| get_base_increment(5..=10, 0.5))
            .collect();
        assert!(increments.iter().all(|n| n <= &10));
        assert!(increments.iter().all(|n| n >= &5));
    }
}

#[cfg(test)]
mod test_incrementor {
    use std::iter::zip;

    use async_trait::async_trait;
    use futures::future::join_all;

    use crate::handlers::utils::{AdditionalChange, ChangeIntent, Config, DickId, Incrementor, Perk};
    use crate::repo;
    use crate::repo::test::{CHAT_ID_KIND, start_postgres, USER_ID};

    #[tokio::test]
    async fn test_incrementor() {
        let (_container, db) = start_postgres().await;
        let dicks = repo::Dicks::new(db.clone(), Default::default());
        let incr = Incrementor {
            config: Config {
                growth_range: -1..=1,
                grow_shrink_ratio: 0.5,
                newcomers_grace_days: 1,
                dod_bonus_range: 1..=2,
            },
            dicks,
            perks: Vec::default()
        };

        test_growth_increment_base(&incr).await;
        test_dod_increment_base(&incr).await;
        test_with_perks(&incr).await;
        test_perk_with_overflow(&incr).await;
    }

    async fn test_growth_increment_base(incr: &Incrementor) {
        let lazy_vals = (0..100)
            .map(|_| incr.growth_increment(USER_ID, CHAT_ID_KIND, 1));
        for fut in lazy_vals {
            let val = fut.await;
            assert_eq!(val.base, val.total);
            assert_ne!(val.base, 0);
            assert!(val.base >= -1);
            assert!(val.base <= 1);
        }

        let lazy_positive_vals = (0..100)
            .map(|_| incr.growth_increment(USER_ID, CHAT_ID_KIND, 0));
        for fut in lazy_positive_vals {
            let val = fut.await;
            assert_eq!(val.base, val.total);
            assert!(val.base > 0);
        }
    }

    async fn test_dod_increment_base(incr: &Incrementor) {
        let val = (0..100)
            .map(|_| incr.dod_increment(USER_ID, CHAT_ID_KIND));
        let val = join_all(val).await;
        assert!(val.iter().all(|n| { n.base == n.total }));
        assert!(val.iter().all(|n| { n.base == 1 || n.base == 2 }))
    }

    #[derive(Clone)]
    struct AddPerk {
        value: i32,
        name: String,
    }

    impl AddPerk {
        fn boxed(value: i32) -> Box<Self> {
            Box::new(Self {
                value,
                name: format!("add-perk-{value}")
            })
        }
    }

    #[async_trait]
    impl Perk for AddPerk {
        fn name(&self) -> &str {
            &self.name
        }

        async fn apply(&self, _: &DickId, _: ChangeIntent) -> AdditionalChange {
            AdditionalChange::new(self.value)
        }

        fn enabled(&self) -> bool {
            true
        }
    }

    async fn test_with_perks(incr: &Incrementor) {
        let mut incr = incr.clone();
        let perk_plus2 = AddPerk::boxed(2);
        let perk_minus1 = AddPerk::boxed(-1);
        incr.set_perks(vec![perk_plus2.clone(), perk_minus1.clone()]);

        let growth_lazy_vals = (0..100)
            .map(|_| incr.growth_increment(USER_ID, CHAT_ID_KIND, 1));
        let dod_lazy_vals = (0..100)
            .map(|_| incr.dod_increment(USER_ID, CHAT_ID_KIND));

        macro_rules! assertions {
            ($val:ident) => {
                assert_eq!($val.total - $val.base, 1);
                assert_eq!($val.by_perks[perk_plus2.name()], 2);
                assert_eq!($val.by_perks[perk_minus1.name()], -1);
            };
        }

        for (growth_fut, dod_fut) in zip(growth_lazy_vals, dod_lazy_vals) {
            let (growth_val, dod_val) = (growth_fut.await, dod_fut.await);
            assertions!(growth_val);
            assertions!(dod_val);
        }
    }
    
    async fn test_perk_with_overflow(incr: &Incrementor) {
        let mut incr = incr.clone();
        let perk_add_max_int = AddPerk::boxed(i32::MAX);
        incr.set_perks(vec![perk_add_max_int.clone()]);
        
        let increment = incr.dod_increment(USER_ID, CHAT_ID_KIND).await;
        assert_eq!(increment.base, increment.total);
        assert!(increment.by_perks.is_empty());
    }
}
