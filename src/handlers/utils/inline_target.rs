#[derive(Debug, PartialEq)]
pub struct PvpQuery {
    pub amount: u16,
    /// an explicit win-probability for the initiator (e.g. "90%" -> Some(90.0), "0,0025%" ->
    /// Some(0.0025) - both comma and dot work as the decimal separator), overriding the default
    /// 50/50 (or skill-based, if that toggle is enabled) odds when present.
    pub probability_pct: Option<f64>,
    pub target_name: Option<String>,
}

/// Parses a percentage value accepting either a comma or a dot as the decimal separator (e.g.
/// "25", "25.5", "25,5", "0,0025", ",5" are all valid), to be lenient with Italian input.
fn parse_percentage(s: &str) -> Option<f64> {
    s.replace(',', ".").parse().ok()
}

/// Parses "<amount>" or "<amount> <name...>" out of free-form inline query text. Generic so
/// donate (which also accepts negative amounts, for its "pull"/request mode) can parse an `i32`
/// while keeping the type unconstrained for any other future caller.
fn parse_amount_and_target<T: std::str::FromStr>(text: &str) -> Option<(T, Option<String>)> {
    let text = text.trim();
    let (amount_str, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let amount: T = amount_str.parse().ok()?;
    let name = rest.trim();
    let name = if name.is_empty() { None } else { Some(name.to_owned()) };
    Some((amount, name))
}

/// Parses "<amount> [<percentage>%] [<name...>]" out of free-form inline query text. Generic so
/// both pvp (u16, win probability) and p2p loans (i32, negative = pull, interest rate) can reuse
/// it for their respective trailing percentage.
fn parse_amount_percentage_and_target<T: std::str::FromStr>(text: &str) -> Option<(T, Option<f64>, Option<String>)> {
    let text = text.trim();
    let (amount_str, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let amount: T = amount_str.parse().ok()?;
    let rest = rest.trim_start();

    let (percentage, rest) = match rest.split_once(char::is_whitespace) {
        Some((maybe_pct, after)) if maybe_pct.ends_with('%') => {
            (parse_percentage(maybe_pct.trim_end_matches('%')), after)
        },
        None if !rest.is_empty() && rest.ends_with('%') => {
            (parse_percentage(rest.trim_end_matches('%')), "")
        },
        _ => (None, rest)
    };

    let target_name = rest.trim();
    let target_name = if target_name.is_empty() { None } else { Some(target_name.to_owned()) };
    Some((amount, percentage, target_name))
}

/// Parses "<amount> [<probability>%] [<name...>]" out of free-form inline query text.
fn parse_amount_probability_and_target(text: &str) -> Option<PvpQuery> {
    let (amount, probability_pct, target_name) = parse_amount_percentage_and_target(text)?;
    Some(PvpQuery { amount, probability_pct, target_name })
}

/// Strips a leading keyword (case-insensitively) followed by whitespace or end-of-string,
/// returning the remainder. ASCII-only keywords are assumed.
fn strip_keyword_prefix<'a>(text: &'a str, keywords: &[&str]) -> Option<&'a str> {
    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    keywords.iter()
        .find(|kw| lower.starts_with(*kw))
        .and_then(|kw| {
            let after = &trimmed[kw.len()..];
            (after.is_empty() || after.starts_with(char::is_whitespace))
                .then(|| after.trim_start())
        })
}

/// Bare "<amount>", optionally followed by "<probability>%" and/or "<name>" (an optional "pvp"
/// keyword prefix is also accepted, kept for symmetry with [`parse_donate_inline_query`]).
/// The bare-amount form is reserved for PVP for backward compatibility with the pre-existing
/// inline shortcut.
pub fn parse_pvp_inline_query(text: &str) -> Option<PvpQuery> {
    let rest = strip_keyword_prefix(text, &["pvp"]).unwrap_or_else(|| text.trim());
    parse_amount_probability_and_target(rest)
}

/// Requires a "dona" keyword prefix, since a bare amount is already claimed by PVP. Only one
/// keyword is accepted on purpose (no "donate" synonym) to keep the syntax unambiguous.
/// A negative amount (e.g. "dona -10 Mario") means a "pull": a request instead of a gift.
pub fn parse_donate_inline_query(text: &str) -> Option<(i32, Option<String>)> {
    let rest = strip_keyword_prefix(text, &["dona"])?;
    parse_amount_and_target(rest)
}

#[derive(Debug, PartialEq)]
pub struct P2PLoanQuery {
    /// a negative amount means a "pull": a request to BORROW, not to lend - the proposer
    /// becomes the borrower once someone accepts (mirrors donate's pull).
    pub amount: i32,
    /// an explicit interest rate (e.g. "40%" -> Some(40.0)), overriding the configured default
    /// either way (lending or requesting).
    pub interest_rate_pct: Option<f64>,
    pub target_name: Option<String>,
}

/// Requires a "presta" keyword prefix, for the same reason "dona" is required for donate.
pub fn parse_p2p_loan_inline_query(text: &str) -> Option<P2PLoanQuery> {
    let rest = strip_keyword_prefix(text, &["presta"])?;
    let (amount, interest_rate_pct, target_name) = parse_amount_percentage_and_target(rest)?;
    Some(P2PLoanQuery { amount, interest_rate_pct, target_name })
}

/// One leg of a combo offer - whichever single-offer query it happened to parse as.
#[derive(Debug, PartialEq)]
pub enum ComboLegQuery {
    Pvp(PvpQuery),
    Donate { amount: i32, target_name: Option<String> },
    P2PLoan(P2PLoanQuery),
}

impl ComboLegQuery {
    pub fn target_name(&self) -> Option<&str> {
        match self {
            ComboLegQuery::Pvp(q) => q.target_name.as_deref(),
            ComboLegQuery::Donate { target_name, .. } => target_name.as_deref(),
            ComboLegQuery::P2PLoan(q) => q.target_name.as_deref(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ComboQuery {
    pub leg1: ComboLegQuery,
    pub leg2: ComboLegQuery,
}

/// Tries each single-offer parser in turn, keeping whichever one matches. Unambiguous: `dona`/
/// `presta` each require their own keyword, and pvp's bare-number fallback only matches when the
/// leading token is actually numeric, so a keyworded leg can never be mis-parsed as pvp.
fn parse_combo_leg(text: &str) -> Option<ComboLegQuery> {
    parse_donate_inline_query(text).map(|(amount, target_name)| ComboLegQuery::Donate { amount, target_name })
        .or_else(|| parse_p2p_loan_inline_query(text).map(ComboLegQuery::P2PLoan))
        .or_else(|| parse_pvp_inline_query(text).map(ComboLegQuery::Pvp))
}

/// Parses "<leg1> combo <leg2>" out of free-form inline query text: splits on a standalone,
/// case-insensitive "combo" keyword (rejecting it if missing, or ambiguous - i.e. appearing more
/// than once), then parses each half as its own single-offer query. A target name, if any, is
/// expected just once, wherever either leg's own syntax naturally ends with it - reconciling the
/// two legs' target names (when both are given) is the caller's job, not this parser's.
pub fn parse_combo_inline_query(text: &str) -> Option<ComboQuery> {
    let lower = text.to_ascii_lowercase();
    let mut matches = lower.match_indices(" combo ");
    let (idx, matched) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let leg1 = parse_combo_leg(text[..idx].trim())?;
    let leg2 = parse_combo_leg(text[idx + matched.len()..].trim())?;
    Some(ComboQuery { leg1, leg2 })
}

#[cfg(test)]
mod test {
    use super::*;

    fn pvp_query(amount: u16, probability_pct: Option<f64>, target_name: Option<&str>) -> Option<PvpQuery> {
        Some(PvpQuery { amount, probability_pct, target_name: target_name.map(str::to_owned) })
    }

    #[test]
    fn pvp_bare_amount() {
        assert_eq!(parse_pvp_inline_query("10"), pvp_query(10, None, None));
        assert_eq!(parse_pvp_inline_query("  10  "), pvp_query(10, None, None));
    }

    #[test]
    fn pvp_amount_and_name() {
        assert_eq!(parse_pvp_inline_query("10 Mario"), pvp_query(10, None, Some("Mario")));
        assert_eq!(parse_pvp_inline_query("10   Simone Santuari"), pvp_query(10, None, Some("Simone Santuari")));
    }

    #[test]
    fn pvp_amount_and_probability() {
        assert_eq!(parse_pvp_inline_query("10 90%"), pvp_query(10, Some(90.0), None));
        assert_eq!(parse_pvp_inline_query("10 10%"), pvp_query(10, Some(10.0), None));
    }

    #[test]
    fn pvp_amount_probability_and_name() {
        assert_eq!(parse_pvp_inline_query("10 90% Mario"), pvp_query(10, Some(90.0), Some("Mario")));
        assert_eq!(parse_pvp_inline_query("10 90%   Simone Santuari"), pvp_query(10, Some(90.0), Some("Simone Santuari")));
    }

    #[test]
    fn pvp_keyword_prefix_accepted() {
        assert_eq!(parse_pvp_inline_query("pvp 10 Mario"), pvp_query(10, None, Some("Mario")));
        assert_eq!(parse_pvp_inline_query("PVP 10"), pvp_query(10, None, None));
        assert_eq!(parse_pvp_inline_query("pvp 10 90% Mario"), pvp_query(10, Some(90.0), Some("Mario")));
    }

    #[test]
    fn pvp_invalid() {
        assert_eq!(parse_pvp_inline_query(""), None);
        assert_eq!(parse_pvp_inline_query("Mario"), None);
        assert_eq!(parse_pvp_inline_query("donate 10 Mario"), None);
    }

    #[test]
    fn pvp_out_of_range_probability_is_unparsed_not_rejected() {
        // the raw percentage number is kept as-is; range validation (must be strictly between
        // 0 and 100) is the handler's responsibility, so it can answer with a clear error
        // instead of silently falling through to the generic inline listone.
        assert_eq!(parse_pvp_inline_query("10 150%"), pvp_query(10, Some(150.0), None));
        assert_eq!(parse_pvp_inline_query("10 0%"), pvp_query(10, Some(0.0), None));
    }

    #[test]
    fn pvp_probability_accepts_comma_and_up_to_four_decimals() {
        assert_eq!(parse_pvp_inline_query("10 0,0025%"), pvp_query(10, Some(0.0025), None));
        assert_eq!(parse_pvp_inline_query("10 0.0025%"), pvp_query(10, Some(0.0025), None));
        assert_eq!(parse_pvp_inline_query("10 ,5%"), pvp_query(10, Some(0.5), None));
        assert_eq!(parse_pvp_inline_query("10 33,3333% Mario"), pvp_query(10, Some(33.3333), Some("Mario")));
    }

    #[test]
    fn donate_requires_keyword() {
        assert_eq!(parse_donate_inline_query("10"), None);
        assert_eq!(parse_donate_inline_query("10 Mario"), None);
    }

    #[test]
    fn donate_with_keyword() {
        assert_eq!(parse_donate_inline_query("dona 10"), Some((10, None)));
        assert_eq!(parse_donate_inline_query("dona 10 Mario"), Some((10, Some("Mario".to_owned()))));
        assert_eq!(parse_donate_inline_query("DONA 10 Mario"), Some((10, Some("Mario".to_owned()))));
    }

    #[test]
    fn donate_no_longer_accepts_the_english_synonym() {
        // only "dona" is accepted now, on purpose, to keep a single unambiguous keyword
        assert_eq!(parse_donate_inline_query("donate 10"), None);
    }

    #[test]
    fn keyword_must_be_a_whole_word() {
        // "donate10" shouldn't match the "dona" keyword prefix as if it were "dona te10"
        assert_eq!(parse_donate_inline_query("donatello 10"), None);
    }

    #[test]
    fn donate_negative_amount_is_a_pull() {
        assert_eq!(parse_donate_inline_query("dona -10"), Some((-10, None)));
        assert_eq!(parse_donate_inline_query("dona -10 Mario"), Some((-10, Some("Mario".to_owned()))));
    }

    fn loan_query(amount: i32, interest_rate_pct: Option<f64>, target_name: Option<&str>) -> Option<P2PLoanQuery> {
        Some(P2PLoanQuery { amount, interest_rate_pct, target_name: target_name.map(str::to_owned) })
    }

    #[test]
    fn p2p_loan_requires_keyword() {
        assert_eq!(parse_p2p_loan_inline_query("10"), None);
        assert_eq!(parse_p2p_loan_inline_query("presta 10"), loan_query(10, None, None));
        assert_eq!(parse_p2p_loan_inline_query("presta 10 Mario"), loan_query(10, None, Some("Mario")));
        assert_eq!(parse_p2p_loan_inline_query("PRESTA 10 Mario"), loan_query(10, None, Some("Mario")));
    }

    #[test]
    fn p2p_loan_custom_rate() {
        assert_eq!(parse_p2p_loan_inline_query("presta 50 40%"), loan_query(50, Some(40.0), None));
        assert_eq!(parse_p2p_loan_inline_query("presta 50 40% Mario"), loan_query(50, Some(40.0), Some("Mario")));
    }

    #[test]
    fn p2p_loan_negative_amount_is_a_pull() {
        assert_eq!(parse_p2p_loan_inline_query("presta -50"), loan_query(-50, None, None));
        assert_eq!(parse_p2p_loan_inline_query("presta -50 40%"), loan_query(-50, Some(40.0), None));
        assert_eq!(parse_p2p_loan_inline_query("presta -50 40% Mario"), loan_query(-50, Some(40.0), Some("Mario")));
        assert_eq!(parse_p2p_loan_inline_query("presta -50 Mario"), loan_query(-50, None, Some("Mario")));
    }

    fn combo_query(leg1: ComboLegQuery, leg2: ComboLegQuery) -> Option<ComboQuery> {
        Some(ComboQuery { leg1, leg2 })
    }

    #[test]
    fn combo_pvp_and_presta() {
        assert_eq!(
            parse_combo_inline_query("50 15% combo presta 100 30% Tommaso"),
            combo_query(
                ComboLegQuery::Pvp(PvpQuery { amount: 50, probability_pct: Some(15.0), target_name: None }),
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 100, interest_rate_pct: Some(30.0), target_name: Some("Tommaso".to_owned()) }),
            )
        );
    }

    #[test]
    fn combo_donate_and_presta() {
        assert_eq!(
            parse_combo_inline_query("dona 10 combo presta 50"),
            combo_query(
                ComboLegQuery::Donate { amount: 10, target_name: None },
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 50, interest_rate_pct: None, target_name: None }),
            )
        );
    }

    #[test]
    fn combo_pvp_and_donate_with_name_on_first_leg() {
        assert_eq!(
            parse_combo_inline_query("10 Mario combo dona 20"),
            combo_query(
                ComboLegQuery::Pvp(PvpQuery { amount: 10, probability_pct: None, target_name: Some("Mario".to_owned()) }),
                ComboLegQuery::Donate { amount: 20, target_name: None },
            )
        );
    }

    #[test]
    fn combo_is_case_insensitive() {
        assert_eq!(
            parse_combo_inline_query("10 COMBO dona 20"),
            combo_query(
                ComboLegQuery::Pvp(PvpQuery { amount: 10, probability_pct: None, target_name: None }),
                ComboLegQuery::Donate { amount: 20, target_name: None },
            )
        );
    }

    #[test]
    fn combo_requires_the_keyword_exactly_once() {
        assert_eq!(parse_combo_inline_query("10 dona 20"), None);
        assert_eq!(parse_combo_inline_query("10 combo dona 20 combo presta 30"), None);
    }

    #[test]
    fn combo_rejects_an_unparseable_leg() {
        assert_eq!(parse_combo_inline_query("Mario combo dona 20"), None);
        assert_eq!(parse_combo_inline_query("10 combo Mario"), None);
    }

    /// All 9 orderings of {pvp, dona, presta} as combo legs (including a type paired with
    /// itself), to make sure none of them gets mis-claimed by another leg's parser.
    #[test]
    fn combo_handles_every_pairing_of_pvp_dona_and_presta() {
        assert_eq!(
            parse_combo_inline_query("10 combo 20"),
            combo_query(ComboLegQuery::Pvp(PvpQuery { amount: 10, probability_pct: None, target_name: None }),
                ComboLegQuery::Pvp(PvpQuery { amount: 20, probability_pct: None, target_name: None }))
        );
        assert_eq!(
            parse_combo_inline_query("10 combo dona 20"),
            combo_query(ComboLegQuery::Pvp(PvpQuery { amount: 10, probability_pct: None, target_name: None }),
                ComboLegQuery::Donate { amount: 20, target_name: None })
        );
        assert_eq!(
            parse_combo_inline_query("10 combo presta 20"),
            combo_query(ComboLegQuery::Pvp(PvpQuery { amount: 10, probability_pct: None, target_name: None }),
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 20, interest_rate_pct: None, target_name: None }))
        );
        assert_eq!(
            parse_combo_inline_query("dona 10 combo 20"),
            combo_query(ComboLegQuery::Donate { amount: 10, target_name: None },
                ComboLegQuery::Pvp(PvpQuery { amount: 20, probability_pct: None, target_name: None }))
        );
        assert_eq!(
            parse_combo_inline_query("dona 10 combo dona 20"),
            combo_query(ComboLegQuery::Donate { amount: 10, target_name: None },
                ComboLegQuery::Donate { amount: 20, target_name: None })
        );
        assert_eq!(
            parse_combo_inline_query("dona 10 combo presta 20"),
            combo_query(ComboLegQuery::Donate { amount: 10, target_name: None },
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 20, interest_rate_pct: None, target_name: None }))
        );
        assert_eq!(
            parse_combo_inline_query("presta 10 combo 20"),
            combo_query(ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 10, interest_rate_pct: None, target_name: None }),
                ComboLegQuery::Pvp(PvpQuery { amount: 20, probability_pct: None, target_name: None }))
        );
        assert_eq!(
            parse_combo_inline_query("presta 10 combo dona 20"),
            combo_query(ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 10, interest_rate_pct: None, target_name: None }),
                ComboLegQuery::Donate { amount: 20, target_name: None })
        );
        assert_eq!(
            parse_combo_inline_query("presta 10 combo presta 20"),
            combo_query(ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 10, interest_rate_pct: None, target_name: None }),
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 20, interest_rate_pct: None, target_name: None }))
        );
    }

    /// Reproduces the user's exact report: without the dispatcher checking combo's filter before
    /// pvp's, this whole string used to be swallowed by `parse_pvp_inline_query` alone (treating
    /// "combo presta 50 30%" as a target name) - this confirms the parser itself, in isolation,
    /// has always split it correctly; the bug was the filter *ordering* in `main.rs`.
    #[test]
    fn combo_with_probability_and_rate_no_target() {
        assert_eq!(
            parse_combo_inline_query("50 70% combo presta 50 30%"),
            combo_query(
                ComboLegQuery::Pvp(PvpQuery { amount: 50, probability_pct: Some(70.0), target_name: None }),
                ComboLegQuery::P2PLoan(P2PLoanQuery { amount: 50, interest_rate_pct: Some(30.0), target_name: None }),
            )
        );
    }
}
