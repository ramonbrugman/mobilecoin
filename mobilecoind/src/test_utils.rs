// Copyright (c) 2018-2020 MobileCoin Inc.

//! Utilities for mobilecoind unit tests

// TODO
#![allow(dead_code)]

use crate::{
    database::Database,
    monitor_store::{MonitorData, MonitorId},
    payments::TransactionsManager,
    service::Service,
};
use grpcio::{ChannelBuilder, EnvBuilder};
use mc_account_keys::{AccountKey, PublicAddress, DEFAULT_SUBADDRESS_INDEX};
use mc_common::logger::{log, Logger};
use mc_connection::{Connection, ConnectionManager};
use mc_connection_test_utils::{test_client_uri, MockBlockchainConnection};
use mc_consensus_scp::QuorumSet;
use mc_crypto_keys::RistrettoPrivate;
use mc_crypto_rand::{CryptoRng, RngCore};
use mc_ledger_db::{Ledger, LedgerDB};
use mc_ledger_sync::PollingNetworkState;
use mc_mobilecoind_api::mobilecoind_api_grpc::MobilecoindApiClient;
use mc_transaction_core::{
    ring_signature::KeyImage, tx::TxOut, Block, BlockContents, BLOCK_VERSION,
};
use mc_util_from_random::FromRandom;
use mc_util_uri::ConnectionUri;
use mc_watcher::watcher_db::WatcherDB;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
};
use tempdir::TempDir;

/// The amount each recipient gets in the test ledger.
pub const PER_RECIPIENT_AMOUNT: u64 = 5_000 * 1_000_000_000_000;

/// Number of initial blocks generated by `get_testing_environment`;
pub const GET_TESTING_ENVIRONMENT_NUM_BLOCKS: usize = 10;

/// Sets up ledger_db and mobilecoind_db. Each block will contains one txo per recipient.
///
/// # Arguments
/// *
/// * `num_random_recipients` - Number of random recipients to create.
/// * `known_recipients` - A list of known recipients to create.
/// * `num_blocks` - Number of blocks to create in the ledger_db.
/// * `logger`
/// * `rng`
///
/// Note that all txos will be controlled by the subindex DEFAULT_SUBADDRESS_INDEX
pub fn get_test_databases(
    num_random_recipients: u32,
    known_recipients: &[PublicAddress],
    num_blocks: usize,
    logger: Logger,
    mut rng: &mut (impl CryptoRng + RngCore),
) -> (LedgerDB, Database) {
    let mut public_addresses: Vec<PublicAddress> = (0..num_random_recipients)
        .map(|_i| mc_account_keys::AccountKey::random(&mut rng).default_subaddress())
        .collect();

    public_addresses.extend(known_recipients.iter().cloned());

    // Note that TempDir manages uniqueness by constructing paths
    // like: /tmp/ledger_db.tvF0XHTKsilx
    let ledger_db_tmp = TempDir::new("ledger_db").expect("Could not make tempdir for ledger db");
    let ledger_db_path = ledger_db_tmp
        .path()
        .to_str()
        .expect("Could not get path as string");
    let mobilecoind_db_tmp =
        TempDir::new("mobilecoind_db").expect("Could not make tempdir for mobilecoind db");
    let mobilecoind_db_path = mobilecoind_db_tmp
        .path()
        .to_str()
        .expect("Could not get path as string");

    let mut ledger_db = generate_ledger_db(&ledger_db_path);

    for block_index in 0..num_blocks {
        let key_images = if block_index == 0 {
            vec![]
        } else {
            vec![KeyImage::from(rng.next_u64())]
        };
        let _new_block_height =
            add_block_to_ledger_db(&mut ledger_db, &public_addresses, &key_images, rng);
    }

    let mobilecoind_db = Database::new(mobilecoind_db_path.to_string(), logger)
        .expect("failed creating new mobilecoind db");

    (ledger_db, mobilecoind_db)
}

pub fn get_test_monitor_data_and_id(
    rng: &mut (impl CryptoRng + RngCore),
) -> (MonitorData, MonitorId) {
    let account_key = AccountKey::random(rng);

    let data = MonitorData::new(
        account_key,
        DEFAULT_SUBADDRESS_INDEX, // first_subaddress
        1,                        // num_subaddresses
        0,                        // first_block
        "",                       // name
    )
    .unwrap();

    let monitor_id = MonitorId::from(&data);
    (data, monitor_id)
}

/// Creates an empty LedgerDB.
///
/// # Arguments
/// * `path` - Path to the ledger's data.mdb file. If such a file exists, it will be replaced.
fn generate_ledger_db(path: &str) -> LedgerDB {
    // DELETE the old database if it already exists.
    let _ = std::fs::remove_file(format!("{}/data.mdb", path));
    LedgerDB::create(PathBuf::from(path)).expect("Could not create ledger_db");
    let db = LedgerDB::open(PathBuf::from(path)).expect("Could not open ledger_db");
    db
}

/// Adds a block containing one txo for each provided recipient and returns new block height.
///
/// # Arguments
/// * `ledger_db`
/// * `recipients` - Recipients of outputs.
/// * `rng`
pub fn add_block_to_ledger_db(
    ledger_db: &mut LedgerDB,
    recipients: &[PublicAddress],
    key_images: &[KeyImage],
    rng: &mut (impl CryptoRng + RngCore),
) -> u64 {
    // Each initial account gets 5k MOB, which are each 10^12 picoMOB.
    let value: u64 = PER_RECIPIENT_AMOUNT;

    let outputs: Vec<_> = recipients
        .iter()
        .map(|recipient| {
            TxOut::new(
                // TODO: allow for subaddress index!
                value,
                recipient,
                &RistrettoPrivate::from_random(rng),
                Default::default(),
                rng,
            )
            .unwrap()
        })
        .collect();

    let block_contents = BlockContents::new(key_images.to_vec(), outputs.clone());

    let num_blocks = ledger_db.num_blocks().expect("failed to get block height");

    let new_block;
    if num_blocks > 0 {
        let parent = ledger_db
            .get_block(num_blocks - 1)
            .expect("failed to get parent block");
        new_block =
            Block::new_with_parent(BLOCK_VERSION, &parent, &Default::default(), &block_contents);
    } else {
        new_block = Block::new_origin_block(&outputs);
    }

    ledger_db
        .append_block(&new_block, &block_contents, None)
        .expect("failed writing initial transactions");

    ledger_db.num_blocks().expect("failed to get block height")
}

/// Adds a block containing the given TXOs.
///
/// # Arguments
/// * `ledger_db`
/// * `outputs` - TXOs to add to ledger.
pub fn add_txos_to_ledger_db(
    ledger_db: &mut LedgerDB,
    outputs: &Vec<TxOut>,
    rng: &mut (impl CryptoRng + RngCore),
) -> u64 {
    let block_contents = BlockContents::new(vec![KeyImage::from(rng.next_u64())], outputs.clone());

    let num_blocks = ledger_db.num_blocks().expect("failed to get block height");

    let new_block;
    if num_blocks > 0 {
        let parent = ledger_db
            .get_block(num_blocks - 1)
            .expect("failed to get parent block");
        new_block =
            Block::new_with_parent(BLOCK_VERSION, &parent, &Default::default(), &block_contents);
    } else {
        new_block = Block::new_origin_block(&outputs);
    }

    ledger_db
        .append_block(&new_block, &block_contents, None)
        .expect("failed writing initial transactions");

    ledger_db.num_blocks().expect("failed to get block height")
}

fn get_free_port() -> u16 {
    static PORT_NR: AtomicUsize = AtomicUsize::new(0);
    PORT_NR.fetch_add(1, SeqCst) as u16 + 30100
}

fn setup_server(
    logger: Logger,
    ledger_db: LedgerDB,
    mobilecoind_db: Database,
    watcher_db: Option<WatcherDB>,
    test_port: u16,
) -> (
    Service,
    ConnectionManager<MockBlockchainConnection<LedgerDB>>,
) {
    let peer1 = MockBlockchainConnection::new(test_client_uri(1), ledger_db.clone(), 0);
    let peer2 = MockBlockchainConnection::new(test_client_uri(2), ledger_db.clone(), 0);

    let quorum_set = QuorumSet::new_with_node_ids(
        2,
        vec![
            peer1.uri().responder_id().unwrap(),
            peer2.uri().responder_id().unwrap(),
        ],
    );

    let conn_manager = ConnectionManager::new(vec![peer1, peer2], logger.clone());

    let network_state = Arc::new(Mutex::new(PollingNetworkState::new(
        quorum_set,
        conn_manager.clone(),
        logger.clone(),
    )));

    {
        let mut network_state = network_state.lock().unwrap();
        network_state.poll();
    }

    let transactions_manager = TransactionsManager::new(
        ledger_db.clone(),
        mobilecoind_db.clone(),
        conn_manager.clone(),
        logger.clone(),
    );

    let service = Service::new(
        ledger_db,
        mobilecoind_db,
        watcher_db,
        transactions_manager,
        network_state,
        test_port,
        None,
        logger,
    );

    (service, conn_manager)
}

fn setup_client(test_port: u16) -> MobilecoindApiClient {
    let address = format!("127.0.0.1:{}", test_port);
    let env = Arc::new(
        EnvBuilder::new()
            .name_prefix(format!("gRPC-{}", address))
            .build(),
    );
    let ch = ChannelBuilder::new(env).connect(&address);
    MobilecoindApiClient::new(ch)
}

/// Create a ready test environment.
/// Recipients can be randomly gernerated or passed in.
/// The ledger has GET_TESTING_ENVIRONMENT_NUM_BLOCKS blocks. Each block has one txo per recipient.
/// Monitors are created for each provided MonitorData.
/// The function delays return until all monitors have processed the entire ledger.
///
/// # Arguments
/// * `num_random_recipients` - random recipients to add
/// * `recipients` - particular recipient public addresses to add
/// * `monitors` - MonitorData objects specifying monitors to add
/// * `logger`
/// * `rng`

pub fn get_testing_environment(
    num_random_recipients: u32,
    recipients: &[PublicAddress],
    monitors: &[MonitorData],
    logger: Logger,
    mut rng: &mut (impl CryptoRng + RngCore),
) -> (
    LedgerDB,
    Database,
    MobilecoindApiClient,
    Service,
    ConnectionManager<MockBlockchainConnection<LedgerDB>>,
) {
    let (ledger_db, mobilecoind_db) = get_test_databases(
        num_random_recipients,
        recipients,
        GET_TESTING_ENVIRONMENT_NUM_BLOCKS,
        logger.clone(),
        &mut rng,
    );
    let port = get_free_port();
    log::debug!(logger, "Setting up server {:?}", port);
    let (server, server_conn_manager) = setup_server(
        logger.clone(),
        ledger_db.clone(),
        mobilecoind_db.clone(),
        None,
        port,
    );
    log::debug!(logger, "Setting up client {:?}", port);
    let client = setup_client(port);

    for data in monitors {
        mobilecoind_db
            .add_monitor(&data)
            .expect("failed adding monitor");
    }

    wait_for_monitors(&mobilecoind_db, &ledger_db, &logger);
    (
        ledger_db,
        mobilecoind_db,
        client,
        server,
        server_conn_manager,
    )
}

/// Waits until all monitors are current with the last block of the ledger DB
///
/// # Arguments
/// * `mobilecoind_db` - Database instance
/// * `ledger_db` - LedgerDB instance
/// * `logger`
pub fn wait_for_monitors(mobilecoind_db: &Database, ledger_db: &LedgerDB, logger: &Logger) {
    let num_blocks = ledger_db.num_blocks().unwrap();

    let mut monitor_map_len: usize;
    std::thread::sleep(std::time::Duration::from_secs(1));

    'outer: loop {
        let monitor_map = mobilecoind_db
            .get_monitor_map()
            .expect("failed getting monitor map");

        monitor_map_len = monitor_map.len();

        for (i, (_monitor_id, data)) in monitor_map.iter().enumerate() {
            if data.next_block < num_blocks {
                log::info!(
                    logger,
                    "waiting for monitor {}/{}: {} of {} blocks processed",
                    i + 1, // display ordinal rather than index
                    monitor_map_len,
                    data.next_block,
                    num_blocks
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue 'outer;
            }
        }
        break;
    }

    if monitor_map_len > 0 {
        let plurality_char = if monitor_map_len == 1 { "" } else { "s" };
        log::info!(
            logger,
            "waited for {} monitor{} to finish processing {} blocks",
            monitor_map_len,
            plurality_char,
            num_blocks
        );
    }
}
