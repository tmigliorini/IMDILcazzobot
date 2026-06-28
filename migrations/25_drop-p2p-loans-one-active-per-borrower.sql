-- a borrower may now have multiple simultaneous active P2P loans (from different lenders);
-- repayment priority among them is handled in application code (oldest loan first).
DROP INDEX IF EXISTS idx_p2p_loans_one_active_per_borrower;
