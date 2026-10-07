# Vortexes

Solana governance programs for Protean DAO: the Solana counterparts of the EVM governance models in [Spaces](https://github.com/replico-labs/Spaces). Each model is its own program; the rules they share live in one library crate.

| Program | Model | Status |
|---|---|---|
| `vortex-token-weighted` | Deposit tokens, vote for / against / abstain, quorum + approval, timelock, execute | Built, tested (LiteSVM) |
| `vortex-quadratic` | Token-weighted with square-root vote weight | Planned |
| `vortex-optimistic` | Passes after a challenge window unless challenged | Planned |
| `vortex-board` | Signers confirm; executes at the required number | Planned |

Built with Anchor 1.2.1 on Agave 4.3. Nothing is deployed yet.

## How tokenWeighted works

- **A DAO** governs with one SPL token: classic SPL Token or Token-2022. `create_dao(name, config)` makes the DAO account and its vault. `create_key` is any fresh keypair; it only makes the DAO's address unique.
- **Voting power** is what a member has deposited into the DAO's vault (`deposit` / `withdraw`).
- **Proposals** carry the instructions they'll run (up to 8), plus a description. Proposing needs `proposal_threshold` deposited.
- **Voting** opens `voting_delay` seconds after proposing and lasts `voting_period`. Each member votes once (for, against or abstain) with their full deposit.
- **Passing** needs both:
  - quorum: for + against + abstain ≥ `quorum_bps` of all deposits at the time of proposing;
  - approval: for / (for + against) ≥ `approval_bps`. Abstain counts toward quorum only.
- **Queue and execute:** anyone can queue a passed proposal once voting ends, then execute it after `timelock` and before `execution_period` runs out. Its instructions run signed by the DAO's **treasury**, a PDA that holds the DAO's SOL and owns its token accounts. Executing marks the proposal done before any instruction runs, so an instruction can't execute it a second time.
- **Cancel:** the proposer, or the DAO itself through a proposal, can cancel anything not yet executed.
- **Changing the rules:** `update_config` needs the treasury's signature, which only an executing proposal can give.

### Differences from the EVM version

- **No vote snapshots.** Solana tokens have no balance history. Instead, a member who votes can't withdraw until every proposal they voted on has closed, so the same tokens can't vote twice from another wallet. Voting power is the deposit at the moment of voting.
- **Times are seconds,** not blocks.
- **The token isn't created by the program.** A DAO uses any existing mint. To let the DAO mint more through proposals, make the treasury PDA the mint authority.

## Accounts

| Account | Seeds | Holds |
|---|---|---|
| DAO | `["dao", create_key]` | name, mint, vault, rules, proposal count, total deposited |
| Vault | `["vault", dao]` | the deposited tokens (token account, authority = DAO) |
| Treasury | `["treasury", dao]` | the DAO's SOL; owns its token accounts; signs executed proposals |
| Voter | `["voter", dao, owner]` | a member's deposit and when it unlocks |
| Proposal | `["proposal", dao, id (u64 LE)]` | description, timing, tally, instructions |
| Vote record | `["vote", proposal, owner]` | one per voter per proposal; closable for its rent once voting ends |

## Layout

```
crates/vortex-core/       shared rules: config checks, proposal states, pass rule, running stored instructions
programs/token-weighted/  the tokenWeighted program
tests/                    end-to-end tests: the compiled programs in LiteSVM with the real token programs
idl/                      Anchor IDLs, for clients (the bot)
```

The IDLs are generated with `anchor idl build -p vortex_token_weighted -o idl/vortex_token_weighted.json`; regenerate after changing a program. Error codes come from `GovError` in `vortex-core`, so they aren't listed in the IDL: code 6000 is its first variant, 6001 the second, and so on.

## Building and testing

You need Rust, Agave 4.3 (`solana`, `cargo-build-sbf`) and optionally the Anchor CLI 1.2.1.

```bash
cargo-build-sbf                 # builds every program into target/deploy/
cargo test                      # unit tests + LiteSVM end-to-end tests (needs the build above)
```

The tests run the compiled `.so` files in [LiteSVM](https://github.com/LiteSVM/litesvm) with the real SPL Token, Token-2022 and Associated Token programs, so no validator is needed. They cover:
- the full lifecycle, paying out SOL and tokens from the treasury;
- one vote per member, and votes locking deposits;
- the proposal threshold, and the rule that only the treasury may sign a proposal's instructions;
- defeat by approval and by quorum, and abstain counting toward quorum only;
- expiry, and who may cancel;
- rule changes only through a passed proposal (invalid ones are refused);
- a proposal that tries to execute itself again;
- closing vote records for their rent;
- refusing another mint's vault;
- Token-2022 mints.

## Deploying

The program IDs in `declare_id!` and `Anchor.toml` are placeholders. Before the first deploy, generate your own program keypairs and point the code at them:

```bash
anchor keys sync                # writes target/deploy/*-keypair.json addresses into declare_id! and Anchor.toml
cargo-build-sbf
solana program deploy target/deploy/vortex_token_weighted.so --program-id target/deploy/vortex_token_weighted-keypair.json
```

Keep the program keypairs out of git; `.gitignore` already does.
