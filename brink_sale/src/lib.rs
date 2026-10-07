//! Brink fixed-price token sale.
//!
//! One `Round` account per round (angel, public). A round sells a fixed allocation of an SPL token for SOL at a
//! price fixed before it opens. The program holds the SOL in an escrow PDA and the tokens in a vault PDA until the
//! round is finalised; nothing leaves either account on an authority's say-so alone.
//!
//! ```text
//! create_round ──fund_round (tokens into vault)──▶ open_round ──contribute*──▶ close_round ──▶ finalise(tge)
//!                                                                     │                         │
//!                                                              soft cap missed             claim* (TGE share,
//!                                                              or cancel_round              then linear vesting)
//!                                                                     ▼
//!                                                                 refund*
//! ```
//!
//! * Price, caps, per-wallet bounds, window, unlock share and vesting length are fixed at creation and cannot be
//!   edited. A mistake is corrected by cancelling the round (everyone is refunded) and creating a new one.
//! * `open_round` refuses until the vault holds enough tokens to honour the hard cap, so a sold-out round can
//!   always deliver.
//! * `contribute` refuses anything that would exceed the hard cap or a wallet's bounds; the client reads the room
//!   left and quotes exactly. Contributions are counted, never pro-rated, so a buyer knows their allocation when
//!   the transaction confirms.
//! * `close_round` is callable by anyone once the window ends or the hard cap is reached; nobody, the authority
//!   included, can close early. If the soft cap was missed the round cancels itself and every participant may
//!   `refund`.
//! * `finalise` sets the token generation time, moves the escrowed SOL to the treasury recorded at creation and
//!   returns unsold tokens. Until then the authority cannot touch a lamport.
//! * `claim` pays the unlocked share at TGE and the remainder linearly over `vest_seconds`; a round with
//!   `tge_bps = 10_000` is fully unlocked at TGE.
//! * A round may be allowlisted through a Merkle root over participant addresses, or open to any wallet.
//! * Only the program's upgrade authority creates rounds, so a round id on this program is always an official
//!   round. A Closed round that is not finalised within `FINALISE_GRACE` of its window end can be expired by
//!   anyone, which cancels it and reopens refunds; buyer principal is never trapped behind a lost key.
//! * The escrow is funded with its rent floor at creation, so a donation of dust cannot block the last refund.
//! * Authority changes are two-step (`set_authority`, then `accept_authority` by the new key).
//!
//! Arithmetic is checked everywhere; amounts are computed in u128 and refused if they do not fit u64.
#![allow(clippy::module_name_repetitions)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )
)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::system_program::{self, Transfer};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use solana_sha256_hasher::hashv;

declare_id!("GYCJoDiZCh7mbqyWRjYJmpPUt3nPLs6887tGm58rmK3v");

pub const BPS: u64 = 10_000;
/// Smallest per-wallet minimum: keeps the escrow above the rent-exempt floor after any partial refund sequence.
pub const MIN_WALLET_FLOOR: u64 = 10_000_000;
/// Latest TGE an authority may set at finalisation: 30 days out.
pub const MAX_TGE_DELAY: i64 = 30 * 86_400;
/// Longest vesting a round may carry: 2 years.
pub const MAX_VEST_SECONDS: i64 = 730 * 86_400;
/// Longest allowlist proof accepted (2^24 participants).
pub const MAX_PROOF: usize = 24;
/// Longest contribution window: 90 days.
pub const MAX_WINDOW: i64 = 90 * 86_400;
/// Time after the window end within which a Closed round must be finalised; afterwards anyone may expire it.
pub const FINALISE_GRACE: i64 = 30 * 86_400;

#[program]
pub mod brink_sale {
    use super::*;

    /// Creates a round and its token vault. Every parameter is final.
    pub fn create_round(ctx: Context<CreateRound>, args: CreateRoundArgs) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(args.lamports_per_token > 0, SaleError::Price);
        require!(
            args.hard_cap > 0 && args.soft_cap > 0 && args.soft_cap <= args.hard_cap,
            SaleError::Caps
        );
        require!(
            args.min_per_wallet >= MIN_WALLET_FLOOR
                && args.min_per_wallet <= args.max_per_wallet
                && args.max_per_wallet <= args.hard_cap,
            SaleError::WalletBounds
        );
        require!(
            args.start_ts >= now && args.end_ts > args.start_ts,
            SaleError::Window
        );
        require!(
            args.end_ts
                .checked_sub(args.start_ts)
                .ok_or(SaleError::Math)?
                <= MAX_WINDOW,
            SaleError::Window
        );
        require!(
            args.tge_bps >= 1 && args.tge_bps <= 10_000,
            SaleError::Unlock
        );
        require!(
            (args.tge_bps == 10_000) == (args.vest_seconds == 0),
            SaleError::Unlock
        );
        require!(
            args.vest_seconds >= 0 && args.vest_seconds <= MAX_VEST_SECONDS,
            SaleError::Unlock
        );
        let decimals = ctx.accounts.mint.decimals;
        // The hard cap must be deliverable in tokens that fit u64.
        let _ = tokens_for(args.hard_cap, args.lamports_per_token, decimals)?;

        // The escrow carries its own rent floor from the start, so the final refund of a cancelled round can
        // never be refused for leaving the account below the floor (a dust donation would otherwise do that).
        let floor = Rent::get()?.minimum_balance(0);
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.authority.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                },
            ),
            floor,
        )?;

        let r = &mut ctx.accounts.round;
        r.authority = ctx.accounts.authority.key();
        r.pending_authority = Pubkey::default();
        r.treasury = ctx.accounts.treasury.key();
        r.mint = ctx.accounts.mint.key();
        r.vault = ctx.accounts.vault.key();
        r.round_id = args.round_id;
        r.decimals = decimals;
        r.lamports_per_token = args.lamports_per_token;
        r.hard_cap = args.hard_cap;
        r.soft_cap = args.soft_cap;
        r.min_per_wallet = args.min_per_wallet;
        r.max_per_wallet = args.max_per_wallet;
        r.start_ts = args.start_ts;
        r.end_ts = args.end_ts;
        r.tge_ts = 0;
        r.tge_bps = args.tge_bps;
        r.vest_seconds = args.vest_seconds;
        r.allowlist_root = args.allowlist_root;
        r.allowlisted = args.allowlisted;
        r.raised = 0;
        r.tokens_sold = 0;
        r.tokens_claimed = 0;
        r.participants = 0;
        r.state = RoundState::Pending;
        r.bump = ctx.bumps.round;
        r.escrow_bump = ctx.bumps.escrow;
        r.reserved = [0; 64];
        emit_cpi!(RoundCreated {
            round: r.key(),
            round_id: r.round_id,
            lamports_per_token: r.lamports_per_token,
            hard_cap: r.hard_cap,
            soft_cap: r.soft_cap,
            start_ts: r.start_ts,
            end_ts: r.end_ts,
            tge_bps: r.tge_bps,
            vest_seconds: r.vest_seconds,
        });
        Ok(())
    }

    /// Opens the round for contributions once the vault can honour the hard cap.
    pub fn open_round(ctx: Context<Authority>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.round;
        require!(r.state == RoundState::Pending, SaleError::State);
        require!(now < r.end_ts, SaleError::Window);
        let cap_tokens = tokens_for(r.hard_cap, r.lamports_per_token, r.decimals)?;
        require!(ctx.accounts.vault.amount >= cap_tokens, SaleError::Unfunded);
        r.state = RoundState::Open;
        emit_cpi!(RoundOpened {
            round: r.key(),
            cap_tokens,
            vault_tokens: ctx.accounts.vault.amount
        });
        Ok(())
    }

    /// Buys tokens at the fixed price. `lamports` must respect the wallet bounds and the room left in the round.
    pub fn contribute(ctx: Context<Contribute>, lamports: u64, proof: Vec<[u8; 32]>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let buyer = ctx.accounts.buyer.key();
        let r = &mut ctx.accounts.round;
        require!(r.state == RoundState::Open, SaleError::State);
        require!(now >= r.start_ts && now < r.end_ts, SaleError::Window);
        if r.allowlisted {
            require!(proof.len() <= MAX_PROOF, SaleError::Allowlist);
            require!(
                verify_allowlist(&r.allowlist_root, &buyer, &proof),
                SaleError::Allowlist
            );
        }
        let p = &mut ctx.accounts.participant;
        if p.buyer == Pubkey::default() {
            p.buyer = buyer;
            p.round = r.key();
            p.bump = ctx.bumps.participant;
            r.participants = r.participants.checked_add(1).ok_or(SaleError::Math)?;
        }
        let wallet_total = p.lamports.checked_add(lamports).ok_or(SaleError::Math)?;
        require!(
            wallet_total >= r.min_per_wallet && wallet_total <= r.max_per_wallet,
            SaleError::WalletBounds
        );
        let raised = r.raised.checked_add(lamports).ok_or(SaleError::Math)?;
        require!(raised <= r.hard_cap, SaleError::RoundFull);
        let tokens = tokens_for(lamports, r.lamports_per_token, r.decimals)?;
        require!(tokens > 0, SaleError::Dust);

        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                },
            ),
            lamports,
        )?;

        p.lamports = wallet_total;
        p.tokens = p.tokens.checked_add(tokens).ok_or(SaleError::Math)?;
        r.raised = raised;
        r.tokens_sold = r.tokens_sold.checked_add(tokens).ok_or(SaleError::Math)?;
        emit_cpi!(Contributed {
            round: r.key(),
            buyer,
            lamports,
            tokens,
            raised: r.raised,
            tokens_sold: r.tokens_sold
        });
        Ok(())
    }

    /// Closes contributions. Anyone may call once the window has ended or the hard cap is reached; nobody may
    /// close early. A round below its soft cap cancels and refunds.
    pub fn close_round(ctx: Context<Anyone>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.round;
        require!(r.state == RoundState::Open, SaleError::State);
        require!(now >= r.end_ts || r.raised == r.hard_cap, SaleError::Window);
        r.state = if r.raised >= r.soft_cap {
            RoundState::Closed
        } else {
            RoundState::Cancelled
        };
        emit_cpi!(RoundClosed {
            round: r.key(),
            raised: r.raised,
            tokens_sold: r.tokens_sold,
            cancelled: r.state == RoundState::Cancelled
        });
        Ok(())
    }

    /// Cancels a round that has not been finalised. Every participant may then refund.
    pub fn cancel_round(ctx: Context<Authority>) -> Result<()> {
        let r = &mut ctx.accounts.round;
        require!(
            matches!(
                r.state,
                RoundState::Pending | RoundState::Open | RoundState::Closed
            ),
            SaleError::State
        );
        r.state = RoundState::Cancelled;
        emit_cpi!(RoundClosed {
            round: r.key(),
            raised: r.raised,
            tokens_sold: r.tokens_sold,
            cancelled: true
        });
        Ok(())
    }

    /// Anyone: cancels an Open or Closed round whose window ended more than `FINALISE_GRACE` ago, so buyers can
    /// refund when the authority never finalises. Liveness does not depend on one key.
    pub fn expire_round(ctx: Context<Anyone>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.round;
        require!(
            matches!(r.state, RoundState::Open | RoundState::Closed),
            SaleError::State
        );
        require!(
            now >= r
                .end_ts
                .checked_add(FINALISE_GRACE)
                .ok_or(SaleError::Math)?,
            SaleError::Window
        );
        r.state = RoundState::Cancelled;
        emit_cpi!(RoundClosed {
            round: r.key(),
            raised: r.raised,
            tokens_sold: r.tokens_sold,
            cancelled: true
        });
        Ok(())
    }

    /// Sets the token generation time, releases the escrowed SOL to the treasury and returns unsold tokens.
    pub fn finalise(ctx: Context<Finalise>, tge_ts: i64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &mut ctx.accounts.round;
        require!(r.state == RoundState::Closed, SaleError::State);
        require!(
            now < r
                .end_ts
                .checked_add(FINALISE_GRACE)
                .ok_or(SaleError::Math)?,
            SaleError::Window
        );
        require!(
            tge_ts >= now && tge_ts <= now.checked_add(MAX_TGE_DELAY).ok_or(SaleError::Math)?,
            SaleError::Window
        );
        r.tge_ts = tge_ts;
        r.state = RoundState::Finalised;

        let escrow_lamports = ctx.accounts.escrow.lamports();
        let round_key = r.key();
        let seeds: &[&[u8]] = &[b"escrow", round_key.as_ref(), &[r.escrow_bump]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.escrow.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
                &[seeds],
            ),
            escrow_lamports,
        )?;

        let unsold = ctx
            .accounts
            .vault
            .amount
            .checked_sub(r.tokens_sold)
            .ok_or(SaleError::Math)?;
        if unsold > 0 {
            let vault_seeds: &[&[u8]] = &[b"round", &[r.round_id], &[r.bump]];
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.vault.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        to: ctx.accounts.unsold_destination.to_account_info(),
                        authority: r.to_account_info(),
                    },
                    &[vault_seeds],
                ),
                unsold,
                r.decimals,
            )?;
        }
        emit_cpi!(RoundFinalised {
            round: round_key,
            tge_ts,
            raised: r.raised,
            escrow_swept: escrow_lamports,
            tokens_sold: r.tokens_sold,
            unsold_returned: unsold
        });
        Ok(())
    }

    /// Pays the vested, unclaimed tokens to the buyer's token account.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let r = &ctx.accounts.round;
        require!(r.state == RoundState::Finalised, SaleError::State);
        require!(now >= r.tge_ts, SaleError::NotYetVested);
        let p = &mut ctx.accounts.participant;
        let vested = vested_amount(p.tokens, r.tge_bps, r.tge_ts, r.vest_seconds, now)?;
        let due = vested.checked_sub(p.claimed).ok_or(SaleError::Math)?;
        require!(due > 0, SaleError::NothingToClaim);
        let seeds: &[&[u8]] = &[b"round", &[r.round_id], &[r.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.buyer_token.to_account_info(),
                    authority: r.to_account_info(),
                },
                &[seeds],
            ),
            due,
            r.decimals,
        )?;
        p.claimed = vested;
        let (buyer_key, claimed_total, finished) = (p.buyer, p.claimed, p.claimed == p.tokens);
        let r = &mut ctx.accounts.round;
        r.tokens_claimed = r.tokens_claimed.checked_add(due).ok_or(SaleError::Math)?;
        emit_cpi!(Claimed {
            round: r.key(),
            buyer: buyer_key,
            tokens: due,
            claimed_total
        });
        if finished {
            // Everything is paid: the participant record closes and its rent returns to the buyer.
            ctx.accounts
                .participant
                .close(ctx.accounts.buyer.to_account_info())?;
        }
        Ok(())
    }

    /// Returns a participant's SOL after a cancelled round.
    pub fn refund(ctx: Context<Refund>) -> Result<()> {
        let r = &ctx.accounts.round;
        require!(r.state == RoundState::Cancelled, SaleError::State);
        let p = &mut ctx.accounts.participant;
        require!(!p.refunded && p.lamports > 0, SaleError::NothingToClaim);
        let round_key = r.key();
        let seeds: &[&[u8]] = &[b"escrow", round_key.as_ref(), &[r.escrow_bump]];
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.escrow.to_account_info(),
                    to: ctx.accounts.buyer.to_account_info(),
                },
                &[seeds],
            ),
            p.lamports,
        )?;
        p.refunded = true;
        emit_cpi!(Refunded {
            round: round_key,
            buyer: p.buyer,
            lamports: p.lamports
        });
        // The record closes with the refund (`close = buyer`), returning its rent to the buyer.
        Ok(())
    }

    /// After a cancellation, returns every token in the vault to the authority's destination.
    pub fn withdraw_cancelled(ctx: Context<WithdrawCancelled>) -> Result<()> {
        let r = &ctx.accounts.round;
        require!(r.state == RoundState::Cancelled, SaleError::State);
        let amount = ctx.accounts.vault.amount;
        require!(amount > 0, SaleError::NothingToClaim);
        let seeds: &[&[u8]] = &[b"round", &[r.round_id], &[r.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.destination.to_account_info(),
                    authority: r.to_account_info(),
                },
                &[seeds],
            ),
            amount,
            r.decimals,
        )?;
        emit_cpi!(CancelledWithdrawn {
            round: r.key(),
            destination: ctx.accounts.destination.key(),
            tokens: amount
        });
        Ok(())
    }

    /// Step one of a hand-over: names the key that may accept the round (a timelock PDA or a governance key).
    /// Nothing changes until that key calls `accept_authority`; naming the zero key clears a pending hand-over.
    pub fn set_authority(ctx: Context<Authority>, new_authority: Pubkey) -> Result<()> {
        require!(
            new_authority != ctx.accounts.round.authority,
            SaleError::ZeroKey
        );
        let r = &mut ctx.accounts.round;
        r.pending_authority = new_authority;
        emit_cpi!(AuthorityProposed {
            round: r.key(),
            from: r.authority,
            to: new_authority
        });
        Ok(())
    }

    /// Step two of a hand-over: the named key takes the round.
    pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
        let r = &mut ctx.accounts.round;
        require!(r.pending_authority != Pubkey::default(), SaleError::ZeroKey);
        let from = r.authority;
        r.authority = r.pending_authority;
        r.pending_authority = Pubkey::default();
        emit_cpi!(AuthorityChanged {
            round: r.key(),
            from,
            to: r.authority
        });
        Ok(())
    }
}

/* ---------------- arithmetic ---------------- */

/// Token base units for `lamports` at `lamports_per_token` (lamports per whole token). Floors.
pub fn tokens_for(lamports: u64, lamports_per_token: u64, decimals: u8) -> Result<u64> {
    require!(lamports_per_token > 0, SaleError::Price);
    let unit = 10u128
        .checked_pow(u32::from(decimals))
        .ok_or(SaleError::Math)?;
    let t = u128::from(lamports)
        .checked_mul(unit)
        .ok_or(SaleError::Math)?
        .checked_div(u128::from(lamports_per_token))
        .ok_or(SaleError::Math)?;
    u64::try_from(t).map_err(|_| error!(SaleError::Math))
}

/// Tokens unlocked at `now`: the TGE share, then the remainder linearly over `vest_seconds` from `tge_ts`.
pub fn vested_amount(
    total: u64,
    tge_bps: u16,
    tge_ts: i64,
    vest_seconds: i64,
    now: i64,
) -> Result<u64> {
    if now < tge_ts {
        return Ok(0);
    }
    let total128 = u128::from(total);
    let tge = total128
        .checked_mul(u128::from(tge_bps))
        .ok_or(SaleError::Math)?
        .checked_div(u128::from(BPS))
        .ok_or(SaleError::Math)?;
    if vest_seconds <= 0 {
        return u64::try_from(total128).map_err(|_| error!(SaleError::Math));
    }
    let elapsed = now.checked_sub(tge_ts).ok_or(SaleError::Math)?;
    if elapsed >= vest_seconds {
        return Ok(total);
    }
    let rest = total128.checked_sub(tge).ok_or(SaleError::Math)?;
    let elapsed_u = u128::try_from(elapsed).map_err(|_| error!(SaleError::Math))?;
    let vest_u = u128::try_from(vest_seconds).map_err(|_| error!(SaleError::Math))?;
    let linear = rest
        .checked_mul(elapsed_u)
        .ok_or(SaleError::Math)?
        .checked_div(vest_u)
        .ok_or(SaleError::Math)?;
    u64::try_from(tge.checked_add(linear).ok_or(SaleError::Math)?)
        .map_err(|_| error!(SaleError::Math))
}

/// Merkle membership: leaf = sha256(0x00 || address), node = sha256(0x01 || min || max).
pub fn verify_allowlist(root: &[u8; 32], who: &Pubkey, proof: &[[u8; 32]]) -> bool {
    let mut node = hashv(&[&[0u8], who.as_ref()]).to_bytes();
    for sib in proof {
        node = if node <= *sib {
            hashv(&[&[1u8], &node, sib]).to_bytes()
        } else {
            hashv(&[&[1u8], sib, &node]).to_bytes()
        };
    }
    node == *root
}

/* ---------------- state ---------------- */

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum RoundState {
    Pending,
    Open,
    Closed,
    Finalised,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct CreateRoundArgs {
    pub round_id: u8,
    pub lamports_per_token: u64,
    pub hard_cap: u64,
    pub soft_cap: u64,
    pub min_per_wallet: u64,
    pub max_per_wallet: u64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tge_bps: u16,
    pub vest_seconds: i64,
    pub allowlist_root: [u8; 32],
    pub allowlisted: bool,
}

#[account]
#[derive(InitSpace, Debug)]
pub struct Round {
    pub authority: Pubkey,
    /// Key named by `set_authority`; zero when no hand-over is pending.
    pub pending_authority: Pubkey,
    pub treasury: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub round_id: u8,
    pub decimals: u8,
    pub lamports_per_token: u64,
    pub hard_cap: u64,
    pub soft_cap: u64,
    pub min_per_wallet: u64,
    pub max_per_wallet: u64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tge_ts: i64,
    pub tge_bps: u16,
    pub vest_seconds: i64,
    pub allowlist_root: [u8; 32],
    pub allowlisted: bool,
    pub raised: u64,
    pub tokens_sold: u64,
    pub tokens_claimed: u64,
    pub participants: u32,
    pub state: RoundState,
    pub bump: u8,
    pub escrow_bump: u8,
    /// Space for later releases; always zero today.
    pub reserved: [u8; 64],
}

#[account]
#[derive(InitSpace, Debug)]
pub struct Participant {
    pub round: Pubkey,
    pub buyer: Pubkey,
    pub lamports: u64,
    pub tokens: u64,
    pub claimed: u64,
    pub refunded: bool,
    pub bump: u8,
}

/* ---------------- accounts ---------------- */

#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateRoundArgs)]
pub struct CreateRound<'info> {
    /// The deployer: must be this program's upgrade authority, so nobody else can create a round on this program.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// This program's own `ProgramData`; binds `authority` to the upgrade authority.
    #[account(
        address = bpf_loader_upgradeable::get_program_data_address(&crate::ID) @ SaleError::NotUpgradeAuthority,
        constraint = program_data.upgrade_authority_address == Some(authority.key()) @ SaleError::NotUpgradeAuthority,
    )]
    pub program_data: Account<'info, ProgramData>,
    /// CHECK: destination of the proceeds, recorded at creation and never changed; any address.
    pub treasury: UncheckedAccount<'info>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer = authority, space = 8 + Round::INIT_SPACE, seeds = [b"round", std::slice::from_ref(&args.round_id)], bump)]
    pub round: Box<Account<'info, Round>>,
    /// CHECK: system-owned PDA that holds the escrowed SOL; it has no data and is only moved by this program.
    /// Funded with its rent floor here.
    #[account(mut, seeds = [b"escrow", round.key().as_ref()], bump)]
    pub escrow: UncheckedAccount<'info>,
    #[account(init, payer = authority, seeds = [b"vault", round.key().as_ref()], bump, token::mint = mint, token::authority = round, token::token_program = token_program)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(constraint = token_program.key() == *mint.to_account_info().owner @ SaleError::TokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Authority<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ SaleError::Authority, has_one = vault @ SaleError::Vault)]
    pub round: Box<Account<'info, Round>>,
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Anyone<'info> {
    pub caller: Signer<'info>,
    #[account(mut)]
    pub round: Box<Account<'info, Round>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Contribute<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(mut, seeds = [b"round", std::slice::from_ref(&round.round_id)], bump = round.bump)]
    pub round: Box<Account<'info, Round>>,
    /// CHECK: escrow PDA; seeds checked, lamports only.
    #[account(mut, seeds = [b"escrow", round.key().as_ref()], bump = round.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = buyer, space = 8 + Participant::INIT_SPACE, seeds = [b"participant", round.key().as_ref(), buyer.key().as_ref()], bump)]
    pub participant: Box<Account<'info, Participant>>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Finalise<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority @ SaleError::Authority, has_one = treasury @ SaleError::Treasury, has_one = vault @ SaleError::Vault, has_one = mint @ SaleError::Mint)]
    pub round: Box<Account<'info, Round>>,
    /// CHECK: escrow PDA; seeds checked, drained to the treasury.
    #[account(mut, seeds = [b"escrow", round.key().as_ref()], bump = round.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,
    /// CHECK: must equal `round.treasury`, enforced by `has_one`.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub unsold_destination: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    #[account(mut, has_one = vault @ SaleError::Vault, has_one = mint @ SaleError::Mint)]
    pub round: Box<Account<'info, Round>>,
    #[account(mut, seeds = [b"participant", round.key().as_ref(), buyer.key().as_ref()], bump = participant.bump, has_one = buyer @ SaleError::Authority)]
    pub participant: Box<Account<'info, Participant>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = mint, token::authority = buyer, token::token_program = token_program)]
    pub buyer_token: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Refund<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,
    pub round: Box<Account<'info, Round>>,
    #[account(mut, close = buyer, seeds = [b"participant", round.key().as_ref(), buyer.key().as_ref()], bump = participant.bump, has_one = buyer @ SaleError::Authority)]
    pub participant: Box<Account<'info, Participant>>,
    /// CHECK: escrow PDA; seeds checked, lamports only.
    #[account(mut, seeds = [b"escrow", round.key().as_ref()], bump = round.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawCancelled<'info> {
    pub authority: Signer<'info>,
    #[account(has_one = authority @ SaleError::Authority, has_one = vault @ SaleError::Vault, has_one = mint @ SaleError::Mint)]
    pub round: Box<Account<'info, Round>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub vault: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub destination: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptAuthority<'info> {
    pub new_authority: Signer<'info>,
    #[account(mut, constraint = round.pending_authority == new_authority.key() @ SaleError::Authority)]
    pub round: Box<Account<'info, Round>>,
}

/* ---------------- events ---------------- */

#[event]
pub struct RoundCreated {
    pub round: Pubkey,
    pub round_id: u8,
    pub lamports_per_token: u64,
    pub hard_cap: u64,
    pub soft_cap: u64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tge_bps: u16,
    pub vest_seconds: i64,
}
#[event]
pub struct RoundOpened {
    pub round: Pubkey,
    pub cap_tokens: u64,
    pub vault_tokens: u64,
}
#[event]
pub struct Contributed {
    pub round: Pubkey,
    pub buyer: Pubkey,
    pub lamports: u64,
    pub tokens: u64,
    pub raised: u64,
    pub tokens_sold: u64,
}
#[event]
pub struct RoundClosed {
    pub round: Pubkey,
    pub raised: u64,
    pub tokens_sold: u64,
    pub cancelled: bool,
}
#[event]
pub struct RoundFinalised {
    pub round: Pubkey,
    pub tge_ts: i64,
    /// Sum of contributions.
    pub raised: u64,
    /// Lamports moved to the treasury: `raised`, the escrow rent floor and any donations.
    pub escrow_swept: u64,
    pub tokens_sold: u64,
    pub unsold_returned: u64,
}
#[event]
pub struct Claimed {
    pub round: Pubkey,
    pub buyer: Pubkey,
    pub tokens: u64,
    pub claimed_total: u64,
}
#[event]
pub struct Refunded {
    pub round: Pubkey,
    pub buyer: Pubkey,
    pub lamports: u64,
}
#[event]
pub struct AuthorityProposed {
    pub round: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
}
#[event]
pub struct AuthorityChanged {
    pub round: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
}
#[event]
pub struct CancelledWithdrawn {
    pub round: Pubkey,
    pub destination: Pubkey,
    pub tokens: u64,
}

/* ---------------- errors ---------------- */

#[error_code]
pub enum SaleError {
    #[msg("The price must be a positive number of lamports per token")]
    Price,
    #[msg("The soft cap must be positive and not exceed the hard cap")]
    Caps,
    #[msg("The contribution is outside this wallet's bounds for the round")]
    WalletBounds,
    #[msg("The round is not in its contribution window")]
    Window,
    #[msg("The unlock share and vesting length are inconsistent")]
    Unlock,
    #[msg("The round is not in the required state for this instruction")]
    State,
    #[msg("The vault does not hold enough tokens to honour the hard cap")]
    Unfunded,
    #[msg("This wallet is not on the round's allowlist")]
    Allowlist,
    #[msg("The contribution would exceed the hard cap")]
    RoundFull,
    #[msg("The contribution is too small to buy a token")]
    Dust,
    #[msg("Nothing is vested or due for this wallet yet")]
    NotYetVested,
    #[msg("Nothing to claim or refund")]
    NothingToClaim,
    #[msg("Arithmetic overflow")]
    Math,
    #[msg("Only the round authority may do this")]
    Authority,
    #[msg("The treasury does not match the round")]
    Treasury,
    #[msg("The vault does not match the round")]
    Vault,
    #[msg("The mint does not match the round")]
    Mint,
    #[msg("The token program does not own the mint")]
    TokenProgram,
    #[msg("Only the program's upgrade authority may create a round")]
    NotUpgradeAuthority,
    #[msg("The proposed authority is empty or unchanged")]
    ZeroKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_floor_and_fit() {
        // 1 SOL at 10_000 lamports per token with 9 decimals: 100_000 tokens.
        assert_eq!(
            tokens_for(1_000_000_000, 10_000, 9).unwrap(),
            100_000 * 1_000_000_000
        );
        assert_eq!(tokens_for(15_000, 10_000, 9).unwrap(), 1_500_000_000);
        assert_eq!(tokens_for(1, 10_000, 9).unwrap(), 100_000);
        assert!(tokens_for(u64::MAX, 1, 9).is_err());
    }

    #[test]
    fn vesting_curve() {
        let total = 1_000_000_000_000u64; // 1 000 tokens
        let tge = 1_700_000_000;
        let vest = 60 * 86_400;
        assert_eq!(vested_amount(total, 2_500, tge, vest, tge - 1).unwrap(), 0);
        assert_eq!(
            vested_amount(total, 2_500, tge, vest, tge).unwrap(),
            total / 4
        );
        assert_eq!(
            vested_amount(total, 2_500, tge, vest, tge + vest / 2).unwrap(),
            total / 4 + (total - total / 4) / 2
        );
        assert_eq!(
            vested_amount(total, 2_500, tge, vest, tge + vest).unwrap(),
            total
        );
        assert_eq!(
            vested_amount(total, 2_500, tge, vest, tge + vest * 3).unwrap(),
            total
        );
        assert_eq!(vested_amount(total, 10_000, tge, 0, tge).unwrap(), total);
    }

    #[test]
    fn allowlist_two_leaves() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let la = hashv(&[&[0u8], a.as_ref()]).to_bytes();
        let lb = hashv(&[&[0u8], b.as_ref()]).to_bytes();
        let root = if la <= lb {
            hashv(&[&[1u8], &la, &lb])
        } else {
            hashv(&[&[1u8], &lb, &la])
        }
        .to_bytes();
        assert!(verify_allowlist(&root, &a, &[lb]));
        assert!(verify_allowlist(&root, &b, &[la]));
        assert!(!verify_allowlist(&root, &Pubkey::new_unique(), &[la]));
        assert!(!verify_allowlist(&root, &a, &[]));
    }
}
