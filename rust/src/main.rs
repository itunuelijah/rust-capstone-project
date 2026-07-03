#![allow(unused)]
use bitcoincore_rpc::bitcoin::{Address, Amount, BlockHash, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;

// Node access params (regtest defaults).
const RPC_URL: &str = "http://127.0.0.1:18443";
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

/// Bundles every field that ends up in out.txt so we pass one value
/// around instead of ten loose locals.
struct TransactionSummary {
    txid: Txid,
    miner_input_address: Address,
    miner_input_amount: f64,
    trader_address: Address,
    trader_amount: f64,
    change_address: Address,
    change_amount: f64,
    fee: f64,
    block_height: u32,
    block_hash: BlockHash,
}

/// Creates a wallet if it doesn't exist yet; otherwise loads the
/// existing one. Bitcoin Core errors if you try to create a wallet
/// that's already on disk, so we treat that as the "load instead" case.
fn ensure_wallet(rpc: &Client, wallet_name: &str) -> bitcoincore_rpc::Result<()> {
    match rpc.create_wallet(wallet_name, None, None, None, None) {
        Ok(_) => println!("Created wallet: {wallet_name}"),
        Err(_) => {
            // Wallet likely already exists on disk from a previous run —
            // load it instead of failing.
            rpc.load_wallet(wallet_name)?;
            println!("Loaded wallet: {wallet_name}");
        }
    }
    Ok(())
}

/// Builds an RPC client scoped to a specific wallet, e.g.
/// `.../wallet/Miner`. Every wallet-specific call (balances, addresses,
/// sends) has to go through a client scoped this way rather than the
/// base node client.
fn wallet_client(auth: &Auth, name: &str) -> bitcoincore_rpc::Result<Client> {
    Client::new(&format!("{RPC_URL}/wallet/{name}"), auth.clone())
}

/// Requests a new receiving address from a wallet, tagged with a label
/// so it's identifiable later (e.g. "Mining Reward", "Received").
fn create_receiving_address(rpc: &Client, label: &str) -> bitcoincore_rpc::Result<Address> {
    Ok(rpc.get_new_address(Some(label), None)?.assume_checked())
}

/// Mines blocks to `mining_addr`, one at a time, until the wallet
/// balance becomes positive.
///
/// Why this takes 101 blocks, not 1:
/// Every mined block pays a coinbase reward to `mining_addr`, but
/// coinbase outputs are consensus-locked and cannot be spent until
/// they have 100 confirmations. Bitcoin Core's wallet only counts a
/// coinbase output toward the *spendable* balance once it's matured,
/// so the very first block's reward stays invisible to get_balance
/// until 100 more blocks have been mined on top of it. That's why we
/// see the balance stay at zero for a long stretch and then jump to
/// positive all at once: block 1 creates the reward, blocks 2-101
/// mature it.
fn mine_until_positive_balance(
    rpc: &Client,
    mining_addr: &Address,
) -> bitcoincore_rpc::Result<u64> {
    let mut blocks_mined = 0u64;
    while rpc.get_balance(None, None)? == Amount::ZERO {
        rpc.generate_to_address(1, mining_addr)?;
        blocks_mined += 1;
    }
    Ok(blocks_mined)
}

/// Sends `amount_btc` to `addr` using the `send` RPC call. This isn't
/// exposed as a typed method on the `bitcoincore-rpc` crate, so we call
/// it manually and deserialize just the fields we need.
fn send_payment(rpc: &Client, addr: &Address, amount_btc: f64) -> bitcoincore_rpc::Result<Txid> {
    #[derive(Deserialize)]
    struct SendResult {
        complete: bool,
        txid: String,
    }

    let addr_str = addr.to_string();
    let args = [
        json!([{ addr_str: amount_btc }]), // recipient address -> amount
        json!(null),                       // conf target
        json!(null),                       // estimate mode
        json!(null),                       // fee rate in sats/vb
        json!(null),                       // options object
    ];
    let result = rpc.call::<SendResult>("send", &args)?;
    assert!(result.complete, "transaction did not complete");
    Ok(result.txid.parse().expect("valid txid returned from send"))
}

/// After the transaction is confirmed, pulls together every detail the
/// output file needs: the input being spent, which output went to the
/// Trader, which output is the Miner's own change, the fee, and where
/// it landed in the chain.
fn fetch_transaction_details(
    rpc: &Client,
    txid: Txid,
    trader_addr: &Address,
) -> bitcoincore_rpc::Result<TransactionSummary> {
    // Wallet-level view gives us fee and confirmation info directly.
    let wallet_tx = rpc.get_transaction(&txid, None)?;
    let fee = wallet_tx.fee.expect("fee present on wallet send").to_btc();
    let block_height = wallet_tx.info.blockheight.expect("tx confirmed");
    let block_hash = wallet_tx.info.blockhash.expect("tx confirmed");

    // Raw transaction gives us the actual inputs/outputs to inspect.
    let raw_tx_info = rpc.get_raw_transaction_info(&txid, None)?;

    // Resolve the Miner's input: our tx has exactly one input, which
    // references a previous transaction's output — look that output up
    // to find the address and amount that funded this send.
    let vin = &raw_tx_info.vin[0];
    let prev_txid = vin.txid.expect("non-coinbase input");
    let prev_vout = vin.vout.expect("non-coinbase input");
    let prev_tx_info = rpc.get_raw_transaction_info(&prev_txid, None)?;
    let prev_out = &prev_tx_info.vout[prev_vout as usize];
    let miner_input_address = prev_out
        .script_pub_key
        .address
        .clone()
        .expect("address on prev output")
        .assume_checked();
    let miner_input_amount = prev_out.value.to_btc();

    // Our tx has two outputs: the 20 BTC payment to Trader, and the
    // Miner's change. Whichever output isn't Trader's address must be
    // the change going back to Miner.
    let mut trader_amount = 0.0;
    let mut change_address = None;
    let mut change_amount = 0.0;
    for vout in &raw_tx_info.vout {
        if let Some(addr) = &vout.script_pub_key.address {
            let addr = addr.clone().assume_checked();
            if &addr == trader_addr {
                trader_amount = vout.value.to_btc();
            } else {
                change_address = Some(addr);
                change_amount = vout.value.to_btc();
            }
        }
    }
    let change_address = change_address.expect("change output present");

    Ok(TransactionSummary {
        txid,
        miner_input_address,
        miner_input_amount,
        trader_address: trader_addr.clone(),
        trader_amount,
        change_address,
        change_amount,
        fee,
        block_height,
        block_hash,
    })
}

/// Writes the summary to out.txt, one field per line, in the exact
/// order the grader expects.
fn write_output(path: &str, summary: &TransactionSummary) -> bitcoincore_rpc::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "{}", summary.txid)?;
    writeln!(file, "{}", summary.miner_input_address)?;
    writeln!(file, "{}", summary.miner_input_amount)?;
    writeln!(file, "{}", summary.trader_address)?;
    writeln!(file, "{}", summary.trader_amount)?;
    writeln!(file, "{}", summary.change_address)?;
    writeln!(file, "{}", summary.change_amount)?;
    writeln!(file, "{}", summary.fee)?;
    writeln!(file, "{}", summary.block_height)?;
    writeln!(file, "{}", summary.block_hash)?;
    Ok(())
}

fn main() -> bitcoincore_rpc::Result<()> {
    let auth = Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned());
    let base_rpc = Client::new(RPC_URL, auth.clone())?;

    // --- Wallet initialization ---
    // Both wallets must exist before we can scope clients to them.
    ensure_wallet(&base_rpc, "Miner")?;
    ensure_wallet(&base_rpc, "Trader")?;
    let miner_rpc = wallet_client(&auth, "Miner")?;
    let trader_rpc = wallet_client(&auth, "Trader")?;
    println!("Wallets ready.");

    // --- Mining ---
    // Mine to a fresh Miner address until its wallet balance turns
    // positive (see mine_until_positive_balance for why this takes
    // 101 blocks, not 1).
    let mining_addr = create_receiving_address(&miner_rpc, "Mining Reward")?;
    let blocks_mined = mine_until_positive_balance(&miner_rpc, &mining_addr)?;
    println!("Mined {blocks_mined} blocks before balance turned positive.");
    println!("Miner balance: {}", miner_rpc.get_balance(None, None)?);

    // --- Transaction creation ---
    // Get a Trader address to receive funds, then send 20 BTC to it.
    let trader_addr = create_receiving_address(&trader_rpc, "Received")?;
    println!("Trader address: {trader_addr}");
    let txid = send_payment(&miner_rpc, &trader_addr, 20.0)?;
    println!("Broadcast tx: {txid}");

    // --- Mempool check ---
    // Before it's confirmed, the transaction should be visible in the
    // node's mempool. Print the whole entry, as the assignment asks.
    let mempool_entry = miner_rpc.get_mempool_entry(&txid)?;
    println!("Mempool entry: {mempool_entry:#?}");

    // --- Transaction confirmation ---
    // Mine one block to confirm it.
    let confirm_blocks = miner_rpc.generate_to_address(1, &mining_addr)?;
    println!("Confirmed in block: {}", confirm_blocks[0]);

    // --- Transaction inspection ---
    // Pull together everything out.txt needs to describe the tx.
    let summary = fetch_transaction_details(&miner_rpc, txid, &trader_addr)?;

    // --- Output generation ---
    write_output("../out.txt", &summary)?;
    println!("Wrote transaction details to ../out.txt");

    Ok(())
}
