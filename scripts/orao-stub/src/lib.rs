//! A stand-in for ORAO VRF on a local validator, for end-to-end tests of
//! sortition draws (the bot's scripts/test-solana-new-models.mjs). ORAO's
//! published npm package ships an older build without `request_v2`, and
//! devnet's program can't be cloned everywhere, so this answers the same
//! three instructions with ORAO's discriminators and account layouts
//! (from its IDL, @orao-network/solana-vrf 0.8.0):
//!
//! - `init_network(fee, config_authority, fulfillment_authorities, token_fee_config)`:
//!   the network state PDA, `[NetworkState disc][authority][treasury][fee]...`.
//! - `request_v2(seed)`: the request PDA `[b"orao-vrf-randomness-request", seed]`,
//!   `[RandomnessV2 disc][0 = pending][client][seed]`, paying `fee` to the treasury.
//! - `fulfill_v2()`: by a fulfillment authority, sets the request to
//!   `[1 = fulfilled][client][seed][64 random bytes]`. Real ORAO checks an
//!   ed25519 signature of the seed and uses it as the randomness; this takes
//!   sha256(seed) twice. Not random - a test tool only. Never deploy it.

use anchor_lang::prelude::*;
use solana_sha256_hasher::hashv;
use anchor_lang::system_program::{create_account, transfer, CreateAccount, Transfer};

declare_id!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");

const STATE_SEED: &[u8] = b"orao-vrf-network-configuration";
const REQUEST_SEED: &[u8] = b"orao-vrf-randomness-request";
const NETWORK_STATE: [u8; 8] = [212, 237, 148, 56, 97, 245, 51, 169];
const RANDOMNESS_V2: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];
const STATE_LEN: usize = 8 + 32 + 32 + 8 + 4 + 32 * 4;
const REQUEST_LEN: usize = 8 + 1 + 32 + 32 + 64;

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct OraoTokenFeeConfig {
    pub mint: Pubkey,
    pub treasury: Pubkey,
    pub fee: u64,
}

#[error_code]
pub enum StubError {
    #[msg("Wrong account")]
    WrongAccount,
    #[msg("Not a fulfillment authority")]
    NotFulfiller,
}

fn make_pda<'info>(payer: &Signer<'info>, account: &UncheckedAccount<'info>, system: &Program<'info, System>, seeds: &[&[u8]], space: usize) -> Result<()> {
    let lamports = Rent::get()?.minimum_balance(space);
    create_account(
        CpiContext::new_with_signer(system.key(), CreateAccount { from: payer.to_account_info(), to: account.to_account_info() }, &[seeds]),
        lamports,
        space as u64,
        &crate::ID,
    )
}

#[program]
pub mod orao_stub {
    use super::*;

    pub fn init_network(ctx: Context<InitNetwork>, fee: u64, config_authority: Pubkey, fulfillment_authorities: Vec<Pubkey>, _token_fee_config: Option<OraoTokenFeeConfig>) -> Result<()> {
        let (state, bump) = Pubkey::find_program_address(&[STATE_SEED], &crate::ID);
        require_keys_eq!(state, ctx.accounts.network_state.key(), StubError::WrongAccount);
        require!(fulfillment_authorities.len() <= 4, StubError::WrongAccount);
        make_pda(&ctx.accounts.payer, &ctx.accounts.network_state, &ctx.accounts.system_program, &[STATE_SEED, &[bump]], STATE_LEN)?;
        let mut data = ctx.accounts.network_state.try_borrow_mut_data()?;
        data[..8].copy_from_slice(&NETWORK_STATE);
        data[8..40].copy_from_slice(config_authority.as_ref());
        data[40..72].copy_from_slice(ctx.accounts.treasury.key().as_ref());
        data[72..80].copy_from_slice(&fee.to_le_bytes());
        data[80..84].copy_from_slice(&(fulfillment_authorities.len() as u32).to_le_bytes());
        for (i, k) in fulfillment_authorities.iter().enumerate() {
            data[84 + 32 * i..116 + 32 * i].copy_from_slice(k.as_ref());
        }
        Ok(())
    }

    pub fn request_v2(ctx: Context<Request>, seed: [u8; 32]) -> Result<()> {
        let (request, bump) = Pubkey::find_program_address(&[REQUEST_SEED, &seed], &crate::ID);
        require_keys_eq!(request, ctx.accounts.request.key(), StubError::WrongAccount);
        let (treasury, fee) = {
            let state = ctx.accounts.network_state.try_borrow_data()?;
            require!(state.len() == STATE_LEN && state[..8] == NETWORK_STATE, StubError::WrongAccount);
            (Pubkey::try_from(&state[40..72]).unwrap(), u64::from_le_bytes(state[72..80].try_into().unwrap()))
        };
        require_keys_eq!(treasury, ctx.accounts.treasury.key(), StubError::WrongAccount);
        transfer(CpiContext::new(ctx.accounts.system_program.key(), Transfer { from: ctx.accounts.payer.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), fee)?;
        make_pda(&ctx.accounts.payer, &ctx.accounts.request, &ctx.accounts.system_program, &[REQUEST_SEED, &seed, &[bump]], REQUEST_LEN)?;
        let mut data = ctx.accounts.request.try_borrow_mut_data()?;
        data[..8].copy_from_slice(&RANDOMNESS_V2);
        data[8] = 0;
        data[9..41].copy_from_slice(ctx.accounts.payer.key().as_ref());
        data[41..73].copy_from_slice(&seed);
        Ok(())
    }

    pub fn fulfill_v2(ctx: Context<Fulfill>) -> Result<()> {
        {
            let state = ctx.accounts.network_state.try_borrow_data()?;
            require!(state.len() == STATE_LEN && state[..8] == NETWORK_STATE, StubError::WrongAccount);
            let n = u32::from_le_bytes(state[80..84].try_into().unwrap()) as usize;
            let payer = ctx.accounts.payer.key();
            require!((0..n).any(|i| state[84 + 32 * i..116 + 32 * i] == payer.as_ref()[..]), StubError::NotFulfiller);
        }
        require_keys_eq!(*ctx.accounts.request.owner, crate::ID, StubError::WrongAccount);
        let mut data = ctx.accounts.request.try_borrow_mut_data()?;
        require!(data.len() == REQUEST_LEN && data[..8] == RANDOMNESS_V2 && data[8] == 0, StubError::WrongAccount);
        let h = hashv(&[&data[41..73]]).to_bytes();
        data[8] = 1;
        data[73..105].copy_from_slice(&h);
        data[105..137].copy_from_slice(&hashv(&[&h]).to_bytes());
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitNetwork<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: the network state PDA, made here.
    #[account(mut)]
    pub network_state: UncheckedAccount<'info>,
    /// CHECK: where request fees go.
    pub treasury: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Request<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: checked against its layout.
    #[account(mut)]
    pub network_state: UncheckedAccount<'info>,
    /// CHECK: checked against the network state.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the request PDA, made here.
    #[account(mut)]
    pub request: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Fulfill<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: ORAO's instructions sysvar slot (unused here).
    pub instruction_acc: UncheckedAccount<'info>,
    /// CHECK: checked against its layout.
    #[account(mut)]
    pub network_state: UncheckedAccount<'info>,
    /// CHECK: checked against its layout.
    #[account(mut)]
    pub request: UncheckedAccount<'info>,
    /// CHECK: the requester (unused here).
    #[account(mut)]
    pub client: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}
