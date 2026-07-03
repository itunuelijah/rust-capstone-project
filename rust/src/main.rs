#![allow(unused)]
use bitcoincore_rpc::bitcoin::{Address, Amount, BlockHash, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;

// Node access params
const RPC_URL: &str = "http://127.0.0.1:18443"; // Default regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

// You can use calls not provided in RPC lib API using the generic `call` function.
// An example of using the `send` RPC call, which doesn't have exposed API.
// You can also use serde_json `Deserialize` derivation to capture the returned json result.

/// This function is used to send `amount_in_btc` to `recipient_address` using the `send` RPC call. I call it with
/// the generic `.call()` method and deserialize only the fields that is needed from the response.
fn send(
    rpc: &Client,
    recipient_address: &str,
    amount_in_btc: f64,
) -> bitcoincore_rpc::Result<String> {
    let args = [
        json!([{recipient_address : amount_in_btc}]), // recipient address. Instead of hardcoding the amount, it is passed as a parameter to the function.
        json!(null),                                  // conf target
        json!(null),                                  // estimate mode
        json!(null),                                  // fee rate in sats/vb
        json!(null),                                  // Empty option object
    ];

    #[derive(Deserialize)]
    struct SendResult {
        complete: bool,
        txid: String,
    }
    let send_result = rpc.call::<SendResult>("send", &args)?;
    assert!(send_result.complete);
    Ok(send_result.txid)
}

/// I use this struct to hold every piece of information I need to
/// write to out.txt, so it can be passed around as a value
struct TransactionSummary {
    transaction_id: Txid,
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

/// This function create a wallet if it doesn't exist yet or load it if it already exists.
/// So, the error it ought to give serves as a signal to load the existing wallet.
fn create_wallet(node_client: &Client, wallet_name: &str) -> bitcoincore_rpc::Result<()> {
    match node_client.create_wallet(wallet_name, None, None, None, None) {
        Ok(_) => println!("Created wallet: {wallet_name}"),
        Err(_) => {
            //  Loading the wallet instead of failing.
            node_client.load_wallet(wallet_name)?;
            println!("Loaded wallet: {wallet_name}");
        }
    }
    Ok(())
}

/// This function is to build an RPC client scoped to a specific wallet
fn wallet_client(authentication: &Auth, wallet_name: &str) -> bitcoincore_rpc::Result<Client> {
    Client::new(
        &format!("{RPC_URL}/wallet/{wallet_name}"),
        authentication.clone(),
    )
}

/// This function request a new receiving address from a wallet
fn create_receiving_address(
    wallet_client: &Client,
    label: &str,
) -> bitcoincore_rpc::Result<Address> {
    Ok(wallet_client
        .get_new_address(Some(label), None)?
        .assume_checked())
}

/// This function mines blocks to mining_address, one at a time, until the wallet balance becomes positive, instead of hardcoding it.
/// This takes 101 blocks and not just 1 because every block that is mined pays a coinbase reward to `mining_address`, but
/// coinbase outputs are locked and cannot be spent until they have 100 confirmations. Bitcoin Core's wallet only
/// counts a coinbase output toward spendable balance once it has matured, therefore, the first block's reward stays invisible to
/// get_balance until it mined 100 more blocks on top of it.
fn mine_until_positive_balance(
    wallet_client: &Client,
    mining_address: &Address,
) -> bitcoincore_rpc::Result<u64> {
    let mut number_of_blocks_mined = 0u64;
    while wallet_client.get_balance(None, None)? == Amount::ZERO {
        wallet_client.generate_to_address(1, mining_address)?;
        number_of_blocks_mined += 1;
    }
    Ok(number_of_blocks_mined)
}

/// This function gathers every detail out.txt needs, once the transaction has been confirmed.
fn fetch_transaction_details(
    wallet_client: &Client,
    transaction_id: Txid,
    trader_address: &Address,
) -> bitcoincore_rpc::Result<TransactionSummary> {
    // I use the wallet-level view of the transaction to get the fee
    // and confirmation details directly.
    let wallet_transaction = wallet_client.get_transaction(&transaction_id, None)?;
    let fee = wallet_transaction
        .fee
        .expect("I expect a fee to be present on a wallet-initiated send")
        .to_btc();
    let block_height = wallet_transaction
        .info
        .blockheight
        .expect("I expect the transaction to be confirmed at this point");
    let block_hash = wallet_transaction
        .info
        .blockhash
        .expect("I expect the transaction to be confirmed at this point");

    // I use the raw transaction to inspect the actual inputs and
    // outputs.
    let raw_transaction_info = wallet_client.get_raw_transaction_info(&transaction_id, None)?;

    //  The Miner's input address is resolved. The transaction has
    // exactly one input which tells previous output it spends (a transaction id and an output index)
    //  So it look up that previous transaction to find the address and amount that funded this send.
    let first_input = &raw_transaction_info.vin[0];
    let previous_transaction_id = first_input
        .txid
        .expect("I expect a non-coinbase input here");
    let previous_output_index = first_input
        .vout
        .expect("I expect a non-coinbase input here");
    let previous_transaction_info =
        wallet_client.get_raw_transaction_info(&previous_transaction_id, None)?;
    let previous_output = &previous_transaction_info.vout[previous_output_index as usize];
    let miner_input_address = previous_output
        .script_pub_key
        .address
        .clone()
        .expect("I expect an address on the previous output")
        .assume_checked();
    let miner_input_amount = previous_output.value.to_btc();

    // Loop through the two outputs of the transaction: the 20
    // BTC payment to the Trader, and the Miner's own change. Whichever
    // output address is not the Trader's address must be the change
    // going back to the Miner.
    let mut trader_amount = 0.0;
    let mut change_address = None;
    let mut change_amount = 0.0;
    for output in &raw_transaction_info.vout {
        if let Some(output_address) = &output.script_pub_key.address {
            let output_address = output_address.clone().assume_checked();
            if &output_address == trader_address {
                trader_amount = output.value.to_btc();
            } else {
                change_address = Some(output_address);
                change_amount = output.value.to_btc();
            }
        }
    }
    let change_address = change_address.expect("I expect a change output to be present");

    Ok(TransactionSummary {
        transaction_id,
        miner_input_address,
        miner_input_amount,
        trader_address: trader_address.clone(),
        trader_amount,
        change_address,
        change_amount,
        fee,
        block_height,
        block_hash,
    })
}

/// This writes the transaction summary to out.txt, with one field per line
fn write_output(
    output_file_path: &str,
    transaction_summary: &TransactionSummary,
) -> bitcoincore_rpc::Result<()> {
    let mut output_file = File::create(output_file_path)?;
    writeln!(output_file, "{}", transaction_summary.transaction_id)?;
    writeln!(output_file, "{}", transaction_summary.miner_input_address)?;
    writeln!(output_file, "{}", transaction_summary.miner_input_amount)?;
    writeln!(output_file, "{}", transaction_summary.trader_address)?;
    writeln!(output_file, "{}", transaction_summary.trader_amount)?;
    writeln!(output_file, "{}", transaction_summary.change_address)?;
    writeln!(output_file, "{}", transaction_summary.change_amount)?;
    writeln!(output_file, "{}", transaction_summary.fee)?;
    writeln!(output_file, "{}", transaction_summary.block_height)?;
    writeln!(output_file, "{}", transaction_summary.block_hash)?;
    Ok(())
}

fn main() -> bitcoincore_rpc::Result<()> {
    // Connect to Bitcoin Core RPC
    let authentication = Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned());
    let rpc = Client::new(RPC_URL, authentication.clone())?;

    // Get blockchain info
    let blockchain_info = rpc.get_blockchain_info()?;
    println!("Blockchain Info: {:?}", blockchain_info);

    // Create/Load the wallets, named 'Miner' and 'Trader'. Have logic to optionally create/load them if they do not exist or not loaded already.
    create_wallet(&rpc, "Miner")?;
    create_wallet(&rpc, "Trader")?;
    let miner_wallet_client = wallet_client(&authentication, "Miner")?;
    let trader_wallet_client = wallet_client(&authentication, "Trader")?;
    println!("Wallets ready.");

    // Generate spendable balances in the Miner wallet. How many blocks needs to be mined?
    let mining_address = create_receiving_address(&miner_wallet_client, "Mining Reward")?;
    let number_of_blocks_mined =
        mine_until_positive_balance(&miner_wallet_client, &mining_address)?;
    println!("Mined {number_of_blocks_mined} blocks before balance turned positive.");
    println!(
        "Miner balance: {}",
        miner_wallet_client.get_balance(None, None)?
    );

    // Load Trader wallet and generate a new address
    let trader_address = create_receiving_address(&trader_wallet_client, "Received")?;
    println!("Trader address: {trader_address}");

    // Send 20 BTC from Miner to Trader
    let txid: Txid = send(&miner_wallet_client, &trader_address.to_string(), 20.0)?
        .parse()
        .expect("I expect a valid transaction id to be returned from send");
    println!("Broadcast tx: {txid}");

    // Check transaction in mempool
    let mempool_entry = miner_wallet_client.get_mempool_entry(&txid)?;
    println!("Mempool entry: {mempool_entry:#?}");

    // Mine 1 block to confirm the transaction
    let confirm_blocks = miner_wallet_client.generate_to_address(1, &mining_address)?;
    println!("Confirmed in block: {}", confirm_blocks[0]);

    // Extract all required transaction details
    let summary = fetch_transaction_details(&miner_wallet_client, txid, &trader_address)?;

    // Write the data to ../out.txt in the specified format given in readme.md
    write_output("../out.txt", &summary)?;
    println!("Wrote transaction details to ../out.txt");

    Ok(())
}
