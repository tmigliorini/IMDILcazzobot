-- `debt` is read back into a `u16` everywhere it's consumed (see `P2PLoan::debt` /
-- `compute_debt`); an excessive interest rate could previously make `lend()` write a `debt` that
-- doesn't fit one, silently corrupting balances once read back. The application now rejects such
-- loans outright (see `P2PLoans::lend`) - this constraint is the matching backstop at the data layer.
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_debt_upper_bound CHECK (debt <= 65535);
