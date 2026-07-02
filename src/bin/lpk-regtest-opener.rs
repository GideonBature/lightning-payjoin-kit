#[cfg(not(feature = "corepc"))]
fn main() {
    eprintln!(
        "lpk-regtest-opener requires the corepc feature.\n\nRun:\n  cargo run --features corepc --bin lpk-regtest-opener"
    );
    std::process::exit(2);
}

#[cfg(feature = "corepc")]
fn main() {
    if let Err(error) = regtest_opener::run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "corepc")]
mod regtest_opener {
    use std::env;
    use std::error::Error as StdError;
    use std::io;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::hex::FromHex;
    use bitcoin::{
        key::{CompressedPublicKey, PrivateKey},
        secp256k1, Address, Amount, Network, NetworkKind, OutPoint, ScriptBuf, Transaction, Txid,
    };
    use lightning_payjoin_kit::chain::{CorepcAuth, CorepcRegtestClient};
    use lightning_payjoin_kit::directory::MockDirectory;
    use lightning_payjoin_kit::lightning::{
        ChannelBalance, ChannelFundingHandoff, CommitmentSafety, FundingScript,
        PayjoinChannelFunder, SimulatedChannelFunder,
    };
    use lightning_payjoin_kit::psbt::FinalizedFunding;
    use lightning_payjoin_kit::wallet::{MemoryWallet, Utxo, Wallet};
    use lightning_payjoin_kit::{FundingCoordinator, FundingMode, FundingPolicy, FundingRequest};
    use serde_json::Value;

    type AppResult<T> = Result<T, Box<dyn StdError>>;

    #[derive(Debug, Clone)]
    struct Config {
        rpc_url: String,
        rpc_user: String,
        rpc_password: String,
        channel_sats: u64,
        initiator_input_sats: u64,
        counterparty_input_sats: u64,
        fee_rate_sat_vb: f32,
        broadcast: bool,
        help: bool,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                rpc_url: "http://127.0.0.1:18443".to_owned(),
                rpc_user: "lpk".to_owned(),
                rpc_password: "lpk".to_owned(),
                channel_sats: 1_000_000,
                initiator_input_sats: 1_100_000,
                counterparty_input_sats: 200_000,
                fee_rate_sat_vb: 2.0,
                broadcast: true,
                help: false,
            }
        }
    }

    pub fn run() -> AppResult<()> {
        let config = parse_args()?;
        if config.help {
            print_help();
            return Ok(());
        }

        println!("Lightning Payjoin regtest opener");
        println!("  rpc: {}", config.rpc_url);
        println!("  channel: {} sats", config.channel_sats);
        println!("  mode: normal control + privacy-input channel open");
        println!();

        let auth = CorepcAuth::UserPass {
            user: config.rpc_user.clone(),
            password: config.rpc_password.clone(),
        };
        let base_rpc = CorepcRegtestClient::new(&config.rpc_url, auth.clone())?;
        let start_height = base_rpc.get_block_count()?;
        let wallet_name = unique_wallet_name();
        base_rpc.create_wallet(&wallet_name)?;

        let wallet_url = format!(
            "{}/wallet/{wallet_name}",
            config.rpc_url.trim_end_matches('/')
        );
        let wallet_rpc = CorepcRegtestClient::new(&wallet_url, auth.clone())?;

        println!("1. Preparing regtest funds");
        println!("  wallet: {wallet_name}");
        println!("  starting height: {start_height}");
        mine_mature_funds(&wallet_rpc)?;

        let initiator_key = private_key(1)?;
        let counterparty_key = private_key(2)?;
        let initiator_address = p2wpkh_address(&initiator_key)?;
        let counterparty_address = p2wpkh_address(&counterparty_key)?;

        let initiator_fund_txid = wallet_rpc.send_to_address(
            &initiator_address,
            Amount::from_sat(config.initiator_input_sats),
        )?;
        let counterparty_fund_txid = wallet_rpc.send_to_address(
            &counterparty_address,
            Amount::from_sat(config.counterparty_input_sats),
        )?;
        let confirmation_address = wallet_rpc.new_address()?;
        wallet_rpc.generate_to_address(1, &confirmation_address)?;

        let initiator_utxo = funded_utxo(
            &wallet_rpc,
            initiator_fund_txid,
            &initiator_address,
            Amount::from_sat(config.initiator_input_sats),
        )?;
        let counterparty_utxo = funded_utxo(
            &wallet_rpc,
            counterparty_fund_txid,
            &counterparty_address,
            Amount::from_sat(config.counterparty_input_sats),
        )?;
        let initiator_outpoint = initiator_utxo.outpoint;
        let counterparty_outpoint = counterparty_utxo.outpoint;
        println!("  initiator input: {initiator_outpoint}");
        println!("  counterparty privacy input: {counterparty_outpoint}");
        println!();

        let secp = secp256k1::Secp256k1::new();
        let funding_script = FundingScript::new_2of2(
            private_key(11)?.public_key(&secp),
            private_key(12)?.public_key(&secp),
        );
        let request = FundingRequest {
            channel_value_sats: config.channel_sats,
            funding_script: funding_script.script_pubkey.clone(),
            mode: FundingMode::PrivacyInput,
            fee_rate_sat_vb: config.fee_rate_sat_vb,
            deadline: Duration::from_secs(30),
        };
        let policy = FundingPolicy::default();
        let directory = MockDirectory::default();

        let mut initiator = FundingCoordinator::new(
            MemoryWallet::new_with_keys(
                vec![initiator_utxo],
                vec![initiator_address.script_pubkey()],
                vec![(initiator_outpoint, initiator_key)],
            ),
            directory.clone(),
            policy.clone(),
        );
        let mut counterparty = FundingCoordinator::new(
            MemoryWallet::new_with_keys(
                vec![counterparty_utxo],
                vec![counterparty_address.script_pubkey()],
                vec![(counterparty_outpoint, counterparty_key)],
            ),
            directory,
            policy,
        );

        println!("2. Building normal control transaction");
        let (session, original) = initiator.post_original_to_directory(&request)?;
        let mut normal_psbt = original.psbt.clone();
        let normal_signing = initiator.wallet().sign_owned_psbt(&mut normal_psbt)?;
        let normal =
            FinalizedFunding::extract(normal_psbt, original.funding_output_index, normal_signing)?;
        print_transaction_summary(
            "Normal single-funder channel open",
            &normal.transaction,
            &["initiator funding input"],
        );
        println!("  status: signed comparison transaction, not broadcast");
        println!();

        println!("3. Coordinating privacy-input funding through async directory");
        println!("  session: {}", session.id.as_str());
        let proposal = counterparty.propose_from_directory(&session.id, &request)?;
        let validation = initiator.validate_proposal_from_directory(&session.id, &original.psbt)?;
        println!(
            "  counterparty fee contribution: {} sats",
            proposal.counterparty_fee_contribution.to_sat()
        );
        println!("  added inputs: {}", validation.added_inputs);
        println!("  added outputs: {}", validation.added_outputs);
        println!("  added fee: {} sats", validation.added_fee.to_sat());

        let private_result =
            initiator.finalize_proposal_from_directory(&session.id, &original.psbt)?;
        print_transaction_summary(
            "Private collaborative channel open",
            &private_result.transaction,
            &["initiator funding input", "counterparty privacy input"],
        );
        println!();

        println!("4. Verifying Lightning funding boundary");
        let handoff = ChannelFundingHandoff::new(
            private_result.clone(),
            funding_script.script_pubkey.clone(),
            ChannelBalance {
                initiator_sats: config.channel_sats,
                counterparty_sats: 0,
            },
            FundingMode::PrivacyInput,
            CommitmentSafety::CommitmentsExchanged,
        );
        let mut channel_funder = SimulatedChannelFunder;
        let channel = channel_funder.accept_funding(handoff)?;
        println!(
            "  simulated channel accepted funding outpoint: {}",
            channel.funding_outpoint
        );
        println!(
            "  initiator channel balance: {} sats",
            channel.balance.initiator_sats
        );
        println!(
            "  counterparty channel balance: {} sats",
            channel.balance.counterparty_sats
        );
        println!();

        if config.broadcast {
            println!("5. Broadcasting private channel funding transaction");
            let mut broadcaster = CorepcRegtestClient::new(&config.rpc_url, auth)?;
            let broadcast_txid = initiator.broadcast_funding(&private_result, &mut broadcaster)?;
            let block_address = wallet_rpc.new_address()?;
            wallet_rpc.generate_to_address(1, &block_address)?;
            let end_height = base_rpc.get_block_count()?;
            println!("  broadcast txid: {broadcast_txid}");
            println!("  mined height: {end_height}");
        } else {
            println!("5. Broadcast skipped");
        }
        println!();

        print_privacy_proof(
            &normal.transaction,
            &private_result.transaction,
            config.channel_sats,
        );
        println!();
        println!("Private raw transaction hex:");
        println!("{}", serialize_hex(&private_result.transaction));

        Ok(())
    }

    fn parse_args() -> AppResult<Config> {
        let mut config = Config::default();
        let mut args = env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => config.help = true,
                "--rpc-url" => config.rpc_url = next_value(&mut args, "--rpc-url")?,
                "--rpc-user" => config.rpc_user = next_value(&mut args, "--rpc-user")?,
                "--rpc-password" => config.rpc_password = next_value(&mut args, "--rpc-password")?,
                "--channel-sats" => {
                    config.channel_sats = parse_u64(next_value(&mut args, "--channel-sats")?)?
                }
                "--initiator-input-sats" => {
                    config.initiator_input_sats =
                        parse_u64(next_value(&mut args, "--initiator-input-sats")?)?
                }
                "--counterparty-input-sats" => {
                    config.counterparty_input_sats =
                        parse_u64(next_value(&mut args, "--counterparty-input-sats")?)?
                }
                "--fee-rate" => {
                    config.fee_rate_sat_vb = parse_f32(next_value(&mut args, "--fee-rate")?)?
                }
                "--no-broadcast" => config.broadcast = false,
                unknown => return Err(input_error(format!("unknown argument: {unknown}"))),
            }
        }

        Ok(config)
    }

    fn next_value(
        args: &mut impl Iterator<Item = String>,
        flag: &'static str,
    ) -> AppResult<String> {
        args.next()
            .ok_or_else(|| input_error(format!("{flag} requires a value")))
    }

    fn parse_u64(value: String) -> AppResult<u64> {
        value
            .parse::<u64>()
            .map_err(|error| input_error(format!("invalid integer '{value}': {error}")))
    }

    fn parse_f32(value: String) -> AppResult<f32> {
        value
            .parse::<f32>()
            .map_err(|error| input_error(format!("invalid decimal '{value}': {error}")))
    }

    fn input_error(message: impl Into<String>) -> Box<dyn StdError> {
        Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
    }

    fn print_help() {
        println!(
            "\
Lightning Payjoin regtest opener

Usage:
  cargo run --features corepc --bin lpk-regtest-opener -- [options]

Options:
  --rpc-url <url>                    Bitcoin Core regtest RPC URL [default: http://127.0.0.1:18443]
  --rpc-user <user>                  RPC username [default: lpk]
  --rpc-password <password>          RPC password [default: lpk]
  --channel-sats <sats>              Channel capacity [default: 1000000]
  --initiator-input-sats <sats>      Initiator funding UTXO value [default: 1100000]
  --counterparty-input-sats <sats>   Counterparty privacy UTXO value [default: 200000]
  --fee-rate <sat/vb>                Funding fee rate [default: 2.0]
  --no-broadcast                     Build and verify, but do not broadcast
  -h, --help                         Show this help
"
        );
    }

    fn mine_mature_funds(wallet_rpc: &CorepcRegtestClient) -> AppResult<()> {
        let mining_address = wallet_rpc.new_address()?;
        wallet_rpc.generate_to_address(101, &mining_address)?;
        Ok(())
    }

    fn unique_wallet_name() -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis();
        format!("lpk-opener-{millis}")
    }

    fn private_key(secret_byte: u8) -> AppResult<PrivateKey> {
        let secret_key = secp256k1::SecretKey::from_slice(&[secret_byte; 32])?;
        Ok(PrivateKey::new(secret_key, NetworkKind::Test))
    }

    fn p2wpkh_address(private_key: &PrivateKey) -> AppResult<Address> {
        let secp = secp256k1::Secp256k1::new();
        let public_key = CompressedPublicKey::from_private_key(&secp, private_key)?;
        Ok(Address::p2wpkh(&public_key, Network::Regtest))
    }

    fn funded_utxo(
        rpc: &CorepcRegtestClient,
        txid: Txid,
        address: &Address,
        value: Amount,
    ) -> AppResult<Utxo> {
        Ok(Utxo {
            outpoint: OutPoint {
                txid,
                vout: funded_vout(rpc, txid, address)?,
            },
            value,
            script_pubkey: address.script_pubkey(),
            confirmed: true,
        })
    }

    fn funded_vout(rpc: &CorepcRegtestClient, txid: Txid, address: &Address) -> AppResult<u32> {
        let tx = rpc.get_wallet_transaction(txid)?;
        let vouts = tx
            .pointer("/decoded/vout")
            .and_then(Value::as_array)
            .ok_or_else(|| input_error("wallet transaction response is missing decoded vouts"))?;

        vouts
            .iter()
            .find_map(|vout| {
                let script_hex = vout.pointer("/scriptPubKey/hex")?.as_str()?;
                let script_bytes = Vec::<u8>::from_hex(script_hex).ok()?;
                let script = ScriptBuf::from_bytes(script_bytes);
                if script == address.script_pubkey() {
                    let n = vout.get("n")?.as_u64()?;
                    Some(n as u32)
                } else {
                    None
                }
            })
            .ok_or_else(|| input_error("funding output was not found in wallet transaction"))
    }

    fn print_transaction_summary(label: &str, tx: &Transaction, input_roles: &[&str]) {
        println!("{label}");
        println!("  txid: {}", tx.compute_txid());
        println!("  inputs: {}", tx.input.len());
        for (index, input) in tx.input.iter().enumerate() {
            let role = input_roles.get(index).copied().unwrap_or("unknown input");
            println!("    [{index}] {role}: {}", input.previous_output);
        }
        println!("  outputs: {}", tx.output.len());
        for (index, output) in tx.output.iter().enumerate() {
            println!(
                "    [{index}] {} sats ({})",
                output.value.to_sat(),
                output_kind(&output.script_pubkey, index)
            );
        }
    }

    fn output_kind(script: &ScriptBuf, index: usize) -> &'static str {
        if index == 0 && script.is_p2wsh() {
            "channel funding p2wsh"
        } else if script.is_p2wpkh() {
            "wallet p2wpkh change"
        } else if script.is_p2wsh() {
            "p2wsh"
        } else {
            "other script"
        }
    }

    fn print_privacy_proof(normal: &Transaction, private: &Transaction, channel_sats: u64) {
        println!("Evidence");
        println!(
            "  normal open: {} input, {} outputs, channel output {} sats",
            normal.input.len(),
            normal.output.len(),
            normal.output[0].value.to_sat()
        );
        println!(
            "  private open: {} inputs, {} outputs, channel output {} sats",
            private.input.len(),
            private.output.len(),
            private.output[0].value.to_sat()
        );
        println!("  expected channel capacity: {channel_sats} sats");
        println!("  privacy claim: the broadcast transaction has inputs from both peers");
        println!(
            "  accounting claim: the counterparty privacy input does not receive channel balance"
        );
    }
}
