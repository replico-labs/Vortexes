//! tokenWeighted end to end: the compiled program in LiteSVM, with the
//! real SPL Token and Associated Token programs.

use anchor_lang::{prelude::Pubkey, AccountDeserialize, InstructionData, ToAccountMetas};
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
use solana_clock::Clock;
use solana_instruction::Instruction;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::{Transaction, TransactionError};
use vortex_core::{GovError, ProposalState, StoredAccountMeta, StoredInstruction, VotingConfig, TREASURY_SEED};
use vortex_token_weighted::{self as tw, Dao, Proposal, VoteChoice, Voter};

const SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/vortex_token_weighted.so");
const START: i64 = 1_800_000_000;

fn config() -> VotingConfig {
    VotingConfig {
        quorum_bps: 1_000,   // 10%
        approval_bps: 6_000, // 60%
        voting_delay: 10,
        voting_period: 100,
        timelock: 50,
        execution_period: 200,
        proposal_threshold: 100,
    }
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    mint: Pubkey,
    dao: Pubkey,
    vault: Pubkey,
    treasury: Pubkey,
    token_program: Pubkey,
}

struct Member {
    kp: Keypair,
    ata: Pubkey,
}

impl Env {
    fn new() -> Self {
        Self::with_token_program(litesvm_token::TOKEN_ID)
    }

    fn with_token_program(token_program: Pubkey) -> Self {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(tw::ID, SO).expect("build the program first: cargo-build-sbf");
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = START;
        svm.set_sysvar(&clock);

        let mint = if token_program == litesvm_token::TOKEN_ID {
            CreateMint::new(&mut svm, &payer).decimals(6).send().unwrap()
        } else {
            create_mint_2022(&mut svm, &payer)
        };
        let create_key = Keypair::new();
        let (dao, _) = Pubkey::find_program_address(&[tw::DAO_SEED, create_key.pubkey().as_ref()], &tw::ID);
        let (vault, _) = Pubkey::find_program_address(&[tw::VAULT_SEED, dao.as_ref()], &tw::ID);
        let (treasury, _) = Pubkey::find_program_address(&[TREASURY_SEED, dao.as_ref()], &tw::ID);
        let mut env = Env { svm, payer, mint, dao, vault, treasury, token_program };
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::CreateDao {
                payer: env.payer.pubkey(),
                create_key: create_key.pubkey(),
                mint,
                dao,
                vault,
                token_program,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: tw::instruction::CreateDao { name: "Ark".into(), config: config() }.data(),
        };
        let payer = env.payer.insecure_clone();
        env.send(&[ix], &[&payer, &create_key]).unwrap();
        env
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), FailedTransactionMetadata> {
        let fee_payer = signers[0].pubkey();
        let tx = Transaction::new(signers, Message::new(ixs, Some(&fee_payer)), self.svm.latest_blockhash());
        let res = self.svm.send_transaction(tx).map(|_| ());
        self.svm.expire_blockhash();
        res
    }

    fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }

    fn warp(&mut self, to: i64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = to;
        self.svm.set_sysvar(&clock);
    }

    fn member(&mut self, tokens: u64) -> Member {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
        let ata = if self.token_program == litesvm_token::TOKEN_ID {
            let ata = CreateAssociatedTokenAccount::new(&mut self.svm, &self.payer, &self.mint).owner(&kp.pubkey()).send().unwrap();
            if tokens > 0 {
                MintTo::new(&mut self.svm, &self.payer, &self.mint, &ata, tokens).send().unwrap();
            }
            ata
        } else {
            self.ata_2022(&kp.pubkey(), tokens)
        };
        Member { kp, ata }
    }

    fn voter_pda(&self, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[tw::VOTER_SEED, self.dao.as_ref(), owner.as_ref()], &tw::ID).0
    }

    fn proposal_pda(&self, id: u64) -> Pubkey {
        Pubkey::find_program_address(&[tw::PROPOSAL_SEED, self.dao.as_ref(), &id.to_le_bytes()], &tw::ID).0
    }

    fn vote_pda(&self, proposal: &Pubkey, owner: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[tw::VOTE_SEED, proposal.as_ref(), owner.as_ref()], &tw::ID).0
    }

    fn deposit(&mut self, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::Deposit {
                owner: m.kp.pubkey(),
                dao: self.dao,
                voter: self.voter_pda(&m.kp.pubkey()),
                owner_token_account: m.ata,
                vault: self.vault,
                mint: self.mint,
                token_program: self.token_program,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: tw::instruction::Deposit { amount }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    fn withdraw(&mut self, m: &Member, amount: u64) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::Withdraw {
                owner: m.kp.pubkey(),
                dao: self.dao,
                voter: self.voter_pda(&m.kp.pubkey()),
                owner_token_account: m.ata,
                vault: self.vault,
                mint: self.mint,
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: tw::instruction::Withdraw { amount }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    fn propose(&mut self, m: &Member, instructions: Vec<StoredInstruction>) -> Result<(u64, Pubkey), FailedTransactionMetadata> {
        let id = self.dao_account().proposal_count + 1;
        let proposal = self.proposal_pda(id);
        let voter = self.voter_pda(&m.kp.pubkey());
        let has_voter = self.svm.get_account(&voter).is_some_and(|a| a.lamports > 0);
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::Propose {
                proposer: m.kp.pubkey(),
                dao: self.dao,
                voter: has_voter.then_some(voter),
                proposal,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: tw::instruction::Propose { id, metadata_uri: "Pay the community call host".into(), instructions }.data(),
        };
        self.send(&[ix], &[&m.kp]).map(|_| (id, proposal))
    }

    fn vote(&mut self, m: &Member, proposal: Pubkey, choice: VoteChoice) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::CastVote {
                owner: m.kp.pubkey(),
                dao: self.dao,
                proposal,
                voter: self.voter_pda(&m.kp.pubkey()),
                vote_record: self.vote_pda(&proposal, &m.kp.pubkey()),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: tw::instruction::CastVote { choice }.data(),
        };
        self.send(&[ix], &[&m.kp])
    }

    fn queue(&mut self, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::Queue { dao: self.dao, proposal }.to_account_metas(None),
            data: tw::instruction::Queue {}.data(),
        };
        let payer = self.payer.insecure_clone();
        self.send(&[ix], &[&payer])
    }

    /// Execute, passing every account the stored instructions use.
    fn execute(&mut self, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let p = self.proposal_account(proposal);
        let mut accounts = tw::accounts::Execute { dao: self.dao, proposal }.to_account_metas(None);
        for ix in &p.instructions {
            for m in &ix.accounts {
                // The treasury signs inside the program, not in the transaction.
                accounts.push(anchor_lang::prelude::AccountMeta { pubkey: m.pubkey, is_signer: false, is_writable: m.is_writable });
            }
            accounts.push(anchor_lang::prelude::AccountMeta::new_readonly(ix.program_id, false));
        }
        let ix = Instruction { program_id: tw::ID, accounts, data: tw::instruction::Execute {}.data() };
        let payer = self.payer.insecure_clone();
        self.send(&[ix], &[&payer])
    }

    fn cancel(&mut self, who: &Keypair, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::Cancel { authority: who.pubkey(), dao: self.dao, proposal }.to_account_metas(None),
            data: tw::instruction::Cancel {}.data(),
        };
        self.send(&[ix], &[who])
    }

    fn dao_account(&self) -> Dao {
        Dao::try_deserialize(&mut self.svm.get_account(&self.dao).unwrap().data.as_slice()).unwrap()
    }

    fn proposal_account(&self, p: Pubkey) -> Proposal {
        Proposal::try_deserialize(&mut self.svm.get_account(&p).unwrap().data.as_slice()).unwrap()
    }

    fn voter_account(&self, owner: &Pubkey) -> Voter {
        Voter::try_deserialize(&mut self.svm.get_account(&self.voter_pda(owner)).unwrap().data.as_slice()).unwrap()
    }

    fn token_balance(&self, account: &Pubkey) -> u64 {
        litesvm_token::get_spl_account::<litesvm_token::spl_token::state::Account>(&self.svm, account).unwrap().amount
    }

    fn state(&self, proposal: Pubkey) -> ProposalState {
        self.proposal_account(proposal).state(&self.dao_account().config, self.now())
    }

    /// A stored instruction: send `lamports` from the treasury.
    fn pay_sol(&self, to: &Pubkey, lamports: u64) -> StoredInstruction {
        stored(solana_system_interface::instruction::transfer(&self.treasury, to, lamports))
    }

    /// A stored instruction calling this program's update_config.
    fn update_config_ix(&self, config: VotingConfig) -> StoredInstruction {
        stored(Instruction {
            program_id: tw::ID,
            accounts: tw::accounts::UpdateConfig { treasury: self.treasury, dao: self.dao }.to_account_metas(None),
            data: tw::instruction::UpdateConfig { config }.data(),
        })
    }
}

/// A Token-2022 mint (no extensions) with `payer` as mint authority.
fn create_mint_2022(svm: &mut LiteSVM, payer: &Keypair) -> Pubkey {
    use spl_token_2022_interface::{instruction::initialize_mint2, state::Mint, ID};
    use anchor_lang::solana_program::program_pack::Pack;
    let mint = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(Mint::LEN);
    let ixs = [
        solana_system_interface::instruction::create_account(&payer.pubkey(), &mint.pubkey(), rent, Mint::LEN as u64, &ID),
        initialize_mint2(&ID, &mint.pubkey(), &payer.pubkey(), None, 6).unwrap(),
    ];
    let tx = Transaction::new(&[payer, &mint], Message::new(&ixs, Some(&payer.pubkey())), svm.latest_blockhash());
    svm.send_transaction(tx).unwrap();
    mint.pubkey()
}

impl Env {
    /// `owner`'s Token-2022 associated account, holding `tokens`.
    fn ata_2022(&mut self, owner: &Pubkey, tokens: u64) -> Pubkey {
        use spl_token_2022_interface::{instruction::mint_to_checked, ID};
        let ata = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(owner, &self.mint, &ID);
        let mut ixs = vec![spl_associated_token_account_interface::instruction::create_associated_token_account(&self.payer.pubkey(), owner, &self.mint, &ID)];
        if tokens > 0 {
            ixs.push(mint_to_checked(&ID, &self.mint, &ata, &self.payer.pubkey(), &[], tokens, 6).unwrap());
        }
        let payer = self.payer.insecure_clone();
        self.send(&ixs, &[&payer]).unwrap();
        ata
    }
}

fn stored(ix: Instruction) -> StoredInstruction {
    StoredInstruction {
        program_id: ix.program_id,
        accounts: ix.accounts.iter().map(|m| StoredAccountMeta { pubkey: m.pubkey, is_signer: m.is_signer, is_writable: m.is_writable }).collect(),
        data: ix.data,
    }
}

/// The custom error a failed transaction ended with.
fn code<T: std::fmt::Debug>(res: Result<T, FailedTransactionMetadata>) -> u32 {
    match res.expect_err("expected the transaction to fail").err {
        TransactionError::InstructionError(_, InstructionError::Custom(c)) => c,
        other => panic!("not a custom error: {other:?}"),
    }
}

fn err(e: GovError) -> u32 {
    u32::from(e)
}

/// Alice 600, Bob 400 deposited; returns them plus a funded treasury.
fn dao_with_members() -> (Env, Member, Member) {
    let mut env = Env::new();
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(&alice, 600).unwrap();
    env.deposit(&bob, 400).unwrap();
    env.svm.airdrop(&env.treasury, 5_000_000_000).unwrap();
    (env, alice, bob)
}

#[test]
fn full_lifecycle_pays_out_from_the_treasury() {
    let (mut env, alice, bob) = dao_with_members();
    assert_eq!(env.dao_account().total_deposited, 1_000);
    assert_eq!(env.token_balance(&env.vault), 1_000);

    // The treasury also holds tokens; the proposal pays SOL and tokens.
    let host = Keypair::new();
    let treasury_ata = CreateAssociatedTokenAccount::new(&mut env.svm, &env.payer, &env.mint).owner(&env.treasury).send().unwrap();
    MintTo::new(&mut env.svm, &env.payer, &env.mint, &treasury_ata, 500).send().unwrap();
    let host_ata = CreateAssociatedTokenAccount::new(&mut env.svm, &env.payer, &env.mint).owner(&host.pubkey()).send().unwrap();
    let pay_tokens = stored(
        spl_token_interface::instruction::transfer_checked(&litesvm_token::TOKEN_ID, &treasury_ata, &env.mint, &host_ata, &env.treasury, &[], 250, 6).unwrap(),
    );
    let (id, p) = env.propose(&alice, vec![env.pay_sol(&host.pubkey(), 1_000_000_000), pay_tokens]).unwrap();
    assert_eq!(id, 1);
    let prop = env.proposal_account(p);
    assert_eq!(prop.quorum_base, 1_000);
    assert_eq!(prop.voting_starts_at, START + 10);
    assert_eq!(env.state(p), ProposalState::Pending);

    assert_eq!(code(env.vote(&alice, p, VoteChoice::For)), err(GovError::ProposalNotActive), "voting not open yet");
    env.warp(START + 10);
    assert_eq!(env.state(p), ProposalState::Active);
    env.vote(&alice, p, VoteChoice::For).unwrap();
    env.vote(&bob, p, VoteChoice::Against).unwrap();
    let t = env.proposal_account(p).tally;
    assert_eq!((t.for_votes, t.against_votes, t.abstain_votes), (600, 400, 0));

    assert_eq!(code(env.queue(p)), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    assert_eq!(env.state(p), ProposalState::Succeeded, "60% for meets a 60% threshold");
    env.queue(p).unwrap();
    assert_eq!(env.state(p), ProposalState::Queued);
    assert_eq!(code(env.execute(p)), err(GovError::ProposalNotExecutable), "timelock not over");

    env.warp(START + 160);
    let before = env.svm.get_account(&host.pubkey()).map_or(0, |a| a.lamports);
    env.execute(p).unwrap();
    assert_eq!(env.svm.get_account(&host.pubkey()).unwrap().lamports - before, 1_000_000_000);
    assert_eq!(env.token_balance(&host_ata), 250);
    assert_eq!(env.token_balance(&treasury_ata), 250);
    assert_eq!(env.state(p), ProposalState::Executed);
    assert_eq!(code(env.execute(p)), err(GovError::ProposalAlreadyExecuted));
}

#[test]
fn one_vote_each_and_votes_lock_tokens_until_the_end() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&bob, p, VoteChoice::For).unwrap();
    assert!(env.vote(&bob, p, VoteChoice::Against).is_err(), "second vote refused (vote record exists)");
    assert_eq!(env.proposal_account(p).tally.for_votes, 400, "first vote unchanged");

    // Bob can't move his voting tokens to another wallet mid-vote.
    assert_eq!(env.voter_account(&bob.kp.pubkey()).locked_until, START + 110);
    assert_eq!(code(env.withdraw(&bob, 400)), err(GovError::TokensLocked));
    // Alice didn't vote, so she can withdraw any time.
    env.withdraw(&alice, 100).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 500);
    assert_eq!(env.dao_account().total_deposited, 900);

    env.warp(START + 110);
    assert_eq!(code(env.withdraw(&bob, 401)), err(GovError::InsufficientDeposit));
    env.withdraw(&bob, 400).unwrap();
    assert_eq!(env.token_balance(&bob.ata), 1_000);
    assert_eq!(env.voter_account(&bob.kp.pubkey()).amount, 0);
}

#[test]
fn proposing_needs_the_threshold_and_valid_instructions() {
    let (mut env, alice, _bob) = dao_with_members();
    let carol = env.member(1_000);
    // No deposit at all, then too little.
    assert_eq!(code(env.propose(&carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)])), err(GovError::ProposalThresholdNotMet));
    env.deposit(&carol, 99).unwrap();
    assert_eq!(code(env.propose(&carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)])), err(GovError::ProposalThresholdNotMet));
    env.deposit(&carol, 1).unwrap();
    env.propose(&carol, vec![env.pay_sol(&carol.kp.pubkey(), 1)]).unwrap();

    // Only the treasury can be asked to sign.
    let mut bad = env.pay_sol(&alice.kp.pubkey(), 1);
    bad.accounts[0].pubkey = alice.kp.pubkey();
    assert_eq!(code(env.propose(&alice, vec![bad])), err(GovError::InvalidProposalSigner));
    assert_eq!(code(env.propose(&alice, vec![])), err(GovError::EmptyProposalActions));
}

#[test]
fn defeated_proposals_cant_be_queued() {
    // Approval: 400 for vs 600 against.
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, p, VoteChoice::Against).unwrap();
    env.vote(&bob, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    assert_eq!(env.state(p), ProposalState::Defeated);
    assert_eq!(code(env.queue(p)), err(GovError::ApprovalThresholdNotMet));

    // Quorum: 10% of 1,000 is 100; a 50-token voter alone falls short.
    let (mut env, alice, _bob) = dao_with_members();
    let dave = env.member(1_000);
    env.deposit(&dave, 50).unwrap();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&dave.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&dave, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    assert_eq!(env.proposal_account(p).quorum_base, 1_050);
    assert_eq!(code(env.queue(p)), err(GovError::QuorumNotReached));
}

#[test]
fn abstain_counts_toward_quorum_only() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, p, VoteChoice::Abstain).unwrap();
    env.vote(&bob, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    // 100% of for+against is for; abstain helped reach quorum.
    env.queue(p).unwrap();
}

#[test]
fn queued_proposals_expire() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, p, VoteChoice::For).unwrap();
    env.warp(START + 110);
    env.queue(p).unwrap();
    // queued at 110: executable from 160 through 360.
    env.warp(START + 361);
    assert_eq!(env.state(p), ProposalState::Expired);
    assert_eq!(code(env.execute(p)), err(GovError::ProposalExpired));
}

#[test]
fn only_the_proposer_or_the_dao_can_cancel() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    assert_eq!(code(env.cancel(&bob.kp, p)), err(GovError::Unauthorized));
    env.cancel(&alice.kp, p).unwrap();
    assert_eq!(env.state(p), ProposalState::Cancelled);
    env.warp(START + 10);
    assert_eq!(code(env.vote(&bob, p, VoteChoice::For)), err(GovError::ProposalNotActive));
    assert_eq!(code(env.cancel(&alice.kp, p)), err(GovError::ProposalAlreadyCancelled));
}

#[test]
fn rules_change_only_through_a_passed_proposal() {
    let (mut env, alice, _bob) = dao_with_members();
    let new_config = VotingConfig { quorum_bps: 2_000, voting_period: 300, ..config() };

    // Nobody can call update_config directly: the treasury can't sign a transaction.
    let impostor = Keypair::new();
    env.svm.airdrop(&impostor.pubkey(), 1_000_000_000).unwrap();
    let direct = Instruction {
        program_id: tw::ID,
        accounts: tw::accounts::UpdateConfig { treasury: impostor.pubkey(), dao: env.dao }.to_account_metas(None),
        data: tw::instruction::UpdateConfig { config: new_config }.data(),
    };
    assert!(env.send(&[direct], &[&impostor]).is_err());

    // Bad rules are refused even from the DAO.
    let (_, bad) = env.propose(&alice, vec![env.update_config_ix(VotingConfig { quorum_bps: 0, ..config() })]).unwrap();
    let (_, good) = env.propose(&alice, vec![env.update_config_ix(new_config)]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, bad, VoteChoice::For).unwrap();
    env.vote(&alice, good, VoteChoice::For).unwrap();
    env.warp(START + 110);
    env.queue(bad).unwrap();
    env.queue(good).unwrap();
    env.warp(START + 160);
    assert_eq!(code(env.execute(bad)), err(GovError::InvalidQuorum));
    assert!(!env.proposal_account(bad).executed, "a failed execution changes nothing");
    env.execute(good).unwrap();
    assert_eq!(env.dao_account().config, new_config);
}

#[test]
fn a_proposal_cant_execute_itself_twice() {
    let (mut env, alice, _bob) = dao_with_members();
    // Proposal #1's only instruction is "execute proposal #1".
    let p1 = env.proposal_pda(1);
    let reenter = stored(Instruction {
        program_id: tw::ID,
        accounts: tw::accounts::Execute { dao: env.dao, proposal: p1 }.to_account_metas(None),
        data: tw::instruction::Execute {}.data(),
    });
    env.propose(&alice, vec![reenter]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, p1, VoteChoice::For).unwrap();
    env.warp(START + 110);
    env.queue(p1).unwrap();
    env.warp(START + 160);
    assert_eq!(code(env.execute(p1)), err(GovError::ProposalAlreadyExecuted), "the inner call sees it as executed already");
    assert!(!env.proposal_account(p1).executed);
}

#[test]
fn vote_records_can_be_closed_after_voting() {
    let (mut env, alice, bob) = dao_with_members();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 1)]).unwrap();
    env.warp(START + 10);
    env.vote(&bob, p, VoteChoice::For).unwrap();
    let record = env.vote_pda(&p, &bob.kp.pubkey());
    let close = |who: &Keypair| Instruction {
        program_id: tw::ID,
        accounts: tw::accounts::CloseVoteRecord { owner: who.pubkey(), proposal: p, vote_record: record }.to_account_metas(None),
        data: tw::instruction::CloseVoteRecord {}.data(),
    };
    let ix = close(&bob.kp);
    assert_eq!(code(env.send(&[ix], &[&bob.kp])), err(GovError::VotingNotEnded));
    env.warp(START + 110);
    let ix = close(&alice.kp);
    assert_eq!(code(env.send(&[ix], &[&alice.kp])), err(GovError::Unauthorized), "only the voter");
    let rent = env.svm.get_account(&record).unwrap().lamports;
    let before = env.svm.get_account(&bob.kp.pubkey()).unwrap().lamports;
    let ix = close(&bob.kp);
    env.send(&[ix], &[&bob.kp]).unwrap();
    assert!(env.svm.get_account(&record).is_none_or(|a| a.lamports == 0));
    assert_eq!(env.svm.get_account(&bob.kp.pubkey()).unwrap().lamports, before + rent - 5_000);
}

#[test]
fn deposits_must_use_the_daos_own_vault_and_mint() {
    let (mut env, alice, _bob) = dao_with_members();
    // A different mint and a vault the attacker controls.
    let other_mint = CreateMint::new(&mut env.svm, &env.payer).decimals(6).send().unwrap();
    let attacker = Keypair::new().pubkey();
    let fake_vault = CreateAssociatedTokenAccount::new(&mut env.svm, &env.payer, &other_mint).owner(&attacker).send().unwrap();
    let alice_other = CreateAssociatedTokenAccount::new(&mut env.svm, &env.payer, &other_mint).owner(&alice.kp.pubkey()).send().unwrap();
    MintTo::new(&mut env.svm, &env.payer, &other_mint, &alice_other, 1_000).send().unwrap();
    let ix = Instruction {
        program_id: tw::ID,
        accounts: tw::accounts::Deposit {
            owner: alice.kp.pubkey(),
            dao: env.dao,
            voter: env.voter_pda(&alice.kp.pubkey()),
            owner_token_account: alice_other,
            vault: fake_vault,
            mint: other_mint,
            token_program: litesvm_token::TOKEN_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: tw::instruction::Deposit { amount: 1_000 }.data(),
    };
    assert_eq!(code(env.send(&[ix], &[&alice.kp])), err(GovError::WrongDao));
    assert_eq!(env.voter_account(&alice.kp.pubkey()).amount, 600);
}

#[test]
fn works_with_token_2022_mints() {
    let token_2022: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();
    let mut env = Env::with_token_program(token_2022);
    assert_eq!(env.svm.get_account(&env.vault).unwrap().owner, token_2022);
    let alice = env.member(1_000);
    let bob = env.member(1_000);
    env.deposit(&alice, 700).unwrap();
    env.deposit(&bob, 300).unwrap();
    env.svm.airdrop(&env.treasury, 1_000_000_000).unwrap();
    let (_, p) = env.propose(&alice, vec![env.pay_sol(&bob.kp.pubkey(), 123)]).unwrap();
    env.warp(START + 10);
    env.vote(&alice, p, VoteChoice::For).unwrap();
    assert_eq!(code(env.withdraw(&alice, 1)), err(GovError::TokensLocked));
    env.warp(START + 110);
    env.queue(p).unwrap();
    env.warp(START + 160);
    env.execute(p).unwrap();
    env.withdraw(&alice, 700).unwrap();
    assert_eq!(env.token_balance(&alice.ata), 1_000);
    assert_eq!(env.token_balance(&env.vault), 300);
}
