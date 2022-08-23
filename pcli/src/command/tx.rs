use std::fs::File;

use anyhow::{anyhow, Context, Result};
use penumbra_component::stake::rate::RateData;
use penumbra_crypto::{asset, DelegationToken, IdentityKey, Value, STAKING_TOKEN_ASSET_ID};
use penumbra_proto::view::NotesRequest;
use penumbra_transaction::action::Proposal;
use penumbra_view::SpendableNoteRecord;
use penumbra_view::ViewClient;
use penumbra_wallet::plan;
use rand_core::OsRng;

use crate::App;

#[derive(Debug, clap::Subcommand)]
pub enum TxCmd {
    /// Send funds to a Penumbra address.
    #[clap(display_order = 100)]
    Send {
        /// The destination address to send funds to.
        #[clap(long)]
        to: String,
        /// The amounts to send, written as typed values 1.87penumbra, 12cubes, etc.
        values: Vec<String>,
        /// The transaction fee (paid in upenumbra).
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
        /// Optional. Set the transaction's memo field to the provided text.
        #[clap(long)]
        memo: Option<String>,
    },
    /// Deposit stake into a validator's delegation pool.
    #[clap(display_order = 200)]
    Delegate {
        /// The identity key of the validator to delegate to.
        #[clap(long)]
        to: String,
        /// The amount of stake to delegate.
        amount: String,
        /// The transaction fee (paid in upenumbra).
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
    },
    /// Withdraw stake from a validator's delegation pool.
    #[clap(display_order = 200)]
    Undelegate {
        /// The amount of delegation tokens to undelegate.
        amount: String,
        /// The transaction fee (paid in upenumbra).
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
    },
    /// Redelegate stake from one validator's delegation pool to another.
    #[clap(display_order = 200)]
    Redelegate {
        /// The identity key of the validator to withdraw delegation from.
        #[clap(long)]
        from: String,
        /// The identity key of the validator to delegate to.
        #[clap(long)]
        to: String,
        /// The amount of stake to delegate.
        amount: String,
        /// The transaction fee (paid in upenumbra).
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
    },
    /// Swap tokens of one denomination for another using the DEX.
    ///
    /// Swaps are batched and executed at the market-clearing price.
    ///
    /// A swap generates two transactions: an initial "swap" transaction that
    /// submits the swap, and a "swap claim" transaction that privately mints
    /// the output funds once the batch has executed.  The second transaction
    /// will be created and submitted automatically.
    #[clap(display_order = 300)]
    Swap {
        /// The input amount to swap, written as a typed value 1.87penumbra, 12cubes, etc.
        input: String,
        /// The denomination to swap the input into.
        #[clap(long)]
        into: String,
        /// The transaction fee (paid in upenumbra).
        ///
        /// A swap generates two transactions; the fee will be split equally over both.
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
    },
    /// Propose a new governance vote, escrowing a proposal deposit until voting concludes.
    #[clap(display_order = 400)]
    Propose {
        /// The proposal to vote on, in JSON format.
        #[clap(long)]
        file: camino::Utf8PathBuf,
        /// The transaction fee (paid in upenumbra).
        #[clap(long, default_value = "0")]
        fee: u64,
        /// Optional. Only spend funds originally received by the given address index.
        #[clap(long)]
        source: Option<u64>,
    },
    /// Consolidate many small notes into a few larger notes.
    ///
    /// Since Penumbra transactions reveal their arity (how many spends,
    /// outputs, etc), but transactions are unlinkable from each other, it is
    /// slightly preferable to sweep small notes into larger ones in an isolated
    /// "sweep" transaction, rather than at the point that they should be spent.
    ///
    /// Currently, only zero-fee sweep transactions are implemented.
    #[clap(display_order = 990)]
    Sweep,
}

impl TxCmd {
    /// Determine if this command requires a network sync before it executes.
    pub fn needs_sync(&self) -> bool {
        match self {
            TxCmd::Send { .. } => true,
            TxCmd::Sweep { .. } => true,
            TxCmd::Swap { .. } => true,
            TxCmd::Delegate { .. } => true,
            TxCmd::Undelegate { .. } => true,
            TxCmd::Redelegate { .. } => true,
            TxCmd::Propose { .. } => true,
        }
    }

    pub async fn exec(&self, app: &mut App) -> Result<()> {
        match self {
            TxCmd::Send {
                values,
                to,
                fee,
                source: from,
                memo,
            } => {
                // Parse all of the values provided.
                let values = values
                    .iter()
                    .map(|v| v.parse())
                    .collect::<Result<Vec<Value>, _>>()?;
                let to = to
                    .parse()
                    .map_err(|_| anyhow::anyhow!("address is invalid"))?;

                let plan = plan::send(
                    &app.fvk,
                    &mut app.view,
                    OsRng,
                    &values,
                    *fee,
                    to,
                    *from,
                    memo.clone(),
                )
                .await?;
                app.build_and_submit_transaction(plan).await?;
            }
            TxCmd::Sweep => loop {
                let plans = plan::sweep(&app.fvk, &mut app.view, OsRng).await?;
                let num_plans = plans.len();

                for (i, plan) in plans.into_iter().enumerate() {
                    println!("building sweep {} of {}", i, num_plans);
                    let tx = app.build_transaction(plan).await?;
                    app.submit_transaction_unconfirmed(&tx).await?;
                }
                if num_plans == 0 {
                    println!("finished sweeping");
                    break;
                } else {
                    println!("awaiting confirmations...");
                    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                }
            },
            TxCmd::Swap {
                input,
                into,
                fee,
                source,
            } => {
                let input = input.parse::<Value>()?;
                let into = asset::REGISTRY.parse_unit(into.as_str()).base();

                let swap_plan =
                    plan::swap(&app.fvk, &mut app.view, OsRng, input, into, *fee, *source).await?;
                let swap_plan_inner = swap_plan
                    .swap_plans()
                    .next()
                    .expect("expected swap plan")
                    .clone();

                let swap_nft_asset_id = swap_plan_inner.swap_plaintext.asset_id();

                // Submit the `Swap` transaction.
                app.build_and_submit_transaction(swap_plan).await?;

                // Wait for detection of the note commitment containing the Swap NFT.
                let account_id = app.fvk.hash();
                let note_commitment = swap_plan_inner.swap_body(&app.fvk).swap_nft.note_commitment;
                // Find the swap NFT note associated with the swap plan.
                let swap_nft_note = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    app.view()
                        .await_note_by_commitment(account_id, note_commitment),
                )
                .await
                .context("timeout waiting to detect commitment of submitted transaction")?
                .context("error while waiting for detection of submitted transaction")?;

                // Now that the note commitment is detected, we can submit the `SwapClaim` transaction.

                let claim_plan = plan::swap_claim(
                    &app.fvk,
                    &mut app.view,
                    OsRng,
                    swap_nft_note.note.clone(),
                    *fee,
                    *source,
                )
                .await?;

                // Submit the `SwapClaim` transaction. TODO: should probably have `build_and_submit_transaction` wait for the output notes
                // of a SwapClaim to sync.
                app.build_and_submit_transaction(claim_plan).await?;
            }
            TxCmd::Delegate {
                to,
                amount,
                fee,
                source,
            } => {
                let unbonded_amount = {
                    let Value { amount, asset_id } = amount.parse::<Value>()?;
                    if asset_id != *STAKING_TOKEN_ASSET_ID {
                        return Err(anyhow!("staking can only be done with the staking token"));
                    }
                    amount
                };

                let to = to.parse::<IdentityKey>()?;

                let mut client = app.specific_client().await?;
                let rate_data: RateData = client
                    .next_validator_rate(tonic::Request::new(to.into()))
                    .await?
                    .into_inner()
                    .try_into()?;

                let plan = plan::delegate(
                    &app.fvk,
                    &mut app.view,
                    OsRng,
                    rate_data,
                    unbonded_amount,
                    *fee,
                    *source,
                )
                .await?;

                app.build_and_submit_transaction(plan).await?;
            }
            TxCmd::Undelegate {
                amount,
                fee,
                source,
            } => {
                let (self_address, _dtk) = app
                    .fvk
                    .incoming()
                    .payment_address(source.unwrap_or(0).into());

                let delegation_value @ Value {
                    amount: _,
                    asset_id,
                } = amount.parse::<Value>()?;

                let delegation_token: DelegationToken = app
                    .view()
                    .assets()
                    .await?
                    .get(&asset_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown asset id {}", asset_id))?
                    .clone()
                    .try_into()
                    .context("could not parse supplied denomination as a delegation token")?;

                let from = delegation_token.validator();

                let mut client = app.specific_client().await?;
                let rate_data: RateData = client
                    .next_validator_rate(tonic::Request::new(from.into()))
                    .await?
                    .into_inner()
                    .try_into()?;

                // first, split the input notes into exact change
                let split_plan = plan::send(
                    &app.fvk,
                    &mut app.view,
                    OsRng,
                    &[delegation_value],
                    *fee,
                    self_address,
                    *source,
                    None,
                )
                .await?;

                // find the note commitment corresponding to the delegation value within the split
                // plan, so that we can use it to create the undelegate plan
                let delegation_note_commitment = split_plan
                    .output_plans()
                    .find_map(|output| {
                        let note = output.output_note();
                        // grab the note commitment of whichever output in the spend plan has
                        // exactly the right amount and asset id, and is also addressed to us
                        if note.value() == delegation_value
                        // this check is not necessary currently, because we never construct
                        // undelegations to a different address than ourselves, but it's good to
                        // leave it in here so that if we ever change that invariant, it will fail
                        // here rather than after already executing the plan
                            && app.fvk.incoming().views_address(&output.dest_address)
                        {
                            Some(note.commit())
                        } else {
                            None
                        }
                    })
                    .expect("there must be an exact output for the amount we are expecting");

                // we submit the split transaction before building the undelegate plan, because we
                // need to await the note created by its output
                app.build_and_submit_transaction(split_plan).await?;

                // await the receipt of the exact note we wish to undelegate (this should complete
                // immediately, because the spend in the split plan is awaited when we submit the
                // transaction)
                let delegation_notes = vec![
                    app.view
                        .await_note_by_commitment(app.fvk.hash(), delegation_note_commitment)
                        .await?,
                ];

                // now we can plan and submit an exact-change undelegation
                let undelegate_plan = plan::undelegate(
                    &app.fvk,
                    &mut app.view,
                    OsRng,
                    rate_data,
                    delegation_notes,
                    *fee,
                    *source,
                )
                .await?;

                // Pass None as the change to await, since the change will be quarantined, so we won't detect it.
                // But it's not spendable anyways, so we don't need to detect it.
                let tx = app.build_transaction(undelegate_plan).await?;
                app.submit_transaction(&tx, None).await?;
            }
            TxCmd::Redelegate { .. } => {
                println!("Sorry, this command is not yet implemented");
            }
            TxCmd::Propose { file, fee, source } => {
                let proposal: Proposal = serde_json::from_reader(File::open(&file)?)?;
                let plan =
                    plan::propose(&app.fvk, &mut app.view, OsRng, proposal, *fee, *source).await?;
                app.build_and_submit_transaction(plan).await?;
            }
        }
        Ok(())
    }
}
