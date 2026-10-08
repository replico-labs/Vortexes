# Vortexes

Solana governance programs for Protean DAO: the Solana counterparts of the EVM governance models in [Spaces](https://github.com/replico-labs/Spaces), built so a DAO can change its governance model by vote without its treasury moving.

Built with Anchor 1.2.1 on Agave 4.3. Nothing is deployed yet.

## The design: one hub, many governance models

A DAO is split in two:

- **`vortex-hub`** keeps the DAO's **record** (name, creator, which governance program runs it) and its **treasury** (a PDA that holds the DAO's SOL and owns its token accounts). It runs passed proposals and handles switching models. One hub serves every DAO.
- **Governance programs** only **decide**: proposals, votes, signers. They hold no funds. Any program on the hub's approved list can run a DAO.

| Program | What it is | Status |
|---|---|---|
| `vortex-hub` | DAO records, treasuries, approved models, execution, switching | Built, tested |
| `vortex-token-weighted` | Deposit tokens, vote for / against / abstain, quorum + approval, timelock | Built, tested |
| `vortex-quadratic` | Same, but a deposit of n tokens gives √n votes | Built, tested |
| `vortex-optimistic` | Passes after a challenge window unless challenged; a challenge starts a token vote | Built, tested |
| `vortex-board` | Signers confirm; passes at the required number | Built, tested |
| `vortex-core` | Shared library compiled into the programs above (not deployed) | — |

The split mirrors the EVM version, where `Treasury.sol` is its own contract and `transferGovernance` hands it to a new governance contract. The hub's confirmation comes back through the CPI return data of the governance program it called, so a decision can't be forged.

### Running a passed proposal

1. Members propose and vote in the DAO's governance program.
2. Anyone calls `vortex_hub::execute`.
3. The hub calls the governance program's `confirm_execution`, signed by the hub's **executor PDA** for that DAO (so nobody else can call it). The governance program checks the proposal passed and can run now, marks it executed, and answers with the proposal's address through the CPI return data.
4. The hub reads the proposal's instructions from its standard prefix (`ProposalCore`: DAO, epoch, instructions) and runs them, signed by the DAO's treasury. If any instruction fails, nothing changes.

A proposal's instructions can do anything the treasury could: pay SOL or tokens, mint (if the treasury is a mint authority), call other programs, change the DAO's rules (`update_config` on its governance program), or start a switch on the hub.

### Switching governance model

1. A passed proposal calls `vortex_hub::propose_switch(new_program)` and, in the same proposal, sets the DAO up in the new program (its `init_governance`, paid by the treasury). Only approved models are accepted.
2. The switch waits **2 days** (`SWITCH_DELAY`). During that time the current model still runs the DAO and can call `cancel_switch` through another proposal.
3. After the delay anyone calls `apply_switch`. The hub now takes orders from the new program, and the DAO's **epoch** goes up.

The treasury address, its funds, token accounts and mint authorities don't move. Proposals only run in the epoch they were made in, so anything left over from an earlier setup stays dead, even if the DAO later switches back. Members withdraw their deposits from the old model whenever they like.

### Who controls what

- **Hub admin:** keeps the approved-models list (`set_model`) and can hand the role on (`set_admin`). The admin has no power over any DAO's treasury or rules.
- **DAO creator:** creates the DAO and sets up its first governance. That's all; every later change goes through the DAO's own proposals.
- **Treasury signature:** only the hub gives it, only while running a passed proposal. Rule changes, cancelling by the DAO, switching and switch cancellation all require it (the `onlyGovernance` of the EVM contracts).

## Token-weighted and quadratic

The two share one source file, `programs/token-weighted/src/token_voting.rs`. Each program only sets its ID and its `vote_weight` (n votes, or √n), so they can't drift apart.

- **Voting power** is what a member has deposited into the DAO's vault: classic SPL Token or Token-2022.
- **Proposing** needs `proposal_threshold` deposited (raw tokens, in both models). A proposal carries up to 8 instructions, and only the DAO's treasury may be asked to sign them.
- **Voting** opens `voting_delay` seconds after proposing and lasts `voting_period`. Each member votes once (for, against or abstain), with `vote_weight(deposit)`.
- **Passing** needs both:
  - quorum: for + against + abstain ≥ `quorum_bps` of `vote_weight(all deposits when proposed)`;
  - approval: for / (for + against) ≥ `approval_bps`.

  Then anyone queues it, and it can run through the hub after `timelock`, until `execution_period` runs out.
- **Cancel:** the proposer, or the DAO through a proposal.
- **No snapshots:** Solana tokens have no balance history, so a member who votes can't withdraw until every proposal they voted on has closed. The same tokens can't vote twice from another wallet.
- **Times are seconds,** not blocks.

## Optimistic

Proposals pass by default; only disputed ones go to a vote.

- **Deposits** work as in token-weighted: members deposit the DAO's token into its vault, and the deposit is their voting power if a vote is needed. Proposing needs `proposal_threshold` deposited.
- **Challenge window:** for `challenge_period` seconds after proposing, anyone can challenge by posting `challenge_bond` tokens from their wallet into the bond vault. One challenge per proposal.
- **Unchallenged:** once the window closes, anyone calls `finalize_unchallenged` and the proposal is queued.
- **Challenged:** a token vote runs for `voting_period` (for, against or abstain, weighted by deposit; voters' deposits lock until it closes). It passes with the same quorum and approval rules as token-weighted. Then anyone calls `finalize_challenge`:
  - passed: the proposal is queued and the bond goes to the DAO's treasury token account;
  - failed: the bond goes back to the challenger's token account.

  Only the account being paid is passed in, and its owner is checked.
- **Running:** queued proposals run through the hub after `timelock`, within `execution_period`.
- **Cancel:** the proposer, or the DAO through a proposal. If a challenged proposal is cancelled before it's settled, anyone can call `reclaim_bond` to return the bond to the challenger. (In the EVM contract that bond stays stuck.)
- **Rules** (`update_config`) change only through a passed proposal; a zero challenge window is refused.

## Board

A multisig. There's no token; power is being a signer (up to 20).

- **Proposing:** only signers propose, and proposing counts as the proposer's confirmation.
- **Confirming:** other signers `confirm`. Once `required_approvals` have confirmed, the proposal is queued. A signer can `revoke_confirmation`; if that drops it below the threshold, it's unqueued.
- **Running:** after `timelock`, within `execution_period`, through the hub. Only confirmations from people who are *still* signers count at that moment, so removing a signer also removes their pending confirmations. (In the EVM contract they keep counting.)
- **Cancel:** the proposer, or the DAO through a proposal.
- **Signers and rules** (`add_signer`, `remove_signer`, `update_config`) change only through a passed proposal, never directly, not even by a signer. `required_approvals` must stay between 1 and the number of signers, and removing a signer that would break that is refused.

## Accounts

"voting" means token-weighted, quadratic and optimistic, which share these layouts.

| Program | Account | Seeds | Holds |
|---|---|---|---|
| hub | Hub | `["hub"]` | the admin |
| hub | Model | `["model", program_id]` | an approved governance program |
| hub | DAO | `["dao", create_key]` | name, creator, governance program, epoch, pending switch |
| hub | Treasury | `["treasury", dao]` | the DAO's SOL; owns its token accounts |
| hub | Executor | `["executor", dao]` | signs `confirm_execution` calls (no data) |
| voting | Governance | `["governance", dao]` | rules, mint, vault, proposal count, total deposited |
| voting | Vault | `["vault", governance]` | deposited tokens (authority = governance) |
| voting | Voter | `["voter", governance, owner]` | a member's deposit and when it unlocks |
| voting | Proposal | `["proposal", governance, id]` | `ProposalCore`, timing, tally |
| voting | Vote record | `["vote", proposal, owner]` | one per voter per proposal; closable for its rent after voting |
| optimistic | Bond vault | `["bond_vault", governance]` | challenge bonds until settled (authority = governance) |
| board | Governance | `["governance", dao]` | signers, rules, proposal count |
| board | Proposal | `["proposal", governance, id]` | `ProposalCore`, confirmations, timing |

## Adding a governance model

A new model is a new program that:
1. keeps the DAO's state at `["governance", hub_dao]`, created by the DAO's creator (first setup) or its treasury (switch);
2. starts every proposal account with `ProposalCore`;
3. implements `confirm_execution(epoch)` with accounts `[executor (signer), governance, proposal (writable)]`. It must accept only the DAO's executor PDA, mark the proposal executed, and `set_return_data` its address;
4. accepts only the DAO's treasury as a signer in proposals, and for its own admin actions.

Then the hub admin approves it with `set_model`, and DAOs can create with it or switch to it. The hub doesn't change.

## Building and testing

You need Rust (the repo pins 1.97.1), Agave 4.3 and the Anchor CLI 1.2.1.

```bash
anchor build      # builds each program into target/deploy/ and writes target/idl/
cargo test        # unit tests + LiteSVM end-to-end tests (needs the build above)
```

Use `anchor build`, not a bare `cargo-build-sbf` at the root: the voting programs use the hub as a library, and building everything in one go would build the hub without its entry point.

The tests run the compiled programs in [LiteSVM](https://github.com/LiteSVM/litesvm) with the real SPL Token, Token-2022 and Associated Token programs. They cover:
- **Voting through the hub:** the full lifecycle paying SOL and tokens from the treasury; one vote each and locked deposits; threshold and signer checks; defeat by approval and by quorum; abstain; expiry; cancelling; rule changes only by proposal; a proposal re-executing itself; closing vote records; another mint's vault refused; Token-2022.
- **The hub:** only the hub can confirm executions; it only trusts the DAO's own governance program; only approved models; only the admin approves; only the creator does the first setup.
- **Switching:** token-weighted → quadratic with the same treasury (√ weights checked); one DAO through token-weighted → board → optimistic with one treasury; the 2-day delay; cancelling a pending switch; disabled or same-model switches refused; the new model must be set up first; switching back leaves old proposals dead.
- **Optimistic:** unchallenged proposals pass after the window; a failed challenge pays the bond to the treasury; a successful one returns it to the challenger and nobody else; cancelled proposals' bonds can be reclaimed; the proposal threshold; rule changes only by proposal.
- **Board:** 2-of-3 end to end; outsiders can't propose or confirm; revoking below the threshold unqueues; signers and rules change only by proposal; a removed signer's confirmation stops counting; who can cancel; bad boards refused at setup.

`idl/` holds the IDLs for clients (the bot); copy them from `target/idl/` after changing a program. Error codes come from `GovError` in `vortex-core` and are the same in every program: 6000 is its first variant, 6001 the second, and so on.

## Deploying

The program IDs in `declare_id!` and `Anchor.toml` are placeholders. Before the first deploy, generate your own program keypairs and point the code at them:

```bash
anchor keys sync      # writes target/deploy/*-keypair.json addresses into declare_id! and Anchor.toml
anchor build
anchor deploy --provider.cluster devnet
```

Then, once:
1. Call `init_hub`; the caller becomes the hub admin.
2. Approve the models with `set_model`.

Before mainnet:
- Move the hub admin and every program's upgrade authority to a multisig.
- Get the hub audited: it holds every DAO's treasury.

Keep program keypairs out of git; `.gitignore` already does.

## License

MIT. See [LICENSE](LICENSE).
