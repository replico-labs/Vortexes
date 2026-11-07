//! The security council on the hub: passed proposals wait for it, it
//! survives switches, it can be replaced after its tenure without its
//! confirmation, and turning it off needs it.

use vortex_tests::*;

const TENURE: i64 = 1_000;

struct Council {
    env: Env,
    alice: Member,
    bob: Member,
    signers: Vec<Keypair>,
}

fn ix_set_council(env: &Env, signers: &[Pubkey], required: u8, tenure: i64) -> StoredInstruction {
    stored(Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::TreasuryOnly { treasury: env.treasury, dao: env.dao }.to_account_metas(None),
        data: hub::instruction::SetCouncil { signers: signers.to_vec(), required, tenure }.data(),
    })
}

fn ix_clear_council(env: &Env) -> StoredInstruction {
    stored(Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::TreasuryOnly { treasury: env.treasury, dao: env.dao }.to_account_metas(None),
        data: hub::instruction::ClearCouncil {}.data(),
    })
}

/// A token-weighted DAO that turned on a 2-of-3 council.
fn with_council() -> Council {
    let (mut env, alice, bob) = dao_with_members();
    let signers: Vec<Keypair> = (0..3).map(|_| Keypair::new()).collect();
    for s in &signers {
        env.svm.airdrop(&s.pubkey(), 1_000_000_000).unwrap();
    }
    let keys: Vec<Pubkey> = signers.iter().map(|s| s.pubkey()).collect();
    let on = env.pass(TW, &alice, vec![ix_set_council(&env, &keys, 2, TENURE)]);
    env.execute(TW, on).unwrap();
    let c = env.hub_dao().council.unwrap();
    assert_eq!((c.signers, c.required, c.tenure, c.nonce), (keys, 2, TENURE, 1));
    Council { env, alice, bob, signers }
}

impl Council {
    fn waiting(&self, proposal: Pubkey) -> Pubkey {
        hub::waiting_address(&self.env.dao, &proposal)
    }

    fn record(&self, proposal: Pubkey) -> hub::Waiting {
        hub::Waiting::try_deserialize(&mut self.env.svm.get_account(&self.waiting(proposal)).unwrap().data.as_slice()).unwrap()
    }

    fn submit(&mut self, gp: Pubkey, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let env = &self.env;
        let ix = Instruction {
            program_id: hub::ID,
            accounts: hub::accounts::SubmitToCouncil {
                payer: env.admin.pubkey(),
                dao: env.dao,
                executor: hub::executor_address(&env.dao),
                governance_program: gp,
                governance: env.governance(gp),
                proposal,
                waiting: self.waiting(proposal),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: hub::instruction::SubmitToCouncil {}.data(),
        };
        let admin = self.env.admin.insecure_clone();
        self.env.send(&[ix], &[&admin])
    }

    fn vote(&mut self, signer: &Keypair, proposal: Pubkey, confirm: bool) -> Result<(), FailedTransactionMetadata> {
        let accounts = hub::accounts::CouncilVote { signer: signer.pubkey(), dao: self.env.dao, waiting: self.waiting(proposal) }.to_account_metas(None);
        let data = if confirm { hub::instruction::CouncilConfirm {}.data() } else { hub::instruction::CouncilReject {}.data() };
        self.env.send(&[Instruction { program_id: hub::ID, accounts, data }], &[signer])
    }

    fn run(&mut self, proposal: Pubkey) -> Result<(), FailedTransactionMetadata> {
        let p = self.env.core(proposal);
        let mut accounts = hub::accounts::RunConfirmed { dao: self.env.dao, waiting: self.waiting(proposal), proposal }.to_account_metas(None);
        for ix in &p.instructions {
            for m in &ix.accounts {
                accounts.push(AccountMeta { pubkey: m.pubkey, is_signer: false, is_writable: m.is_writable });
            }
            accounts.push(AccountMeta::new_readonly(ix.program_id, false));
        }
        let ix = Instruction { program_id: hub::ID, accounts, data: hub::instruction::RunConfirmed {}.data() };
        let admin = self.env.admin.insecure_clone();
        self.env.send(&[ix], &[&admin])
    }

    /// Passes `instructions` under token-weighted and submits it.
    fn pass_and_submit(&mut self, instructions: Vec<StoredInstruction>) -> Pubkey {
        let p = self.env.pass(TW, &self.alice, instructions);
        self.submit(TW, p).unwrap();
        p
    }

    fn signer(&self, i: usize) -> Keypair {
        self.signers[i].insecure_clone()
    }
}

#[test]
fn passed_proposals_wait_for_the_council() {
    let mut c = with_council();
    let host = Keypair::new().pubkey();
    let p = c.env.pass(TW, &c.alice, vec![c.env.pay_sol(&host, 1_000_000)]);
    assert_eq!(code(c.env.execute(TW, p)), err(GovError::AwaitingCouncil));
    c.submit(TW, p).unwrap();
    assert!(c.submit(TW, p).is_err(), "submitted once");
    assert_eq!(code(c.run(p)), err(GovError::AwaitingCouncil));

    let outsider = Keypair::new();
    c.env.svm.airdrop(&outsider.pubkey(), 1_000_000_000).unwrap();
    assert_eq!(code(c.vote(&outsider, p, true)), err(GovError::NotCouncilSigner));
    let (s0, s1) = (c.signer(0), c.signer(1));
    c.vote(&s0, p, true).unwrap();
    assert_eq!(code(c.vote(&s0, p, true)), err(GovError::AlreadyConfirmed));
    assert_eq!(code(c.run(p)), err(GovError::AwaitingCouncil), "1 of 2");
    c.vote(&s1, p, true).unwrap();
    c.run(p).unwrap();
    assert_eq!(c.env.lamports(&host), 1_000_000);
    assert_eq!(c.record(p).status, hub::WaitingStatus::Done);
    assert_eq!(code(c.run(p)), err(GovError::ProposalAlreadyExecuted));
}

#[test]
fn a_rejected_proposal_never_runs() {
    let mut c = with_council();
    let host = Keypair::new().pubkey();
    let p = c.pass_and_submit(vec![c.env.pay_sol(&host, 1_000_000)]);
    let (s0, s1, s2) = (c.signer(0), c.signer(1), c.signer(2));
    c.vote(&s0, p, true).unwrap();
    c.vote(&s1, p, false).unwrap();
    assert_eq!(c.record(p).status, hub::WaitingStatus::Waiting, "2 of 3 can still confirm");
    c.vote(&s2, p, false).unwrap();
    assert_eq!(c.record(p).status, hub::WaitingStatus::Rejected);
    assert_eq!(code(c.vote(&s0, p, true)), err(GovError::ProposalNotExecutable));
    assert_eq!(code(c.run(p)), err(GovError::CouncilRejected));
    assert_eq!(c.env.lamports(&host), 0);
}

#[test]
fn the_council_survives_a_switch() {
    let mut c = with_council();
    let init = stored(c.env.ix_init_governance(QV, &c.env.treasury, &c.env.treasury, config()));
    let s = c.pass_and_submit(vec![stored(c.env.ix_propose_switch(QV)), init]);
    let (s0, s2) = (c.signer(0), c.signer(2));
    c.vote(&s0, s, true).unwrap();
    c.vote(&s2, s, true).unwrap();
    c.run(s).unwrap();
    c.env.warp(c.env.hub_dao().pending_switch.unwrap().ready_at);
    c.env.apply_switch(QV).unwrap();
    let d = c.env.hub_dao();
    assert_eq!((d.governance_program, d.epoch), (QV, 2));
    assert_eq!(d.council.unwrap().nonce, 1, "same council");

    // Quadratic's proposals wait for it too.
    let host = Keypair::new().pubkey();
    let p = c.env.pass(QV, &c.bob, vec![c.env.pay_sol(&host, 1_000_000)]);
    assert_eq!(code(c.env.execute(QV, p)), err(GovError::AwaitingCouncil));
    c.submit(QV, p).unwrap();
    c.vote(&s0, p, true).unwrap();
    c.vote(&s2, p, true).unwrap();
    c.run(p).unwrap();
    assert_eq!(c.env.lamports(&host), 1_000_000);
}

#[test]
fn a_waiting_proposal_dies_with_its_epoch() {
    let mut c = with_council();
    let host = Keypair::new().pubkey();
    let pay = c.pass_and_submit(vec![c.env.pay_sol(&host, 1_000_000)]);
    c.env.pin(TW, 2);
    let init = stored(c.env.ix_init_governance(TW, &c.env.treasury, &c.env.treasury, config()));
    c.env.pin(TW, 1);
    let s = c.pass_and_submit(vec![stored(c.env.ix_propose_switch(TW)), init]);
    let (s0, s1) = (c.signer(0), c.signer(1));
    for p in [pay, s] {
        c.vote(&s0, p, true).unwrap();
        c.vote(&s1, p, true).unwrap();
    }
    c.run(s).unwrap();
    c.env.warp(c.env.hub_dao().pending_switch.unwrap().ready_at);
    c.env.pin(TW, 2);
    c.env.apply_switch(TW).unwrap();
    assert_eq!(code(c.run(pay)), err(GovError::StaleProposal));
}

#[test]
fn the_dao_replaces_a_council_that_wont_step_down_once_its_tenure_ends() {
    let mut c = with_council();
    let (s0, s1, s2) = (c.signer(0), c.signer(1), c.signer(2));

    // Turning it off needs the council, and it refuses.
    let off = c.pass_and_submit(vec![ix_clear_council(&c.env)]);
    c.vote(&s0, off, false).unwrap();
    c.vote(&s1, off, false).unwrap();
    assert_eq!(code(c.run(off)), err(GovError::CouncilRejected));

    // A proposal that does anything besides replacing the signers can't skip it.
    let new_signer = Keypair::new();
    c.env.svm.airdrop(&new_signer.pubkey(), 1_000_000_000).unwrap();
    let host = Keypair::new().pubkey();
    let sneaky = c.pass_and_submit(vec![ix_set_council(&c.env, &[new_signer.pubkey()], 1, 0), c.env.pay_sol(&host, 1_000_000)]);

    // A pure replacement waits for the tenure, then runs unconfirmed.
    let replace = c.pass_and_submit(vec![ix_set_council(&c.env, &[new_signer.pubkey()], 1, 0)]);
    c.vote(&s0, replace, false).unwrap();
    c.vote(&s1, replace, false).unwrap();
    let seated = c.env.hub_dao().council.unwrap().seated_at;
    assert!(c.env.now() < seated + TENURE);
    assert_eq!(code(c.run(replace)), err(GovError::TenureNotOver));
    c.env.warp(seated + TENURE);
    assert_eq!(code(c.run(sneaky)), err(GovError::AwaitingCouncil));
    c.run(replace).unwrap();
    let council = c.env.hub_dao().council.unwrap();
    assert_eq!((council.signers, council.required, council.nonce), (vec![new_signer.pubkey()], 1, 2));

    // The old signers' votes don't count any more; the new one decides.
    assert_eq!(code(c.vote(&s2, sneaky, true)), err(GovError::NotCouncilSigner));
    let off = c.pass_and_submit(vec![ix_clear_council(&c.env)]);
    c.vote(&new_signer, off, true).unwrap();
    c.run(off).unwrap();
    assert!(c.env.hub_dao().council.is_none());

    // Off: proposals run straight away again; one left waiting can run.
    let p = c.env.pass(TW, &c.alice, vec![c.env.pay_sol(&host, 2_000_000)]);
    c.env.execute(TW, p).unwrap();
    c.run(sneaky).unwrap();
    assert_eq!(c.env.lamports(&host), 3_000_000);
}

#[test]
fn only_a_passed_proposal_sets_the_council() {
    let (mut env, _alice, _bob) = dao_with_members();
    let creator = env.creator.insecure_clone();
    let ix = Instruction {
        program_id: hub::ID,
        accounts: hub::accounts::TreasuryOnly { treasury: env.treasury, dao: env.dao }.to_account_metas(None),
        data: hub::instruction::SetCouncil { signers: vec![creator.pubkey()], required: 1, tenure: 0 }.data(),
    };
    // The treasury is a PDA: nobody can sign for it outside the hub.
    let mut ix = ix;
    ix.accounts[0].is_signer = false;
    assert!(env.send(&[ix], &[&creator]).is_err());
    assert!(env.hub_dao().council.is_none());
}
