// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use clarity::vm::ast::ASTRules;
use clarity::vm::costs::ExecutionCost;
use clarity::vm::types::PrincipalData;
use lazy_static::lazy_static;
use stacks::burnchains::MagicBytes;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::nakamoto::miner::{NakamotoBlockBuilder, NakamotoBlockProposal};
use stacks::chainstate::nakamoto::NakamotoChainState;
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::chainstate::stacks::miner::{BlockBuilder, BlockLimitFunction, TransactionResult};
use stacks::chainstate::stacks::{StacksTransaction, TransactionPayload};
use stacks::core::{
    StacksEpoch, StacksEpochId, BLOCK_LIMIT_MAINNET_10, HELIUM_BLOCK_LIMIT_20,
    PEER_VERSION_EPOCH_1_0, PEER_VERSION_EPOCH_2_0, PEER_VERSION_EPOCH_2_05,
    PEER_VERSION_EPOCH_2_1, PEER_VERSION_EPOCH_2_2, PEER_VERSION_EPOCH_2_3, PEER_VERSION_EPOCH_2_4,
    PEER_VERSION_EPOCH_2_5, PEER_VERSION_EPOCH_3_0,
};
use stacks_common::address::AddressHashMode;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::consts::STACKS_EPOCH_MAX;
use stacks_common::types::chainstate::{StacksAddress, StacksPrivateKey};
use stacks_common::util::hash::to_hex;
use stacks_common::util::secp256k1::Secp256k1PrivateKey;

use super::bitcoin_regtest::BitcoinCoreController;
use crate::config::{EventKeyType, EventObserverConfig, InitialBalance};
use crate::mockamoto::signer::SelfSigner;
use crate::neon::{Counters, RunLoopCounter};
use crate::run_loop::boot_nakamoto;
use crate::tests::neon_integrations::{
    next_block_and_wait, run_until_burnchain_height, submit_tx, test_observer, wait_for_runloop,
};
use crate::tests::{make_stacks_transfer, to_addr};
use crate::{tests, BitcoinRegtestController, BurnchainController, Config, ConfigFile, Keychain};

static POX_4_DEFAULT_STACKER_BALANCE: u64 = 100_000_000_000_000;
static POX_4_DEFAULT_STACKER_STX_AMT: u128 = 99_000_000_000_000;

lazy_static! {
    pub static ref NAKAMOTO_INTEGRATION_EPOCHS: [StacksEpoch; 9] = [
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch10,
            start_height: 0,
            end_height: 0,
            block_limit: BLOCK_LIMIT_MAINNET_10.clone(),
            network_epoch: PEER_VERSION_EPOCH_1_0
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch20,
            start_height: 0,
            end_height: 1,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_0
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch2_05,
            start_height: 1,
            end_height: 2,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_05
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch21,
            start_height: 2,
            end_height: 3,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_1
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch22,
            start_height: 3,
            end_height: 4,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_2
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch23,
            start_height: 4,
            end_height: 5,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_3
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch24,
            start_height: 5,
            end_height: 6,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_4
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch25,
            start_height: 6,
            end_height: 221,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_2_5
        },
        StacksEpoch {
            epoch_id: StacksEpochId::Epoch30,
            start_height: 221,
            end_height: STACKS_EPOCH_MAX,
            block_limit: HELIUM_BLOCK_LIMIT_20.clone(),
            network_epoch: PEER_VERSION_EPOCH_3_0
        },
    ];
}

pub fn add_initial_balances(
    conf: &mut Config,
    accounts: usize,
    amount: u64,
) -> Vec<StacksPrivateKey> {
    (0..accounts)
        .map(|i| {
            let privk = StacksPrivateKey::from_seed(&[5, 5, 5, i as u8]);
            let address = to_addr(&privk).into();

            conf.initial_balances
                .push(InitialBalance { address, amount });
            privk
        })
        .collect()
}

/// Return a working nakamoto-neon config and the miner's bitcoin address to fund
pub fn naka_neon_integration_conf(seed: Option<&[u8]>) -> (Config, StacksAddress) {
    let mut conf = super::new_test_conf();

    conf.burnchain.mode = "nakamoto-neon".into();

    // tests can override this, but these tests run with epoch 2.05 by default
    conf.burnchain.epochs = Some(NAKAMOTO_INTEGRATION_EPOCHS.to_vec());

    if let Some(seed) = seed {
        conf.node.seed = seed.to_vec();
    }

    // instantiate the keychain so we can fund the bitcoin op signer
    let keychain = Keychain::default(conf.node.seed.clone());

    let mining_key = Secp256k1PrivateKey::from_seed(&[1]);
    conf.miner.mining_key = Some(mining_key);
    conf.miner.self_signing_key = Some(SelfSigner::from_seed(7));

    conf.node.miner = true;
    conf.node.wait_time_for_microblocks = 500;
    conf.burnchain.burn_fee_cap = 20000;

    conf.burnchain.username = Some("neon-tester".into());
    conf.burnchain.password = Some("neon-tester-pass".into());
    conf.burnchain.peer_host = "127.0.0.1".into();
    conf.burnchain.local_mining_public_key =
        Some(keychain.generate_op_signer().get_public_key().to_hex());
    conf.burnchain.commit_anchor_block_within = 0;

    // test to make sure config file parsing is correct
    let mut cfile = ConfigFile::xenon();
    cfile.node.as_mut().map(|node| node.bootstrap_node.take());

    if let Some(burnchain) = cfile.burnchain.as_mut() {
        burnchain.peer_host = Some("127.0.0.1".to_string());
    }

    conf.burnchain.magic_bytes = MagicBytes::from(['T' as u8, '3' as u8].as_ref());
    conf.burnchain.poll_time_secs = 1;
    conf.node.pox_sync_sample_secs = 0;

    conf.miner.min_tx_fee = 1;
    conf.miner.first_attempt_time_ms = i64::max_value() as u64;
    conf.miner.subsequent_attempt_time_ms = i64::max_value() as u64;

    // if there's just one node, then this must be true for tests to pass
    conf.miner.wait_for_block_download = false;

    conf.node.mine_microblocks = false;
    conf.miner.microblock_attempt_time_ms = 10;
    conf.node.microblock_frequency = 0;
    conf.node.wait_time_for_blocks = 200;

    let miner_account = keychain.origin_address(conf.is_mainnet()).unwrap();

    conf.burnchain.pox_prepare_length = Some(5);
    conf.burnchain.pox_reward_length = Some(20);

    (conf, miner_account)
}

pub fn next_block_and<F>(
    btc_controller: &mut BitcoinRegtestController,
    timeout_secs: u64,
    mut check: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<bool, String>,
{
    eprintln!("Issuing bitcoin block");
    btc_controller.build_next_block(1);
    let start = Instant::now();
    while !check()? {
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            error!("Timed out waiting for block to process, trying to continue test");
            return Err("Timed out".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Mine a bitcoin block, and wait until:
///  (1) a new block has been processed by the coordinator
///  (2) 2 block commits have been issued ** or ** more than 10 seconds have
///      passed since (1) occurred
fn next_block_and_mine_commit(
    btc_controller: &mut BitcoinRegtestController,
    timeout_secs: u64,
    coord_channels: &Arc<Mutex<CoordinatorChannels>>,
    commits_submitted: &Arc<AtomicU64>,
) -> Result<(), String> {
    let commits_submitted = commits_submitted.clone();
    let blocks_processed_before = coord_channels
        .lock()
        .expect("Mutex poisoned")
        .get_stacks_blocks_processed();
    let commits_before = commits_submitted.load(Ordering::SeqCst);
    let mut block_processed_time: Option<Instant> = None;
    next_block_and(btc_controller, timeout_secs, || {
        if let Some(block_processed_time) = block_processed_time.as_ref() {
            let commits_sent = commits_submitted.load(Ordering::SeqCst);
            if commits_sent >= commits_before + 2 {
                return Ok(true);
            }
            if commits_sent >= commits_before + 1
                && block_processed_time.elapsed() > Duration::from_secs(6)
            {
                return Ok(true);
            }
            Ok(false)
        } else {
            let blocks_processed = coord_channels
                .lock()
                .expect("Mutex poisoned")
                .get_stacks_blocks_processed();
            if blocks_processed > blocks_processed_before {
                block_processed_time.replace(Instant::now());
            }
            Ok(false)
        }
    })
}

fn setup_stacker(naka_conf: &mut Config) -> Secp256k1PrivateKey {
    let stacker_sk = Secp256k1PrivateKey::new();
    let stacker_address = tests::to_addr(&stacker_sk);
    naka_conf.add_initial_balance(
        PrincipalData::from(stacker_address.clone()).to_string(),
        POX_4_DEFAULT_STACKER_BALANCE,
    );
    stacker_sk
}

///
/// * `stacker_sk` - must be a private key for sending a large `stack-stx` transaction in order
///   for pox-4 to activate
fn boot_to_epoch_3(
    naka_conf: &Config,
    blocks_processed: &RunLoopCounter,
    stacker_sk: Secp256k1PrivateKey,
    btc_regtest_controller: &mut BitcoinRegtestController,
) {
    let epochs = naka_conf.burnchain.epochs.clone().unwrap();
    let epoch_3 = &epochs[StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch30).unwrap()];

    info!(
        "Chain bootstrapped to bitcoin block 201, starting Epoch 2x miner";
        "Epoch 3.0 Boundary" => (epoch_3.start_height - 1),
    );
    let http_origin = format!("http://{}", &naka_conf.node.rpc_bind);
    next_block_and_wait(btc_regtest_controller, &blocks_processed);
    next_block_and_wait(btc_regtest_controller, &blocks_processed);
    // first mined stacks block
    next_block_and_wait(btc_regtest_controller, &blocks_processed);

    // stack enough to activate pox-4
    let pox_addr_tuple = clarity::vm::tests::execute(&format!(
        "{{ hashbytes: 0x{}, version: 0x{:02x} }}",
        to_hex(&[0; 20]),
        AddressHashMode::SerializeP2PKH as u8,
    ));

    let stacking_tx = tests::make_contract_call(
        &stacker_sk,
        0,
        1000,
        &StacksAddress::burn_address(false),
        "pox-4",
        "stack-stx",
        &[
            clarity::vm::Value::UInt(POX_4_DEFAULT_STACKER_STX_AMT),
            pox_addr_tuple,
            clarity::vm::Value::UInt(205),
            clarity::vm::Value::UInt(12),
        ],
    );

    submit_tx(&http_origin, &stacking_tx);

    run_until_burnchain_height(
        btc_regtest_controller,
        &blocks_processed,
        epoch_3.start_height - 1,
        &naka_conf,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, Epoch2x miner should stop");
}

#[test]
#[ignore]
/// This test spins up a nakamoto-neon node.
/// It starts in Epoch 2.0, mines with `neon_node` to Epoch 3.0, and then switches
///  to Nakamoto operation (activating pox-4 by submitting a stack-stx tx). The BootLoop
///  struct handles the epoch-2/3 tear-down and spin-up.
/// This test makes three assertions:
///  * 30 blocks are mined after 3.0 starts. This is enough to mine across 2 reward cycles
///  * A transaction submitted to the mempool in 3.0 will be mined in 3.0
///  * The final chain tip is a nakamoto block
fn simple_neon_integration() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let (mut naka_conf, _miner_account) = naka_neon_integration_conf(None);
    let sender_sk = Secp256k1PrivateKey::new();
    // setup sender + recipient for a test stx transfer
    let sender_addr = tests::to_addr(&sender_sk);
    let send_amt = 1000;
    let send_fee = 100;
    naka_conf.add_initial_balance(
        PrincipalData::from(sender_addr.clone()).to_string(),
        send_amt + send_fee,
    );
    let recipient = PrincipalData::from(StacksAddress::burn_address(false));
    let stacker_sk = setup_stacker(&mut naka_conf);

    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .expect("Failed starting bitcoind");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);
    btc_regtest_controller.bootstrap_chain(201);

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();

    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));
    wait_for_runloop(&blocks_processed);
    boot_to_epoch_3(
        &naka_conf,
        &blocks_processed,
        stacker_sk,
        &mut btc_regtest_controller,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, starting nakamoto miner");

    let burnchain = naka_conf.get_burnchain();
    let sortdb = burnchain.open_sortition_db(true).unwrap();
    let (mut chainstate, _) = StacksChainState::open(
        naka_conf.is_mainnet(),
        naka_conf.burnchain.chain_id,
        &naka_conf.get_chainstate_path_str(),
        None,
    )
    .unwrap();

    let block_height_pre_3_0 =
        NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
            .unwrap()
            .unwrap()
            .stacks_block_height;

    info!("Nakamoto miner started...");
    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(&mut btc_regtest_controller, 60, || {
        let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
        Ok(vrf_count >= 1)
    })
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(&mut btc_regtest_controller, 60, || {
        let commits_count = commits_submitted.load(Ordering::SeqCst);
        Ok(commits_count >= 1)
    })
    .unwrap();

    // Mine 15 nakamoto tenures
    for _i in 0..15 {
        next_block_and_mine_commit(
            &mut btc_regtest_controller,
            60,
            &coord_channel,
            &commits_submitted,
        )
        .unwrap();
    }

    // Submit a TX
    let transfer_tx = make_stacks_transfer(&sender_sk, 0, send_fee, &recipient, send_amt);
    let transfer_tx_hex = format!("0x{}", to_hex(&transfer_tx));

    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();

    let mut mempool = naka_conf
        .connect_mempool_db()
        .expect("Database failure opening mempool");

    mempool
        .submit_raw(
            &mut chainstate,
            &sortdb,
            &tip.consensus_hash,
            &tip.anchored_header.block_hash(),
            transfer_tx.clone(),
            &ExecutionCost::max_value(),
            &StacksEpochId::Epoch30,
        )
        .unwrap();

    // Mine 15 more nakamoto tenures
    for _i in 0..15 {
        next_block_and_mine_commit(
            &mut btc_regtest_controller,
            60,
            &coord_channel,
            &commits_submitted,
        )
        .unwrap();
    }

    // load the chain tip, and assert that it is a nakamoto block and at least 30 blocks have advanced in epoch 3
    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();
    info!(
        "Latest tip";
        "height" => tip.stacks_block_height,
        "is_nakamoto" => tip.anchored_header.as_stacks_nakamoto().is_some(),
    );

    // assert that the transfer tx was observed
    let transfer_tx_included = test_observer::get_blocks()
        .into_iter()
        .find(|block_json| {
            block_json["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tx_json| tx_json["raw_tx"].as_str() == Some(&transfer_tx_hex))
                .is_some()
        })
        .is_some();

    assert!(
        transfer_tx_included,
        "Nakamoto node failed to include the transfer tx"
    );

    assert!(tip.anchored_header.as_stacks_nakamoto().is_some());
    assert!(tip.stacks_block_height >= block_height_pre_3_0 + 30);

    coord_channel
        .lock()
        .expect("Mutex poisoned")
        .stop_chains_coordinator();
    run_loop_stopper.store(false, Ordering::SeqCst);

    run_loop_thread.join().unwrap();
}

/// Test `/v2/block_proposal` API endpoint
///
/// This endpoint allows miners to propose Nakamoto blocks to a node,
/// and test if they would be accepted or rejected
///
/// Notes:
/// - The `tenure_start_block` supplied doesn't seem to matter. It is required by `NakamotoBlockBuilder` but not used/checked?
#[test]
#[ignore]
fn block_proposal_api_endpoint() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let (mut conf, _miner_account) = naka_neon_integration_conf(None);
    let account_keys = add_initial_balances(&mut conf, 10, 1000000);
    let stacker_sk = setup_stacker(&mut conf);

    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller
        .start_bitcoind()
        .expect("Failed starting bitcoind");
    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    btc_regtest_controller.bootstrap_chain(201);

    let mut run_loop = boot_nakamoto::BootRunLoop::new(conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();

    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));
    wait_for_runloop(&blocks_processed);
    boot_to_epoch_3(
        &conf,
        &blocks_processed,
        stacker_sk,
        &mut btc_regtest_controller,
    );

    info!("Bootstrapped to Epoch-3.0 boundary, starting nakamoto miner");

    let burnchain = conf.get_burnchain();
    let sortdb = burnchain.open_sortition_db(true).unwrap();
    let (mut chainstate, _) = StacksChainState::open(
        conf.is_mainnet(),
        conf.burnchain.chain_id,
        &conf.get_chainstate_path_str(),
        None,
    )
    .unwrap();

    let _block_height_pre_3_0 =
        NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
            .unwrap()
            .unwrap()
            .stacks_block_height;

    info!("Nakamoto miner started...");

    // first block wakes up the run loop, wait until a key registration has been submitted.
    next_block_and(&mut btc_regtest_controller, 60, || {
        let vrf_count = vrfs_submitted.load(Ordering::SeqCst);
        Ok(vrf_count >= 1)
    })
    .unwrap();

    // second block should confirm the VRF register, wait until a block commit is submitted
    next_block_and(&mut btc_regtest_controller, 60, || {
        let commits_count = commits_submitted.load(Ordering::SeqCst);
        Ok(commits_count >= 1)
    })
    .unwrap();

    // Mine 15 nakamoto tenures
    for _ in 0..15 {
        next_block_and_mine_commit(
            &mut btc_regtest_controller,
            60,
            &coord_channel,
            &commits_submitted,
        )
        .unwrap();
    }

    // TODO (hack) instantiate the sortdb in the burnchain
    _ = btc_regtest_controller.sortdb_mut();

    // Set up test signer
    let signer = conf.miner.self_signing_key.as_mut().unwrap();

    // ----- Setup boilerplate finished, test block proposal API endpoint -----

    let tip = NakamotoChainState::get_canonical_block_header(chainstate.db(), &sortdb)
        .unwrap()
        .unwrap();

    let privk = conf.miner.mining_key.unwrap().clone();
    let parent_block_id = tip.index_block_hash();
    // TODO: Get current `total_burn` from somewhere
    let sort_tip = SortitionDB::get_canonical_sortition_tip(sortdb.conn())
        .expect("Failed to get sortition tip");
    let db_handle = sortdb.index_handle(&sort_tip);
    let snapshot = db_handle
        .get_block_snapshot(&tip.burn_header_hash)
        .expect("Failed to get block snapshot")
        .expect("No snapshot");
    // Double check we got the right sortition
    assert_eq!(
        snapshot.consensus_hash, tip.consensus_hash,
        "Found incorrect block snapshot"
    );
    let total_burn = snapshot.total_burn;
    let tenure_change = None;
    let coinbase = None;

    let tenure_cause = tenure_change.and_then(|tx: &StacksTransaction| match &tx.payload {
        TransactionPayload::TenureChange(tc) => Some(tc.cause),
        _ => None,
    });

    // Apply both miner/stacker signatures
    let mut sign = |mut p: NakamotoBlockProposal| {
        p.block
            .header
            .sign_miner(&privk)
            .expect("Miner failed to sign");
        signer.sign_nakamoto_block(&mut p.block);
        p
    };

    let block = {
        let mut builder = NakamotoBlockBuilder::new_from_parent(
            &parent_block_id,
            &tip,
            &tip.consensus_hash,
            total_burn,
            tenure_change,
            coinbase,
        )
        .expect("Failed to build Nakamoto block");

        let burn_dbconn = btc_regtest_controller.sortdb_ref().index_conn();
        let mut miner_tenure_info = builder
            .load_tenure_info(&mut chainstate, &burn_dbconn, tenure_cause)
            .unwrap();
        let mut tenure_tx = builder
            .tenure_begin(&burn_dbconn, &mut miner_tenure_info)
            .unwrap();

        let tx = make_stacks_transfer(
            &account_keys[0],
            0,
            100,
            &to_addr(&account_keys[1]).into(),
            10000,
        );
        let tx = StacksTransaction::consensus_deserialize(&mut &tx[..])
            .expect("Failed to deserialize transaction");
        let tx_len = tx.tx_len();

        let res = builder.try_mine_tx_with_len(
            &mut tenure_tx,
            &tx,
            tx_len,
            &BlockLimitFunction::NO_LIMIT_HIT,
            ASTRules::PrecheckSize,
        );
        assert!(
            matches!(res, TransactionResult::Success(..)),
            "Transaction failed"
        );
        builder.mine_nakamoto_block(&mut tenure_tx)
    };

    // Construct a valid proposal. Make alterations to this to test failure cases
    let proposal = NakamotoBlockProposal {
        block,
        tenure_start_block: parent_block_id,
        chain_id: chainstate.chain_id,
    };

    const HTTP_ACCEPTED: u16 = 202;
    const HTTP_BADREQUEST: u16 = 400;
    // TODO: Check error codes?
    let test_cases = [
        (
            "Valid Nakamoto block proposal",
            sign(proposal.clone()),
            HTTP_ACCEPTED,
        ),
        (
            "Corrupted (bit flipped after signing)",
            (|| {
                let mut sp = sign(proposal.clone());
                sp.block.header.consensus_hash.0[3] ^= 0x07;
                sp
            })(),
            HTTP_BADREQUEST,
        ),
        (
            // FIXME: Why does `NakamotoBlockBuilder` not check this?
            "Invalid `tenure_start_block`",
            (|| {
                let mut p = proposal.clone();
                p.tenure_start_block.0[8] ^= 0x55;
                sign(p)
            })(),
            HTTP_ACCEPTED,
        ),
    ];

    // Build HTTP client
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("Failed to build reqwest::Client");
    // Build URL
    let http_origin = format!("http://{}", &conf.node.rpc_bind);
    let path = format!("{http_origin}/v2/block_proposal");

    for (test_description, block_proposal, expected_response) in test_cases {
        eprintln!("test_block_proposal(): {test_description}");
        eprintln!("{block_proposal:?}");

        // Send POST request
        let response = client
            .post(&path)
            .header("Content-Type", "application/json")
            .json(&block_proposal)
            .send()
            .expect("Failed to POST");

        eprintln!("{response:?}");
        assert_eq!(response.status().as_u16(), expected_response);
    }

    // Clean up
    coord_channel
        .lock()
        .expect("Mutex poisoned")
        .stop_chains_coordinator();
    run_loop_stopper.store(false, Ordering::SeqCst);

    run_loop_thread.join().unwrap();
}
