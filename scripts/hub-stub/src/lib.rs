//! A stand-in for the hub in LiteSVM tests (the bot's
//! scripts/test-phoenix-flow.mjs): loaded at the hub's program id, `relay`
//! makes one call signed as a DAO's treasury PDA `[TREASURY_SEED, dao]`, as
//! an executed proposal does. Remaining accounts: the program, then its
//! accounts in order. Anyone can call it: a test tool only. Never deploy it.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

declare_id!("5m9N12e9seKNxEXFBJSKr5vd9uryXpckSakfrzDWxMKe");

const TREASURY_SEED: &[u8] = b"treasury";

#[program]
pub mod hub_stub {
    use super::*;

    pub fn relay<'info>(ctx: Context<'info, Relay>, dao: Pubkey, data: Vec<u8>) -> Result<()> {
        let (treasury, bump) = Pubkey::find_program_address(&[TREASURY_SEED, dao.as_ref()], &crate::ID);
        let (program, accounts) = ctx.remaining_accounts.split_first().ok_or(ProgramError::NotEnoughAccountKeys)?;
        let metas = accounts
            .iter()
            .map(|a| {
                let signer = a.is_signer || a.key() == treasury;
                if a.is_writable { AccountMeta::new(a.key(), signer) } else { AccountMeta::new_readonly(a.key(), signer) }
            })
            .collect();
        let mut infos = accounts.to_vec();
        infos.push(program.clone());
        invoke_signed(&Instruction { program_id: program.key(), accounts: metas, data }, &infos, &[&[TREASURY_SEED, dao.as_ref(), &[bump]]])?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Relay {}
