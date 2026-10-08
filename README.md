# DAO Vortexes

The SVM implementation of [DAO Spaces](https://github.com/replico-labs/Spaces): on-chain DAO governance on Solana where a DAO's treasury and its governance rules are separate, so a DAO can change how it decides by vote, without its treasury moving.

Built with Anchor 1.2.1 on Agave 4.3. Not deployed yet. The companion bot lives in [`protean-bot`](https://github.com/replico-labs/protean-bot).

## Why it's built differently from Spaces

On the EVM, every DAO gets its own `Treasury` contract, and switching models is one call: `transferGovernance(newGovernance)`. On Solana, a treasury held by a program address (PDA) can only ever be signed for by the program it belongs to, so it can't be handed from one governance program to another. Vortexes keeps that shared state in one place instead:

- **`vortex-hub`**, one program serving every DAO. It holds each DAO's **treasury** and its **record**: which governance program the DAO currently listens to.
- **Governance programs** only **decide**: proposals, votes, signers, and the token deposits (staking) voting power comes from. They hold no DAO funds.

## Architecture

```
                   execute(proposal)
   anyone ───────────────────────────────▶ ┌──────────────────────┐
                                           │      vortex-hub      │
   ┌────────────────────────┐  confirm?    │  DAO record:         │
   │ the DAO's current      │ ◀─────────── │  · governance program│
   │ governance program     │              │  · epoch             │
   │ (any approved model)   │ ───────────▶ │                      │
   └────────────────────────┘  receipt     │  treasury (PDA) ─────┼──▶ runs the
                                           └──────────────────────┘    proposal's
                                                                       instructions
```

**Running a proposal.** Anyone calls the hub's `execute`. The hub asks the DAO's current governance program to confirm the proposal (`confirm_execution`, signed by a hub PDA so nobody else can ask). The program checks that the proposal passed and can run now, marks it executed, and answers with the proposal's address. Only then does the hub run the proposal's instructions, signed by the treasury. If any instruction fails, nothing changes.

**Switching models.** A passed proposal calls the hub's `propose_switch(new_program)` and sets the DAO up in the new program. After a 2-day delay (cancellable by another proposal), anyone calls `apply_switch`: the hub now listens only to the new program, and the DAO's **epoch** goes up. Proposals only run in the epoch they were made in, so nothing from an earlier setup can ever run again, even if the DAO switches back.

## Governance models

| Model | How it decides |
|---|---|
| **Token-weighted** | Deposit the DAO's token; vote for, against or abstain; quorum and approval threshold |
| **Quadratic** | Same, but a deposit of n tokens gives √n votes |
| **Optimistic** | Passes after a challenge window unless someone posts a bond to challenge it; challenged proposals go to a token vote |
| **Board (Multisig)** | M-of-N signers confirm; no token at all |
| **Conviction** | Members back one proposal at a time; support builds conviction over time, with per-asset spending budgets |
| **Delegate** | Members elect a council for a term; the council proposes and votes; members can recall council members |

Sortition, Liquid, Sowellian and Decision Markets are not ported yet.

## Programs

| Program | Responsibility |
|---|---|
| `programs/hub` | DAO records, treasuries, the approved-models list, execution, switching |
| `programs/token-weighted` | Token-weighted voting. Its voting logic (`token_voting.rs`) is shared with quadratic |
| `programs/quadratic` | Quadratic voting: token-weighted with a √ vote weight |
| `programs/optimistic` | Challenge window, bonds, challenge votes |
| `programs/board` | Signers, confirmations |
| `programs/conviction` | Conviction, the asset list and spending budgets |
| `programs/delegate` | Elections, council votes, recalls |
| `crates/vortex-core` | Shared library: seeds, the proposal prefix every model uses, vote math, errors (not deployed) |

Each program's header comment explains its rules in full.

## Key design decisions

- **The hub holds the money; governance programs only decide.** A DAO's treasury address, funds and token accounts never move when it switches models.
- **Decisions can't be forged.** The hub only accepts a confirmation from the program the DAO points to, returned through Solana's CPI return data, for a proposal in the current epoch.
- **One shared proposal format.** Every model's proposal account starts with the same `ProposalCore` (DAO, epoch, instructions), so the hub can run proposals from any model.
- **Adding a model doesn't touch the hub.** A new program implements `confirm_execution` and `ProposalCore`, and the hub admin approves it. The admin has no power over any DAO's treasury or rules.
- **Deposits instead of snapshots.** Solana tokens have no balance history, so voting power is what's deposited, and deposits stay locked until the votes they were used in close.
- **Times are in seconds,** not blocks.
- **Fixes over the EVM contracts:**
  - Board: removing a signer also removes their pending confirmations.
  - Delegate: a recalled member's votes stop counting, and a new council ends the old one's proposals.
  - Optimistic: a challenger gets their bond back if the proposal is cancelled.

## Conviction spending budgets

As in Spaces' version 2: the DAO keeps a list of up to 10 assets it protects (SOL and its own token from the start), each with a weight. Every proposal declares a budget per asset, and its bar rises by `weight × amount ÷ the treasury's holding`. Proposals that loosen the rules, switch model or hand over an authority need the bar of spending everything.

The program wraps every proposal's instructions between `record_balances` and `check_budget`, so the check runs inside the hub's execution. Overspending, or closing a watched token account or giving it a new owner or delegate, reverts the whole execution. Weight cuts and removals wait 7 days. Only listed assets are watched.

## Building and testing

You need Rust (pinned to 1.97.1), Agave 4.3 and the Anchor CLI 1.2.1.

```bash
anchor build      # programs into target/deploy/, IDLs into target/idl/
cargo test        # unit tests + end-to-end tests (needs the build above)
```

Use `anchor build`, not a bare `cargo-build-sbf` at the root: building everything in one go builds the hub without its entry point.

The end-to-end tests run the compiled programs in [LiteSVM](https://github.com/LiteSVM/litesvm) with the real SPL Token, Token-2022 and Associated Token programs. Current result: **52 end-to-end tests and 10 unit tests passed, 0 failed**. They cover every model's lifecycle, the hub's checks, switching between models with one treasury, and budgets.

`idl/` holds the IDLs for clients such as the bot; copy them from `target/idl/` after changing a program. Error codes come from `GovError` in `vortex-core` and are the same in every program.

## Deployment

The program IDs are placeholders. Before the first deploy:

```bash
anchor keys sync      # your own program keypairs' addresses into the code
anchor build
anchor deploy --provider.cluster devnet
```

Then, once: call `init_hub` (the caller becomes the hub admin), and approve each model with `set_model`. Keep the program keypairs out of git (`.gitignore` already does).

## Known gaps

- **No audit.** The hub holds every DAO's treasury. Get it audited before mainnet, and move the hub admin and every program's upgrade authority to a multisig.
- **Never run on a live cluster.** Everything has passed in LiteSVM only; a devnet smoke test is next.
- **Not in the bot yet.** The bot runs Spaces on EVM chains only.
- **Budgets only see listed assets,** and only in the treasury's associated token accounts.

## License

MIT. See [LICENSE](LICENSE).
