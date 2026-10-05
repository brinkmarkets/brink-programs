//! Re-entrancy probe. Invoked by `swap_amm` at the enabled hook points with the read-only pool (and swap) and a
//! one-byte point discriminator followed by the payload. Behaviour depends on the point so that one program
//! exercises several attacks (a pool enables one or two points per environment):
//!
//! * point 0 (before open): attempts a CPI back into `swap_amm::sync_vault` with the pool passed read-only, exactly
//!   as received (no privilege escalation): the runtime refuses the re-entry into a program already on the
//!   invocation stack and the whole transaction fails. A failed CPI cannot be caught by the caller on Solana.
//! * point 1 (after open): writes one byte into the pool account data (read-only to the hook); the runtime must
//!   reject the outer instruction.
//! * point 2 (before cancel): attempts a CPI into `swap_amm::trader_cancel_swap` with the pool and swap marked
//!   writable although they were received read-only: the runtime rejects the privilege escalation.
//! * point 6 (before withdraw): vetoes (returns an error).
//! * any other point: observes and returns Ok.
#![allow(unexpected_cfgs)]

use solana_account_info::AccountInfo;
use solana_cpi::invoke;
use solana_instruction::{AccountMeta, Instruction};
use solana_msg::msg;
use solana_program_error::ProgramError;
use solana_pubkey::Pubkey;

pub const SWAP_AMM: Pubkey = Pubkey::from_str_const("EbD862QySygYKCMd8RHM2JaHqnduLQXB8Y632U3pQtce");

fn disc(name: &str) -> [u8; 8] {
    // sha256("global:<name>")[..8], precomputed to avoid pulling a hash crate into the sBPF build
    match name {
        "sync_vault" => SYNC_VAULT,
        _ => CANCEL,
    }
}
// sha256("global:sync_vault")[..8] and sha256("global:trader_cancel_swap")[..8]; verified by the host-side test.
pub const SYNC_VAULT: [u8; 8] = [0x13, 0xd3, 0x96, 0x76, 0x5e, 0xd0, 0x8a, 0xcc];
pub const CANCEL: [u8; 8] = [0xb3, 0x2e, 0xdc, 0x99, 0x02, 0x97, 0x55, 0xf6];

solana_program_entrypoint::entrypoint!(process);

pub fn process(_program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> Result<(), ProgramError> {
    let point = data.first().copied().unwrap_or(255);
    let pool = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    msg!("hook: point {} pool writable {} signer {}", point, pool.is_writable, pool.is_signer);
    match point {
        0 => {
            let ix = Instruction { program_id: SWAP_AMM, accounts: vec![AccountMeta::new_readonly(*pool.key, false), AccountMeta::new_readonly(*pool.key, false)], data: disc("sync_vault").to_vec() };
            match invoke(&ix, &[pool.clone(), pool.clone()]) {
                Ok(()) => msg!("hook: re-entry into sync_vault SUCCEEDED"),
                Err(e) => msg!("hook: re-entry into sync_vault rejected: {:?}", e),
            }
            Ok(())
        }
        1 => {
            let mut d = pool.try_borrow_mut_data()?;
            if d.len() > 8 {
                d[8] ^= 0xff;
            }
            msg!("hook: wrote into read-only pool data");
            Ok(())
        }
        2 => {
            let swap = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
            let mut d = disc("trader_cancel_swap").to_vec();
            d.extend_from_slice(&0u64.to_le_bytes());
            let ix = Instruction { program_id: SWAP_AMM, accounts: vec![AccountMeta::new(*pool.key, false), AccountMeta::new(*swap.key, false)], data: d };
            match invoke(&ix, &[pool.clone(), swap.clone()]) {
                Ok(()) => msg!("hook: re-entry into trader_cancel_swap SUCCEEDED"),
                Err(e) => msg!("hook: re-entry into trader_cancel_swap rejected: {:?}", e),
            }
            Ok(())
        }
        6 => {
            msg!("hook: veto");
            Err(ProgramError::Custom(0x4e5a))
        }
        _ => Ok(()),
    }
}
