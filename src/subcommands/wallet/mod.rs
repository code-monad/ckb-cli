mod index;

use std::collections::HashMap;
use std::path::PathBuf;

use clap::{App, Arg, ArgMatches};
use serde::{Deserialize, Serialize};

use ckb_chain_spec::consensus::TYPE_ID_CODE_HASH;
use ckb_hash::new_blake2b;
use ckb_index::{IndexDatabase, LiveCellInfo};
use ckb_jsonrpc_types as json_types;
use ckb_sdk::{
    bip32::DerivationPath,
    constants::{DAO_TYPE_HASH, MULTISIG_TYPE_HASH, SIGHASH_TYPE_HASH},
    rpc::CkbRpcClient,
    traits::{
        DefaultCellDepResolver, DefaultHeaderDepResolver, DefaultTransactionDependencyProvider,
        Signer,
    },
    tx_builder::{
        transfer::CapacityTransferBuilder, unlock_tx, CapacityBalancer, CapacityProvider, TxBuilder,
    },
    types::ScriptId,
    unlock::{
        MultisigConfig, ScriptUnlocker, SecpMultisigScriptSigner, SecpMultisigUnlocker,
        SecpSighashScriptSigner, SecpSighashUnlocker,
    },
    Address, AddressPayload, GenesisInfo, HumanCapacity, Since, SinceType, SECP256K1,
};
use ckb_types::{
    bytes::Bytes,
    core::{BlockView, Capacity, FeeRate, ScriptHashType, TransactionView},
    packed::{Byte32, CellOutput, Script, WitnessArgs},
    prelude::*,
    H160, H256,
};

use super::{CliSubCommand, Output};
use crate::plugin::PluginManager;
use crate::utils::{
    arg,
    arg_parser::{
        AddressParser, ArgParser, CapacityParser, FixedHashParser, FromStrParser,
        PrivkeyPathParser, PrivkeyWrapper,
    },
    cell_collector::LocalCellCollector,
    index::{with_db, IndexController},
    other::{
        check_capacity, get_address, get_arg_value, get_max_mature_number, get_network_type,
        get_to_data, is_mature, read_password,
    },
    rpc::HttpRpcClient,
    signer::KeyStoreHandlerSigner,
};
pub use index::start_index_thread;

// Max derived change address to search
const DERIVE_CHANGE_ADDRESS_MAX_LEN: u32 = 10000;

pub struct WalletSubCommand<'a> {
    plugin_mgr: &'a mut PluginManager,
    rpc_client: &'a mut HttpRpcClient,
    genesis_info: Option<GenesisInfo>,
    index_dir: PathBuf,
    index_controller: IndexController,
    wait_for_sync: bool,
}

impl<'a> WalletSubCommand<'a> {
    pub fn new(
        rpc_client: &'a mut HttpRpcClient,
        plugin_mgr: &'a mut PluginManager,
        genesis_info: Option<GenesisInfo>,
        index_dir: PathBuf,
        index_controller: IndexController,
        wait_for_sync: bool,
    ) -> WalletSubCommand<'a> {
        WalletSubCommand {
            rpc_client,
            plugin_mgr,
            genesis_info,
            index_dir,
            index_controller,
            wait_for_sync,
        }
    }

    fn genesis_info(&mut self) -> Result<GenesisInfo, String> {
        if self.genesis_info.is_none() {
            let genesis_block: BlockView = self
                .rpc_client
                .get_block_by_number(0)?
                .expect("Can not get genesis block?")
                .into();
            self.genesis_info =
                Some(GenesisInfo::from_block(&genesis_block).map_err(|err| err.to_string())?);
        }
        Ok(self.genesis_info.clone().unwrap())
    }

    fn with_db<F, T>(&mut self, func: F) -> Result<T, String>
    where
        F: FnOnce(IndexDatabase) -> T,
    {
        let genesis_header = self.genesis_info()?.header().clone();
        with_db(
            func,
            self.rpc_client,
            genesis_header,
            &self.index_dir,
            self.index_controller.clone(),
            self.wait_for_sync,
        )
    }

    pub fn subcommand() -> App<'static> {
        App::new("wallet")
            .about("Transfer / query balance (with local index) / key utils")
            .subcommands(vec![
                App::new("transfer")
                    .about("Transfer capacity to an address (can have data)")
                    .arg(arg::privkey_path().required_unless(arg::from_account().get_name()))
                    .arg(
                        arg::from_account()
                            .required_unless(arg::privkey_path().get_name())
                            .conflicts_with(arg::privkey_path().get_name()),
                    )
                    .arg(arg::from_locked_address())
                    .arg(arg::to_address().required(true))
                    .arg(arg::to_data())
                    .arg(arg::to_data_path())
                    .arg(arg::capacity().required(true))
                    .arg(arg::fee_rate())
                    .arg(arg::derive_receiving_address_length())
                    .arg(
                        arg::derive_change_address().conflicts_with(arg::privkey_path().get_name()),
                    )
                    .arg(
                        Arg::with_name("skip-check-to-address")
                            .long("skip-check-to-address")
                            .about("Skip check <to-address> (default only allow sighash/multisig address), be cautious to use this flag"))
                    .arg(
                        Arg::with_name("type-id")
                            .long("type-id")
                            .about("Add type id type script to target output cell"),
                    ),
                App::new("get-capacity")
                    .about("Get capacity by lock script hash or address or lock arg or pubkey")
                    .arg(arg::lock_hash())
                    .arg(arg::address())
                    .arg(arg::pubkey())
                    .arg(arg::lock_arg())
                    .arg(arg::derive_receiving_address_length())
                    .arg(arg::derive_change_address_length())
                    .arg(arg::derived().conflicts_with(arg::lock_hash().get_name())),
                App::new("get-live-cells")
                    .about("Get live cells by lock/type/code  hash")
                    .arg(arg::lock_hash())
                    .arg(arg::type_hash())
                    .arg(arg::code_hash())
                    .arg(arg::address())
                    .arg(arg::live_cells_limit())
                    .arg(arg::from_block_number())
                    .arg(arg::to_block_number())
                    .arg(
                        Arg::with_name("fast-mode")
                            .long("fast-mode")
                            .about("Only visit current range (by --from and --to) of live cells"),
                    ),
                App::new("top-capacity")
                    .about("Show top n capacity owned by lock script hash")
                    .arg(arg::top_n()),
            ])
    }

    pub fn transfer(
        &mut self,
        args: TransferArgs,
        skip_check: bool,
    ) -> Result<TransactionView, String> {
        let TransferArgs {
            privkey_path,
            from_account,
            from_locked_address,
            password,
            derive_receiving_address_length,
            derive_change_address,
            capacity,
            fee_rate,
            to_address,
            to_data,
            is_type_id,
            skip_check_to_address,
        } = args;

        let network_type = get_network_type(self.rpc_client)?;
        let from_privkey: Option<PrivkeyWrapper> = privkey_path
            .map(|input| PrivkeyPathParser.parse(&input))
            .transpose()?;
        let from_account: Option<H160> = from_account
            .map(|input| {
                FixedHashParser::<H160>::default()
                    .parse(&input)
                    .or_else(|err| {
                        let result: Result<Address, String> = AddressParser::new_sighash()
                            .set_network(network_type)
                            .parse(&input);
                        result
                            .map(|address| H160::from_slice(&address.payload().args()).unwrap())
                            .map_err(|_| err)
                    })
            })
            .transpose()?;
        let from_locked_address: Option<Address> = from_locked_address
            .map(|input| {
                AddressParser::default()
                    .set_network(network_type)
                    .set_full_type(MULTISIG_TYPE_HASH.clone())
                    .parse(&input)
            })
            .transpose()?;
        let to_capacity: u64 = CapacityParser.parse(&capacity)?.into();
        let fee_rate: u64 = FromStrParser::<u64>::default().parse(&fee_rate)?;
        let receiving_address_length: u32 = derive_receiving_address_length
            .map(|input| FromStrParser::<u32>::default().parse(&input))
            .transpose()?
            .unwrap_or(1000);
        let last_change_address_opt: Option<Address> = derive_change_address
            .map(|input| {
                AddressParser::default()
                    .set_network(network_type)
                    .parse(&input)
            })
            .transpose()?;
        let to_address: Address = AddressParser::default()
            .set_network(network_type)
            .parse(&to_address)?;
        let to_data = to_data.unwrap_or_default();

        let (from_address_payload, password) = if let Some(from_privkey) = from_privkey.as_ref() {
            let from_pubkey = secp256k1::PublicKey::from_secret_key(&SECP256K1, from_privkey);
            (AddressPayload::from_pubkey(&from_pubkey), None)
        } else {
            let password = if let Some(password) = password {
                Some(password)
            } else if self.plugin_mgr.keystore_require_password() {
                Some(read_password(false, None)?)
            } else {
                None
            };
            (
                AddressPayload::from_pubkey_hash(from_account.unwrap()),
                password,
            )
        };
        let from_address = Address::new(network_type, from_address_payload.clone(), false);

        if let Some(from_locked_address) = from_locked_address.as_ref() {
            let args = from_locked_address.payload().args();
            let err_prefix = "Invalid from-locked-address's args";
            if args.len() != 28 {
                return Err(format!("{}: invalid {}", err_prefix, args.len()));
            }
            let mut since_bytes = [0u8; 8];
            since_bytes.copy_from_slice(&args[20..]);
            let since = Since::from_raw_value(u64::from_le_bytes(since_bytes));
            if !since.flags_is_valid() {
                return Err(format!("{}: invalid since flags", err_prefix));
            }
            if !since.is_absolute() {
                return Err(format!("{}: only support absolute since value", err_prefix));
            }
            if since.extract_metric().map(|(ty, _)| ty) != Some(SinceType::EpochNumberWithFraction)
            {
                return Err(format!("{}: only support epoch since value", err_prefix));
            }
        }

        let to_address_hash_type = to_address.payload().hash_type();
        let to_address_code_hash: H256 = to_address.payload().code_hash().unpack();
        let to_address_args_len = to_address.payload().args().len();
        if !skip_check_to_address
            && !(to_address_hash_type == ScriptHashType::Type
                && to_address_code_hash == SIGHASH_TYPE_HASH
                && to_address_args_len == 20)
            && !(to_address_hash_type == ScriptHashType::Type
                && to_address_code_hash == MULTISIG_TYPE_HASH
                && (to_address_args_len == 20 || to_address_args_len == 28))
        {
            return Err(format!("Invalid to-address: {}\n[Hint]: Add `--skip-check-to-address` flag to transfer to any address", to_address));
        }
        check_capacity(to_capacity, to_data.len())?;

        let genesis_info = self.genesis_info()?;

        // The lock scripts for search live cells
        let sighash_placeholder_witness = WitnessArgs::new_builder()
            .lock(Some(Bytes::from(vec![0u8; 65])).pack())
            .build()
            .as_bytes();
        let mut lock_scripts = vec![(
            Script::from(&from_address_payload),
            sighash_placeholder_witness.clone(),
        )];

        let from_lock_arg = H160::from_slice(from_address.payload().args().as_ref()).unwrap();
        let mut path_map: HashMap<H160, DerivationPath> = Default::default();
        let (change_address_payload, change_path) = if let Some(last_change_address) =
            last_change_address_opt.as_ref()
        {
            // Behave like HD wallet
            let change_last =
                H160::from_slice(last_change_address.payload().args().as_ref()).unwrap();
            let key_set = self.plugin_mgr.keystore_handler().derived_key_set(
                from_lock_arg.clone(),
                receiving_address_length,
                change_last.clone(),
                DERIVE_CHANGE_ADDRESS_MAX_LEN,
                None,
            )?;
            let mut change_path_opt = None;
            for (path, hash160) in key_set.external.iter().chain(key_set.change.iter()) {
                if hash160 == &change_last {
                    change_path_opt = Some(path.clone());
                }
                path_map.insert(hash160.clone(), path.clone());
                let payload = AddressPayload::from_pubkey_hash(hash160.clone());
                lock_scripts.push((Script::from(&payload), sighash_placeholder_witness.clone()));
            }
            (
                last_change_address.payload().clone(),
                change_path_opt.expect("change path not exists"),
            )
        } else {
            (
                from_address.payload().clone(),
                self.plugin_mgr.root_key_path(from_lock_arg.clone())?,
            )
        };

        let get_signer = || -> Result<Box<dyn Signer>, String> {
            if let Some(privkey) = from_privkey.as_ref() {
                Ok(Box::new(privkey.clone()))
            } else {
                let mut signer = KeyStoreHandlerSigner::new(
                    self.plugin_mgr.keystore_handler(),
                    Box::new(DefaultTransactionDependencyProvider::new(
                        self.rpc_client.url(),
                        0,
                    )),
                );
                if let Some(password) = password.as_ref() {
                    signer.set_password(from_lock_arg.clone(), password.clone());
                }
                if let Some(last_change_address) = last_change_address_opt.as_ref() {
                    let change_last =
                        H160::from_slice(last_change_address.payload().args().as_ref()).unwrap();
                    signer.cache_key_set(
                        from_lock_arg.clone(),
                        receiving_address_length,
                        change_last,
                        DERIVE_CHANGE_ADDRESS_MAX_LEN,
                    )?;
                }
                signer.set_change_path(from_lock_arg.clone(), change_path.to_string());
                Ok(Box::new(signer))
            }
        };
        let mut unlockers: HashMap<_, Box<dyn ScriptUnlocker>> = HashMap::new();
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let sighash_unlocker = {
            let signer = get_signer()?;
            SecpSighashUnlocker::new(SecpSighashScriptSigner::new(signer))
        };
        unlockers.insert(sighash_script_id, Box::new(sighash_unlocker));

        if let Some(from_locked_address) = from_locked_address.as_ref() {
            let mut found_lock_arg = false;
            for lock_arg in std::iter::once(&from_lock_arg).chain(path_map.keys()) {
                let mut sighash_addresses = Vec::default();
                sighash_addresses.push(lock_arg.clone());
                let require_first_n = 0;
                let threshold = 1;
                let config =
                    MultisigConfig::new_with(sighash_addresses, require_first_n, threshold)
                        .map_err(|err| err.to_string())?;
                if config.hash160().as_bytes() == &from_locked_address.payload().args()[0..20] {
                    found_lock_arg = true;
                    let config_data = config.to_witness_data();
                    let mut zero_lock =
                        vec![0u8; config_data.len() + 65 * (config.threshold() as usize)];
                    zero_lock[0..config_data.len()].copy_from_slice(&config_data);
                    let lock_script = Script::from(from_locked_address.payload());
                    let placehodler_witness = WitnessArgs::new_builder()
                        .lock(Some(Bytes::from(zero_lock)).pack())
                        .build()
                        .as_bytes();
                    lock_scripts.insert(0, (lock_script, placehodler_witness));
                    let multisig_script_id = ScriptId::new_type(MULTISIG_TYPE_HASH.clone());
                    let multisig_unlocker = {
                        let signer = get_signer()?;
                        SecpMultisigUnlocker::new(SecpMultisigScriptSigner::new(signer, config))
                    };
                    unlockers.insert(multisig_script_id, Box::new(multisig_unlocker));
                    break;
                }
            }
            if !found_lock_arg {
                return Err(String::from(
                    "from-locked-address is not created from the key or derived keys",
                ));
            }
        }

        let balancer = CapacityBalancer {
            fee_rate: FeeRate::from_u64(fee_rate),
            change_lock_script: Some(Script::from(&change_address_payload)),
            capacity_provider: CapacityProvider::new(lock_scripts),
            force_small_change_as_fee: None,
        };
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(self.rpc_client.url(), 10);
        let mut cell_collector = LocalCellCollector::new(
            self.index_dir.clone(),
            self.index_controller.clone(),
            HttpRpcClient::new(self.rpc_client.url().to_string()),
            Some(genesis_info.header().clone()),
            self.wait_for_sync,
        );
        let cell_dep_resolver = DefaultCellDepResolver::new(&genesis_info);
        let header_dep_resolver =
            DefaultHeaderDepResolver::new(CkbRpcClient::new(self.rpc_client.url()));

        // Add outputs
        let placeholder_type_script = if is_type_id {
            Some(
                Script::new_builder()
                    .code_hash(TYPE_ID_CODE_HASH.pack())
                    .hash_type(ScriptHashType::Type.into())
                    .args(Bytes::from(vec![0u8; 32]).pack())
                    .build(),
            )
        } else {
            None
        };
        let to_output = CellOutput::new_builder()
            .capacity(Capacity::shannons(to_capacity).pack())
            .lock(to_address.payload().into())
            .type_(placeholder_type_script.pack())
            .build();
        let builder = CapacityTransferBuilder::new(vec![(to_output, to_data)]);
        let mut tx = builder
            .build_balanced(
                &mut cell_collector,
                &cell_dep_resolver,
                &header_dep_resolver,
                &tx_dep_provider,
                &balancer,
                &unlockers,
            )
            .map_err(|err| err.to_string())?;
        if is_type_id {
            let mut blake2b = new_blake2b();
            let first_cell_input = tx.inputs().into_iter().next().expect("inputs empty");
            blake2b.update(first_cell_input.as_slice());
            blake2b.update(&0u64.to_le_bytes());
            let mut ret = [0; 32];
            blake2b.finalize(&mut ret);
            let type_script = Script::new_builder()
                .code_hash(TYPE_ID_CODE_HASH.pack())
                .hash_type(ScriptHashType::Type.into())
                .args(Bytes::from(ret.to_vec()).pack())
                .build();
            let mut outputs = tx.outputs().into_iter().collect::<Vec<_>>();
            outputs[0] = tx
                .output(0)
                .expect("first output")
                .as_builder()
                .type_(Some(type_script).pack())
                .build();
            tx = tx.as_advanced_builder().set_outputs(outputs).build();
        }
        let (tx, still_locked_groups) =
            unlock_tx(tx, &tx_dep_provider, &unlockers).map_err(|err| err.to_string())?;
        assert!(still_locked_groups.is_empty());

        let outputs_validator = if is_type_id || skip_check || skip_check_to_address {
            Some(json_types::OutputsValidator::Passthrough)
        } else {
            None
        };

        let tx_hash = self
            .rpc_client
            .send_transaction(tx.data(), outputs_validator)
            .map_err(|err| format!("Send transaction error: {}", err))?;
        assert_eq!(tx.hash(), tx_hash.pack());
        Ok(tx)
    }

    pub fn get_capacity(&mut self, lock_hashes: Vec<Byte32>) -> Result<(u64, u64, u64), String> {
        let max_mature_number = get_max_mature_number(self.rpc_client)?;
        self.with_db(|db| {
            let mut total_capacity = 0;
            let mut dao_capacity = 0;
            let mut immature_capacity = 0;
            let mut terminator = |_idx: usize, info: &LiveCellInfo| {
                if !is_mature(info, max_mature_number) {
                    immature_capacity += info.capacity;
                }
                if info
                    .type_hashes
                    .as_ref()
                    .filter(|(code_hash, _)| code_hash == &DAO_TYPE_HASH)
                    .is_some()
                {
                    dao_capacity += info.capacity;
                }
                total_capacity += info.capacity;
                (false, false)
            };
            for lock_hash in lock_hashes {
                let _ = db.get_live_cells_by_lock(lock_hash, None, &mut terminator);
            }
            (total_capacity, immature_capacity, dao_capacity)
        })
    }

    pub fn get_live_cells<F>(
        &mut self,
        to_number: u64,
        limit: usize,
        mut func: F,
        fast_mode: bool,
    ) -> Result<(LiveCells, Option<(u32, u64)>), String>
    where
        F: FnMut(
            &IndexDatabase,
            &mut dyn FnMut(usize, &LiveCellInfo) -> (bool, bool),
        ) -> Vec<LiveCellInfo>,
    {
        let (infos, total_count, total_capacity, current_count, current_capacity) =
            self.with_db(|db| {
                let mut total_count: u32 = 0;
                let mut total_capacity: u64 = 0;
                let mut current_count: u32 = 0;
                let mut current_capacity: u64 = 0;
                let mut terminator = |idx, info: &LiveCellInfo| {
                    let stop = idx >= limit || info.number > to_number;
                    let push_info = !stop;
                    total_count += 1;
                    total_capacity += info.capacity;
                    if push_info {
                        current_count += 1;
                        current_capacity += info.capacity;
                    }
                    (fast_mode && stop, push_info)
                };
                let infos = func(&db, &mut terminator);
                (
                    infos,
                    total_count,
                    total_capacity,
                    current_count,
                    current_capacity,
                )
            })?;

        let max_mature_number = get_max_mature_number(self.rpc_client)?;
        let live_cells = infos
            .into_iter()
            .map(|info| {
                let mature = is_mature(&info, max_mature_number);
                LiveCell { info, mature }
            })
            .collect::<Vec<_>>();
        let total = if fast_mode {
            None
        } else {
            Some((total_count, total_capacity))
        };
        Ok((
            LiveCells {
                live_cells,
                current_count,
                current_capacity,
            },
            total,
        ))
    }
}

impl<'a> CliSubCommand for WalletSubCommand<'a> {
    fn process(&mut self, matches: &ArgMatches, debug: bool) -> Result<Output, String> {
        match matches.subcommand() {
            ("transfer", Some(m)) => {
                let to_data = get_to_data(m)?;
                let args = TransferArgs {
                    privkey_path: m.value_of("privkey-path").map(|s| s.to_string()),
                    from_account: m.value_of("from-account").map(|s| s.to_string()),
                    from_locked_address: m.value_of("from-locked-address").map(|s| s.to_string()),
                    password: None,
                    capacity: get_arg_value(m, "capacity")?,
                    fee_rate: get_arg_value(m, "fee-rate")?,
                    derive_receiving_address_length: Some(get_arg_value(
                        m,
                        "derive-receiving-address-length",
                    )?),
                    derive_change_address: m
                        .value_of("derive-change-address")
                        .map(|s| s.to_string()),
                    to_address: get_arg_value(m, "to-address")?,
                    to_data: Some(to_data),
                    is_type_id: m.is_present("type-id"),
                    skip_check_to_address: m.is_present("skip-check-to-address"),
                };
                let tx = self.transfer(args, false)?;
                if debug {
                    let rpc_tx_view = json_types::TransactionView::from(tx);
                    Ok(Output::new_output(rpc_tx_view))
                } else {
                    let tx_hash: H256 = tx.hash().unpack();
                    Ok(Output::new_output(tx_hash))
                }
            }
            ("get-capacity", Some(m)) => {
                let lock_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "lock-hash", false)?;
                let lock_hashes = if let Some(lock_hash) = lock_hash_opt {
                    vec![lock_hash.pack()]
                } else {
                    let network_type = get_network_type(self.rpc_client)?;

                    let receiving_address_length: u32 = FromStrParser::<u32>::default()
                        .from_matches(m, "derive-receiving-address-length")?;
                    let change_address_length: u32 = FromStrParser::<u32>::default()
                        .from_matches(m, "derive-change-address-length")?;
                    let address_payload = if let Some(address_str) = m.value_of("address") {
                        AddressParser::default()
                            .set_network(network_type)
                            .parse(address_str)?
                            .payload()
                            .clone()
                    } else {
                        get_address(Some(network_type), m)?
                    };
                    let mut lock_hashes = vec![Script::from(&address_payload).calc_script_hash()];
                    if m.is_present("derived") {
                        let lock_arg = H160::from_slice(address_payload.args().as_ref()).unwrap();

                        let key_set = self
                            .plugin_mgr
                            .keystore_handler()
                            .derived_key_set_by_index(
                                lock_arg,
                                0,
                                receiving_address_length,
                                0,
                                change_address_length,
                                None,
                            )?;
                        for (_, hash160) in key_set.external.iter().chain(key_set.change.iter()) {
                            let payload = AddressPayload::from_pubkey_hash(hash160.clone());
                            lock_hashes.push(Script::from(&payload).calc_script_hash());
                        }
                    }
                    lock_hashes
                };

                let (total, immature, dao) = self.get_capacity(lock_hashes)?;

                let mut resp =
                    serde_json::json!({ "total": format!("{:#}", HumanCapacity::from(total)) });
                if immature > 0 {
                    resp["immature"] =
                        serde_json::json!(format!("{:#}", HumanCapacity::from(immature)));
                }
                if dao > 0 {
                    let free = total - dao;
                    resp["dao"] = serde_json::json!(format!("{:#}", HumanCapacity::from(dao)));
                    resp["free"] = serde_json::json!(format!("{:#}", HumanCapacity::from(free)));
                }
                Ok(Output::new_output(resp))
            }
            ("get-live-cells", Some(m)) => {
                let lock_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "lock-hash", false)?;
                let type_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "type-hash", false)?;
                let code_hash_opt: Option<H256> =
                    FixedHashParser::<H256>::default().from_matches_opt(m, "code-hash", false)?;
                let limit: usize = FromStrParser::<usize>::default().from_matches(m, "limit")?;
                let from_number_opt: Option<u64> =
                    FromStrParser::<u64>::default().from_matches_opt(m, "from", false)?;
                let to_number_opt: Option<u64> =
                    FromStrParser::<u64>::default().from_matches_opt(m, "to", false)?;
                let fast_mode = m.is_present("fast-mode");

                let network_type = get_network_type(self.rpc_client)?;
                let lock_hash_opt = if lock_hash_opt.is_none() {
                    let address_opt: Option<Address> = AddressParser::default()
                        .set_network_opt(Some(network_type))
                        .from_matches_opt(m, "address", false)?;
                    address_opt
                        .map(|address| Script::from(address.payload()).calc_script_hash().unpack())
                } else {
                    lock_hash_opt
                };

                if lock_hash_opt.is_none() && type_hash_opt.is_none() && code_hash_opt.is_none() {
                    return Err(
                        "lock-hash or type-hash or code-hash or address is required".to_owned()
                    );
                }

                let to_number = to_number_opt.unwrap_or(std::u64::MAX);
                let (
                    LiveCells {
                        live_cells,
                        current_count,
                        current_capacity,
                    },
                    total,
                ) = self.get_live_cells(
                    to_number,
                    limit,
                    |db, terminator| {
                        if let Some(lock_hash) = &lock_hash_opt {
                            db.get_live_cells_by_lock(
                                lock_hash.clone().pack(),
                                from_number_opt,
                                terminator,
                            )
                        } else if let Some(type_hash) = &type_hash_opt {
                            db.get_live_cells_by_type(
                                type_hash.clone().pack(),
                                from_number_opt,
                                terminator,
                            )
                        } else {
                            db.get_live_cells_by_code(
                                code_hash_opt.clone().unwrap().pack(),
                                from_number_opt,
                                terminator,
                            )
                        }
                    },
                    fast_mode,
                )?;
                let mut resp = serde_json::json!({
                    "live_cells": live_cells.into_iter().map(|live_cell| {
                        let LiveCell{ info, mature } = live_cell;
                        let mut value = serde_json::to_value(&info).unwrap();
                        let mature = serde_json::Value::Bool(mature);
                        let capacity_string = serde_json::Value::String(format!("{:#}", HumanCapacity::from(info.capacity)));
                        let map = value.as_object_mut().unwrap();
                        map.insert("capacity".to_string(), capacity_string);
                        map.insert("mature".to_string(), mature);
                        value
                    }).collect::<Vec<_>>(),
                    "current_count": current_count,
                    "current_capacity": format!("{:#}", HumanCapacity::from(current_capacity)),
                });
                if let Some((total_count, total_capacity)) = total {
                    resp["total_count"] = serde_json::json!(total_count);
                    resp["total_capacity"] =
                        serde_json::json!(format!("{:#}", HumanCapacity::from(total_capacity)));
                }

                Ok(Output::new_output(resp))
            }
            ("top-capacity", Some(m)) => {
                let n: usize = m
                    .value_of("number")
                    .map(|n_str| n_str.parse().unwrap())
                    .unwrap();
                let network_type = get_network_type(self.rpc_client)?;
                let resp = self.with_db(|db| {
                    db.get_top_n(n)
                        .into_iter()
                        .map(|(lock_hash, payload_opt, capacity)| {
                            serde_json::json!({
                                "lock_hash": format!("{:#x}", lock_hash),
                                "address": payload_opt.map(|payload| Address::new(network_type, payload, true).to_string()),
                                "capacity": format!("{:#}", HumanCapacity::from(capacity)),
                            })
                        })
                        .collect::<Vec<_>>()
                })?;
                Ok(Output::new_output(resp))
            }
            _ => Err(Self::subcommand().generate_usage()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransferArgs {
    pub privkey_path: Option<String>,
    pub from_account: Option<String>,
    pub from_locked_address: Option<String>,
    pub password: Option<String>,
    pub derive_receiving_address_length: Option<String>,
    pub derive_change_address: Option<String>,
    pub capacity: String,
    pub fee_rate: String,
    pub to_address: String,
    pub to_data: Option<Bytes>,
    pub is_type_id: bool,
    pub skip_check_to_address: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveCells {
    pub live_cells: Vec<LiveCell>,
    pub current_count: u32,
    pub current_capacity: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveCell {
    pub info: LiveCellInfo,
    pub mature: bool,
}
