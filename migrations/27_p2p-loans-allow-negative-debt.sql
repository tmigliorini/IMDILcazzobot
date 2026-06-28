-- A negative interest rate is now a first-class feature: the lender commits to paying the
-- borrower back too, gradually, out of the lender's own future growth (see
-- `P2PLoans::get_active_loans`, which now resolves "who owes whom" by the sign of `debt` rather
-- than assuming the borrower is always the one paying). `debt` therefore needs a symmetric range
-- instead of being non-negative.
ALTER TABLE P2P_Loans DROP CONSTRAINT p2p_loans_debt_check;
ALTER TABLE P2P_Loans DROP CONSTRAINT p2p_loans_debt_upper_bound;
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_debt_range CHECK (debt BETWEEN -65535 AND 65535);
