//! Pure integer helpers shared by the handlers. Every function is total (returns `Err` instead of panicking).
use crate::{errors::BrinkError, state::*};
use anchor_lang::prelude::*;

pub const SECONDS_PER_DAY: i64 = 86_400;
pub const DAYS_PER_YEAR: u128 = 365;
pub const BP: u128 = 10_000;
/// Basis-point-seconds in one notional-year: `10_000 x 365 x 86_400`. Dividing `diff_bp x notional x seconds`
/// by this gives USDC base units.
pub const BP_SECONDS_PER_YEAR: u128 = 315_360_000_000;

/// Rounds a timestamp up to the next 00:00 UTC (unchanged when already a midnight). Maturities align to
/// fixing days so settlement reads an exact daily fixing (ADR-006 item 5, ADR-017).
pub fn align_up_to_day(ts: i64) -> Result<i64> {
    let day = ts.div_euclid(SECONDS_PER_DAY);
    let start = day
        .checked_mul(SECONDS_PER_DAY)
        .ok_or(BrinkError::Overflow)?;
    if start == ts {
        Ok(ts)
    } else {
        start
            .checked_add(SECONDS_PER_DAY)
            .ok_or(BrinkError::Overflow.into())
    }
}

/// Trader profit or loss, unbounded, from two cumulative accrual readings: the floating leg is
/// `(accrual_end - accrual_start) / SCALE` basis-point-seconds, the fixed leg `fixed_bp x seconds`; the signed
/// difference, oriented for the leg, times notional, over one notional-year, floored towards zero. Exact to the
/// base unit; no intermediate rounding of the average rate (ADR-017 item 1.4).
pub fn pnl_from_accrual(
    leg: LegKind,
    accrual_start: u128,
    accrual_end: u128,
    fixed_bp: u16,
    notional: u64,
    seconds: i64,
) -> Result<i128> {
    require!(seconds > 0, BrinkError::NotMatured);
    let floating_bps = accrual_end
        .checked_sub(accrual_start)
        .ok_or(BrinkError::Overflow)?
        .checked_div(brink_index::ACCRUAL_SCALE)
        .ok_or(BrinkError::Overflow)?;
    let floating_bps = i128::try_from(floating_bps).map_err(|_| BrinkError::Overflow)?;
    let fixed_bps = i128::from(fixed_bp)
        .checked_mul(i128::from(seconds))
        .ok_or(BrinkError::Overflow)?;
    let diff = floating_bps
        .checked_sub(fixed_bps)
        .ok_or(BrinkError::Overflow)?;
    let diff = match leg {
        LegKind::PayFixed => diff,
        LegKind::ReceiveFixed => diff.checked_neg().ok_or(BrinkError::Overflow)?,
    };
    let mag = diff
        .unsigned_abs()
        .checked_mul(u128::from(notional))
        .ok_or(BrinkError::Overflow)?
        .checked_div(BP_SECONDS_PER_YEAR)
        .ok_or(BrinkError::Overflow)?;
    let mag = i128::try_from(mag).map_err(|_| BrinkError::Overflow)?;
    Ok(if diff < 0 {
        mag.checked_neg().ok_or(BrinkError::Overflow)?
    } else {
        mag
    })
}

/// `x · bp / 10_000`, rounded down.
pub fn mul_bp(x: u64, bp: u32) -> Result<u64> {
    u64::try_from(
        u128::from(x)
            .checked_mul(u128::from(bp))
            .ok_or(BrinkError::Overflow)?
            / 10_000,
    )
    .map_err(|_| BrinkError::Overflow.into())
}
/// Shares minted for `amount` at the current share price.
/// Virtual shares and virtual capital added to every share-price computation (maths findings M-8, M-9; the
/// ERC-4626 decimals-offset construction). The first deposit mints `amount * 10^3` shares, so the share mint has
/// 9 decimals against USDC's 6; a wiped pool (tvl = 0, supply > 0) keeps a defined price, and a donation plus
/// `sync_vault` buys at most `donation / 10^3` of rounding edge over a later depositor. The offset was 10^6 with
/// a 12-decimal mint; the SPL mint supply is a `u64`, so that capped a pool at about 18.4 M USDC (audit F-36).
/// At 10^3 the cap is about 18.4 billion USDC.
pub const VIRTUAL_SHARES: u128 = 1_000;
pub const VIRTUAL_TVL: u128 = 1;
/// Base units of one whole share at 9 decimals; one whole share is one USDC at par.
pub const SHARE_UNIT: u128 = 1_000_000_000;
/// Decimals of the pool share mint.
pub const SHARE_DECIMALS: u8 = 9;
pub fn shares_for(amount: u64, tvl: u64, supply: u64) -> Result<u64> {
    u64::try_from(
        u128::from(amount)
            .checked_mul(u128::from(supply) + VIRTUAL_SHARES)
            .ok_or(BrinkError::Overflow)?
            .checked_div(u128::from(tvl) + VIRTUAL_TVL)
            .ok_or(BrinkError::PoolInvariant)?,
    )
    .map_err(|_| BrinkError::Overflow.into())
}
/// USDC returned for `shares`, rounded down (dust stays with the pool).
pub fn amount_for(shares: u64, tvl: u64, supply: u64) -> Result<u64> {
    require!(supply > 0, BrinkError::PoolInvariant);
    u64::try_from(
        u128::from(shares)
            .checked_mul(u128::from(tvl) + VIRTUAL_TVL)
            .ok_or(BrinkError::Overflow)?
            .checked_div(u128::from(supply) + VIRTUAL_SHARES)
            .ok_or(BrinkError::PoolInvariant)?,
    )
    .map_err(|_| BrinkError::Overflow.into())
}
/// Price of one whole share (`SHARE_UNIT` base units) in USDC micro-units; 10^6 means par. A display figure for
/// events only: it saturates at `u64::MAX` rather than failing, so a pool whose capital far exceeds its share
/// supply (capital synchronised into an empty pool) can still accept deposits (external scan 1, L-28).
pub fn share_price_e6(tvl: u64, supply: u64) -> Result<u64> {
    let p = (u128::from(tvl) + VIRTUAL_TVL)
        .checked_mul(SHARE_UNIT)
        .ok_or(BrinkError::Overflow)?
        .checked_div(u128::from(supply) + VIRTUAL_SHARES)
        .ok_or(BrinkError::PoolInvariant)?;
    Ok(u64::try_from(p).unwrap_or(u64::MAX))
}
/// Utilisation of the whole book against `tvl` in bp, rounded up and saturating at 10 000, so that any open
/// notional at all registers as at least 1 bp (external scan 1, M-9).
pub fn total_util_bp_ceil(open_pay: u64, open_rec: u64, tvl: u64) -> Result<u32> {
    let open = open_pay.checked_add(open_rec).ok_or(BrinkError::Overflow)?;
    if tvl == 0 {
        return Ok(if open > 0 { 10_000 } else { 0 });
    }
    let u = u128::from(open)
        .checked_mul(BP)
        .ok_or(BrinkError::Overflow)?
        .div_ceil(u128::from(tvl));
    Ok(u32::try_from(u.min(10_000)).unwrap_or(10_000))
}
/// LP exit fee for `gross` at the pool's current total utilisation: `LP_EXIT_FEE_BP` scaled by utilisation,
/// so an LP leaving a fully used book pays the full fee and a book carrying only dust charges next to nothing.
/// The fee compensates the LPs who remain for the risk the leaver hands them, which is proportional to the
/// exposure, not to whether a swap exists (external scan 1, M-16).
pub fn lp_exit_fee(gross: u64, open_pay: u64, open_rec: u64, tvl: u64) -> Result<u64> {
    let util = total_util_bp_ceil(open_pay, open_rec, tvl)?;
    u64::try_from(
        u128::from(gross)
            .checked_mul(u128::from(vernier::LP_EXIT_FEE_BP))
            .ok_or(BrinkError::Overflow)?
            .checked_mul(u128::from(util))
            .ok_or(BrinkError::Overflow)?
            / (BP * BP),
    )
    .map_err(|_| BrinkError::Overflow.into())
}
/// Recomputes utilisation from open notional so rounding never drifts. Saturates at 10 000 bp: utilisation
/// above 100 percent only arises when trader gains shrink `tvl` under an open book, and the stored figure is then
/// used to refuse new opens (`leg_capacity` is zero at or above a cap) and to keep `cancel` and `liquidate`
/// quoting (`vernier::demand_bp` rejects utilisation above 10 000). A pool with no LP capital and open legs is
/// fully utilised, not malformed (maths finding M-4).
pub fn rebase_utilisation(pool: &mut Pool) -> Result<()> {
    if pool.tvl == 0 {
        pool.util_pay_bp = if pool.open_pay_notional > 0 {
            10_000
        } else {
            0
        };
        pool.util_rec_bp = if pool.open_rec_notional > 0 {
            10_000
        } else {
            0
        };
        return Ok(());
    }
    let util = |n: u64| -> Result<u16> {
        let u = u128::from(n)
            .checked_mul(BP)
            .ok_or(BrinkError::Overflow)?
            .checked_div(u128::from(pool.tvl))
            .ok_or(BrinkError::PoolInvariant)?;
        Ok(u16::try_from(u.min(10_000)).unwrap_or(10_000))
    };
    pool.util_pay_bp = util(pool.open_pay_notional)?;
    pool.util_rec_bp = util(pool.open_rec_notional)?;
    Ok(())
}
/// Trader pnl in USDC for a rate difference held over `days`, bounded to ±collateral.
/// `diff_bp` is `(floating − fixed)` for pay-fixed and `(fixed − floating)` for receive-fixed.
/// Unbounded pnl at second resolution: `diff_bp` of `notional` accrued over `seconds`, floored. Used by the
/// early-close valuation where the accrued and the remaining legs are summed before the collateral clamp is
/// applied, so each leg must stay unbounded here. Returns `i128` to make the sum overflow-free.
pub fn pnl_secs(diff_bp: i64, notional: u64, seconds: i64) -> Result<i128> {
    let secs = u128::try_from(seconds.max(0)).map_err(|_| BrinkError::Overflow)?;
    let mag = u128::from(diff_bp.unsigned_abs())
        .checked_mul(u128::from(notional))
        .ok_or(BrinkError::Overflow)?
        .checked_mul(secs)
        .ok_or(BrinkError::Overflow)?
        .checked_div(
            BP.checked_mul(DAYS_PER_YEAR)
                .and_then(|x| x.checked_mul(86_400))
                .ok_or(BrinkError::Overflow)?,
        )
        .ok_or(BrinkError::Overflow)?;
    let mag = i128::try_from(mag).map_err(|_| BrinkError::Overflow)?;
    Ok(if diff_bp < 0 {
        mag.checked_neg().ok_or(BrinkError::Overflow)?
    } else {
        mag
    })
}

/// Clamps a signed value to `[-bound, +bound]` and narrows to `i64`.
pub fn clamp_to_collateral(value: i128, bound: u64) -> Result<i64> {
    let b = i128::from(bound);
    let v = value.clamp(b.checked_neg().ok_or(BrinkError::Overflow)?, b);
    i64::try_from(v).map_err(|_| BrinkError::Overflow.into())
}

pub fn pnl_bounded(diff_bp: i64, notional: u64, days: u16, collateral: u64) -> Result<i64> {
    let mag = u128::from(diff_bp.unsigned_abs())
        .checked_mul(u128::from(notional))
        .ok_or(BrinkError::Overflow)?
        .checked_mul(u128::from(days))
        .ok_or(BrinkError::Overflow)?
        .checked_div(BP.checked_mul(DAYS_PER_YEAR).ok_or(BrinkError::Overflow)?)
        .ok_or(BrinkError::Overflow)?;
    let mag = u64::try_from(mag).unwrap_or(u64::MAX).min(collateral);
    let mag = i64::try_from(mag).map_err(|_| BrinkError::Overflow)?;
    Ok(if diff_bp < 0 {
        mag.checked_neg().ok_or(BrinkError::Overflow)?
    } else {
        mag
    })
}
/// Signed difference `floating − fixed` oriented for the leg.
pub fn oriented_diff(leg: LegKind, floating_bp: i64, fixed_bp: i64) -> Result<i64> {
    let d = floating_bp
        .checked_sub(fixed_bp)
        .ok_or(BrinkError::Overflow)?;
    Ok(match leg {
        LegKind::PayFixed => d,
        LegKind::ReceiveFixed => d.checked_neg().ok_or(BrinkError::Overflow)?,
    })
}
/// Days remaining, rounded up, clamped to `[0, total]`.
pub fn days_remaining(now: i64, matures_ts: i64, total_days: u16) -> u16 {
    let secs = matures_ts.saturating_sub(now).max(0);
    let d = (secs.saturating_add(SECONDS_PER_DAY.saturating_sub(1))) / SECONDS_PER_DAY;
    u16::try_from(d).unwrap_or(total_days).min(total_days)
}
pub fn tenor_from(u: u8) -> Result<vernier::Tenor> {
    Ok(match u {
        0 => vernier::Tenor::D28,
        1 => vernier::Tenor::D60,
        2 => vernier::Tenor::D90,
        3 => vernier::Tenor::D180,
        _ => return Err(BrinkError::Tenor.into()),
    })
}
pub fn leg_from(l: LegKind) -> vernier::Leg {
    match l {
        LegKind::PayFixed => vernier::Leg::Pay,
        LegKind::ReceiveFixed => vernier::Leg::Receive,
    }
}
pub fn opposite(l: LegKind) -> LegKind {
    match l {
        LegKind::PayFixed => LegKind::ReceiveFixed,
        LegKind::ReceiveFixed => LegKind::PayFixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    const OFFSET: u64 = 1_000;
    #[test]
    fn first_deposit_mints_at_the_offset() {
        assert_eq!(shares_for(1_000, 0, 0).unwrap(), 1_000 * OFFSET);
        assert_eq!(
            amount_for(1_000 * OFFSET, 1_000, 1_000 * OFFSET).unwrap(),
            1_000
        );
    }
    #[test]
    fn shares_track_price() {
        // 10,310 USDC against 10,000 whole shares: a 1,031 USDC deposit mints about 1,000 shares; the virtual
        // offset tilts the price by about 1 part in 10^7 here.
        let supply = 10_000 * OFFSET;
        let minted = shares_for(1_031, 10_310, supply).unwrap();
        assert!(
            minted < 1_000 * OFFSET + 10 && minted > 999 * OFFSET,
            "{minted}"
        );
        let back = amount_for(1_000 * OFFSET, 10_310, supply).unwrap();
        assert!((1_030..=1_031).contains(&back), "{back}");
        assert_eq!(share_price_e6(0, 0).unwrap(), 1_000_000);
        let price = share_price_e6(10_310, supply).unwrap();
        assert!((1_030_900..=1_031_100).contains(&price), "{price}");
    }
    #[test]
    fn supply_fits_u64_up_to_billions_of_usdc() {
        // Audit F-36: with the 10^6 offset a 18.5 M USDC pool overflowed the u64 share supply.
        let tvl = 18_500_000 * 1_000_000u64;
        let minted = shares_for(tvl, 0, 0).unwrap();
        assert_eq!(minted, tvl * OFFSET);
        assert!(shares_for(1_000_000 * 1_000_000, tvl, minted).is_ok());
        // The hard cap is now about 18.4 billion USDC.
        assert!(shares_for(18_000_000_000 * 1_000_000u64, 0, 0).is_ok());
        assert!(shares_for(19_000_000_000 * 1_000_000u64, 0, 0).is_err());
    }
    #[test]
    fn wiped_pool_and_donation_do_not_break_the_price() {
        // tvl = 0, supply > 0: the next depositor receives nearly everything, the stale shares keep dust.
        let minted = shares_for(1_000, 0, 5_000 * OFFSET).unwrap();
        assert!(amount_for(minted, 1_000, 5_000 * OFFSET + minted).unwrap() >= 999);
        // one unit deposited, 1,000,000 USDC donated: a 10,000 USDC depositor loses at most donation / 10^3.
        let supply = shares_for(1, 0, 0).unwrap();
        let minted = shares_for(10_000 * 1_000_000, 1_000_000 * 1_000_000 + 1, supply).unwrap();
        let back = amount_for(
            minted,
            1_000_000 * 1_000_000 + 1 + 10_000 * 1_000_000,
            supply + minted,
        )
        .unwrap();
        // The depositor's loss is bounded by donation / 10^3 (1,000 USDC here); the attacker spent 1,000,000.
        assert!(back + 1_000 * 1_000_000 >= 10_000 * 1_000_000, "{back}");
    }
    #[test]
    fn utilisation_saturates_instead_of_failing() {
        // Maths finding M-4: pool 1 000 000 USDC, receive leg at its 48 percent cap, one pay-fixed trader wins
        // 6 000 USDC. The receive leg is now at 4 828 bp; the close must still book.
        let mut p = Pool {
            benchmark: Pubkey::default(),
            share_mint: Pubkey::default(),
            vault: Pubkey::default(),
            hook_program: Pubkey::default(),
            hooks: HookFlags::default(),
            pricer: Pricer::Vernier,
            params: VernierParamsOnChain {
                model_pay_bp: [0; 4],
                model_rec_bp: [0; 4],
                term_bp: [0; 4],
                demand_k_bp: 0,
                demand_cap_bp: 0,
                collateral_bp: [1; 4],
            },
            pending_params: VernierParamsOnChain {
                model_pay_bp: [0; 4],
                model_rec_bp: [0; 4],
                term_bp: [0; 4],
                demand_k_bp: 0,
                demand_cap_bp: 0,
                collateral_bp: [1; 4],
            },
            pending_effective_slot: 0,
            tvl: 994_000,
            collateral_held: 0,
            util_pay_bp: 0,
            util_rec_bp: 0,
            open_pay_notional: 0,
            open_rec_notional: 480_000,
            open_swaps: 1,
            fees_lifetime: 0,
            event_seq: 0,
            min_notional: 1,
            max_notional: u64::MAX,
            bump: 0,
            fees_buyback_accrued: 0,
            fees_treasury_accrued: 0,
            withdraw_reserved: 0,
            book_pay: BookSide::default(),
            book_rec: BookSide::default(),
            collateral_pay: 0,
            collateral_rec: 0,
            ladder: [0; LADDER_BYTES],
            share_supply: 0,
            queued_shares: 0,
            queue_first_slot: 0,
            limited_window_start: 0,
            limited_window_notional: 0,
            _reserved: [0; 24],
        };
        rebase_utilisation(&mut p).unwrap();
        assert_eq!(p.util_rec_bp, 4_828);
        assert!(p.assert_caps().is_err());
        assert!(p.assert_conservation(994_000).is_ok());
        // Capital gone, book still open: fully utilised; conservation holds; nothing can be opened.
        p.tvl = 0;
        rebase_utilisation(&mut p).unwrap();
        assert_eq!((p.util_pay_bp, p.util_rec_bp), (0, 10_000));
        assert!(p.assert_conservation(0).is_ok());
        assert!(p.assert_caps().is_err());
        assert_eq!(
            vernier::leg_capacity(&p.vernier_pool(), vernier::Leg::Pay),
            0
        );
        // Above 100 percent saturates rather than failing the narrowing.
        p.tvl = 100_000;
        rebase_utilisation(&mut p).unwrap();
        assert_eq!(p.util_rec_bp, 10_000);
        // Empty book, no capital: both zero and the caps hold, so the last LP may leave.
        p.open_rec_notional = 0;
        p.tvl = 0;
        rebase_utilisation(&mut p).unwrap();
        assert_eq!((p.util_pay_bp, p.util_rec_bp), (0, 0));
        assert!(p.assert_caps().is_ok());
    }
    fn swap_strategy() -> impl Strategy<Value = (bool, u64, u128, u16, i64, i64)> {
        (
            any::<bool>(),
            1u64..=10_000_000_000_000,
            0u128..=10_000_000_000_000,
            0u16..=5_000,
            1_000_000_000i64..=2_000_000_000,
            prop::sample::select(vec![7i64, 28, 90, 180, 365]),
        )
            .prop_map(|(pay, n, a_bp_s, f, o, d)| {
                let a = a_bp_s * brink_index::ACCRUAL_SCALE;
                (
                    pay,
                    n,
                    a,
                    f,
                    o,
                    align_up_to_day(o + d * SECONDS_PER_DAY).unwrap(),
                )
            })
    }
    fn book_pool() -> Pool {
        let params = VernierParamsOnChain {
            model_pay_bp: [11, 22, 31, 54],
            model_rec_bp: [9, 12, 14, 19],
            term_bp: [3, 5, 7, 12],
            demand_k_bp: 45,
            demand_cap_bp: 60,
            collateral_bp: [120, 230, 330, 600],
        };
        Pool {
            benchmark: Pubkey::default(),
            share_mint: Pubkey::default(),
            vault: Pubkey::default(),
            hook_program: Pubkey::default(),
            hooks: HookFlags::default(),
            pricer: Pricer::Vernier,
            params,
            pending_params: params,
            pending_effective_slot: 0,
            tvl: u64::MAX,
            collateral_held: u64::MAX,
            util_pay_bp: 0,
            util_rec_bp: 0,
            open_pay_notional: 0,
            open_rec_notional: 0,
            open_swaps: 0,
            fees_lifetime: 0,
            event_seq: 0,
            min_notional: 0,
            max_notional: u64::MAX,
            bump: 0,
            book_pay: BookSide::default(),
            book_rec: BookSide::default(),
            collateral_pay: 0,
            collateral_rec: 0,
            ladder: [0; LADDER_BYTES],
            share_supply: 0,
            queued_shares: 0,
            queue_first_slot: 0,
            limited_window_start: 0,
            limited_window_notional: 0,
            fees_buyback_accrued: 0,
            fees_treasury_accrued: 0,
            withdraw_reserved: 0,
            _reserved: [0; 24],
        }
    }
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]
        /// The O(1) book value equals the sum of the per-swap values computed one by one, to the unit (the
        /// division happens once, on the summed numerator, so the aggregate is at least as exact as the sum of
        /// floors), and closing every swap returns the four aggregates to zero exactly.
        #[test]
        fn book_aggregate_equals_per_swap_sum_and_reverses(
            swaps in prop::collection::vec(swap_strategy(), 1..8),
            a_now_bp_s in 0u128..=20_000_000_000_000,
            v in 0u16..=5_000,
            now in 1_000_000_000i64..=2_100_000_000,
        ) {
            let mut p = book_pool();
            let mut terms = Vec::new();
            let mut sum: i128 = 0;
            for &(pay, n, a, f, o, m) in &swaps {
                let leg = if pay { LegKind::PayFixed } else { LegKind::ReceiveFixed };
                let t = BookTerms::for_swap(leg, n, a, f, o, m).unwrap();
                p.book_add(&t).unwrap();
                terms.push(t);
                let sign: i128 = if pay { 1 } else { -1 };
                let a_open = i128::try_from(a / brink_index::ACCRUAL_SCALE).unwrap();
                let a_now = i128::try_from(a_now_bp_s).unwrap();
                let per = sign * i128::from(n)
                    * ((a_now - a_open) + i128::from(v) * (i128::from(m) - i128::from(now))
                        - i128::from(f) * (i128::from(m) - i128::from(o)));
                sum += per;
            }
            let k = i128::try_from(BP_SECONDS_PER_YEAR).unwrap();
            let got = p.book_value(a_now_bp_s * brink_index::ACCRUAL_SCALE, v, now).unwrap();
            // Each side divides once on its summed numerator, so the two-sided total is within one unit of the
            // single-division sum and never further from the exact value than the sum of per-swap floors.
            prop_assert!((got - sum / k).abs() <= 1, "got {} expected {}", got, sum / k);
            // Clamped value never exceeds collateral or tvl in magnitude.
            p.collateral_held = 1_000_000;
            p.collateral_pay = 600_000;
            p.collateral_rec = 400_000;
            p.tvl = 500_000;
            let c = p.book_value_clamped(a_now_bp_s * brink_index::ACCRUAL_SCALE, v, now).unwrap();
            prop_assert!((-1_000_000..=500_000).contains(&c));
            prop_assert!(p.effective_tvl_for_withdraw(a_now_bp_s * brink_index::ACCRUAL_SCALE, v, now).unwrap() <= 500_000);
            for t in &terms {
                p.book_sub(t).unwrap();
            }
            prop_assert_eq!((p.book_pay, p.book_rec), (BookSide::default(), BookSide::default()));
        }
    }
    #[test]
    fn book_value_matches_a_single_swap_mark() {
        // One pay-fixed swap, 1 M USDC at 700 bp fixed, 90 days; rate at 800 bp throughout: after 30 days the
        // accrued part is 100 bp x 30 d and the forward part 100 bp x 60 d, i.e. the full-term value.
        let n = 1_000_000_000_000u64;
        let o = 20_000 * SECONDS_PER_DAY;
        let m = o + 90 * SECONDS_PER_DAY;
        let mut p = book_pool();
        p.book_add(&BookTerms::for_swap(LegKind::PayFixed, n, 0, 700, o, m).unwrap())
            .unwrap();
        let now = o + 30 * SECONDS_PER_DAY;
        let a_now = 800u128 * 30 * 86_400 * brink_index::ACCRUAL_SCALE;
        let v = p.book_value(a_now, 800, now).unwrap();
        assert_eq!(v, 2_465_753_424);
        // A receive-fixed twin nets the book to zero.
        p.book_add(&BookTerms::for_swap(LegKind::ReceiveFixed, n, 0, 700, o, m).unwrap())
            .unwrap();
        assert_eq!(p.book_value(a_now, 800, now).unwrap(), 0);
        // Deposit pricing: the pool owes 2,465.75 USDC on the first swap alone.
        let mut q = book_pool();
        q.tvl = 10_000_000_000;
        q.collateral_held = 50_000_000_000;
        q.collateral_pay = 50_000_000_000;
        q.book_add(&BookTerms::for_swap(LegKind::PayFixed, n, 0, 700, o, m).unwrap())
            .unwrap();
        assert_eq!(
            q.effective_tvl_for_deposit(a_now, 800, now).unwrap(),
            10_000_000_000 - 2_465_753_424
        );
        assert_eq!(
            q.effective_tvl_for_withdraw(a_now, 800, now).unwrap(),
            10_000_000_000 - 2_465_753_424
        );
        // Rate at 600: traders owe; deposits price it in, withdrawals do not.
        let a_low = 600u128 * 30 * 86_400 * brink_index::ACCRUAL_SCALE;
        assert_eq!(
            q.effective_tvl_for_deposit(a_low, 600, now).unwrap(),
            10_000_000_000 + 2_465_753_424
        );
        assert_eq!(
            q.effective_tvl_for_withdraw(a_low, 600, now).unwrap(),
            10_000_000_000
        );
    }
    #[test]
    fn maturity_aligns_up_to_midnight() {
        assert_eq!(align_up_to_day(0).unwrap(), 0);
        assert_eq!(align_up_to_day(1).unwrap(), SECONDS_PER_DAY);
        assert_eq!(align_up_to_day(SECONDS_PER_DAY).unwrap(), SECONDS_PER_DAY);
        assert_eq!(
            align_up_to_day(90 * SECONDS_PER_DAY + 3_600).unwrap(),
            91 * SECONDS_PER_DAY
        );
    }
    #[test]
    fn pnl_from_accrual_matches_the_day_formula_without_the_bp_floor() {
        // 100 bp over 90 days on 1,000,000 USDC (6 dp): exactly 1e12 x 100 x 90 / (1e4 x 365) = 2_465_753_424.65
        let secs = 90 * SECONDS_PER_DAY;
        let end = 800u128 * u128::try_from(secs).unwrap() * brink_index::ACCRUAL_SCALE;
        let n = 1_000_000_000_000u64;
        assert_eq!(
            pnl_from_accrual(LegKind::PayFixed, 0, end, 700, n, secs).unwrap(),
            2_465_753_424
        );
        assert_eq!(
            pnl_from_accrual(LegKind::ReceiveFixed, 0, end, 700, n, secs).unwrap(),
            -2_465_753_424
        );
        // A fractional average (699.5 bp against 700 fixed) is a small loss, not zero: the floor on the average
        // rate that `average_bp` applies is gone.
        let end = 6_995u128 * u128::try_from(secs).unwrap() * brink_index::ACCRUAL_SCALE / 10;
        assert_eq!(
            pnl_from_accrual(LegKind::PayFixed, 0, end, 700, n, secs).unwrap(),
            -12_328_767
        );
        assert!(pnl_from_accrual(LegKind::PayFixed, 0, 0, 1, 1, 0).is_err());
    }
    #[test]
    fn exit_fee_is_50bp_at_full_utilisation_and_scales_down() {
        assert_eq!(mul_bp(100_000, vernier::LP_EXIT_FEE_BP).unwrap(), 500);
        // External scan 1, M-16: the fee follows the exposure handed to remaining LPs.
        assert_eq!(lp_exit_fee(100_000_000, 1_000, 0, 1_000).unwrap(), 500_000); // 100 percent used
        assert_eq!(lp_exit_fee(100_000_000, 500, 0, 1_000).unwrap(), 250_000); // 50 percent
        assert_eq!(lp_exit_fee(100_000_000, 0, 0, 1_000).unwrap(), 0); // no book
                                                                       // A 1 USDC swap on a 1 M USDC pool: utilisation rounds up to 1 bp, fee 0.0001 x 50 bp.
        assert_eq!(
            lp_exit_fee(100_000_000, 1_000_000, 0, 1_000_000_000_000).unwrap(),
            50
        );
    }
    /// External scan 1, M-9: any exposure at all registers as at least 1 bp.
    #[test]
    fn total_utilisation_rounds_up() {
        assert_eq!(total_util_bp_ceil(1, 0, 1_000_000_000_000).unwrap(), 1);
        assert_eq!(total_util_bp_ceil(0, 0, 1_000_000_000_000).unwrap(), 0);
        assert_eq!(total_util_bp_ceil(5_000, 0, 10_000).unwrap(), 5_000);
        assert_eq!(total_util_bp_ceil(1, 0, 0).unwrap(), 10_000);
        assert_eq!(total_util_bp_ceil(u64::MAX, 0, 1).unwrap(), 10_000);
    }
    /// External scan 1, L-28: the display price saturates instead of refusing the deposit.
    #[test]
    fn share_price_saturates_for_display() {
        assert_eq!(share_price_e6(u64::MAX, 0).unwrap(), u64::MAX);
        assert_eq!(share_price_e6(0, 0).unwrap(), 1_000_000);
    }
    #[test]
    fn pnl_is_bounded_and_signed() {
        // 100 bp over 90 days on 1,000,000 USDC = 1,000,000 × 0.01 × 90/365 = 2,465.75 → 2,465
        assert_eq!(pnl_bounded(100, 1_000_000, 90, 10_000_000).unwrap(), 2_465);
        assert_eq!(
            pnl_bounded(-100, 1_000_000, 90, 10_000_000).unwrap(),
            -2_465
        );
        assert_eq!(
            pnl_bounded(-5_000, 1_000_000, 180, 33_000).unwrap(),
            -33_000
        );
        assert_eq!(pnl_bounded(5_000, 1_000_000, 180, 33_000).unwrap(), 33_000);
    }
    #[test]
    fn pay_fixed_gains_when_floating_rises() {
        assert_eq!(oriented_diff(LegKind::PayFixed, 750, 700).unwrap(), 50);
        assert_eq!(oriented_diff(LegKind::ReceiveFixed, 750, 700).unwrap(), -50);
    }
    #[test]
    fn days_remaining_rounds_up_and_clamps() {
        assert_eq!(days_remaining(0, 90 * SECONDS_PER_DAY, 90), 90);
        assert_eq!(days_remaining(1, 90 * SECONDS_PER_DAY, 90), 90);
        assert_eq!(
            days_remaining(89 * SECONDS_PER_DAY + 1, 90 * SECONDS_PER_DAY, 90),
            1
        );
        assert_eq!(
            days_remaining(91 * SECONDS_PER_DAY, 90 * SECONDS_PER_DAY, 90),
            0
        );
    }
}
