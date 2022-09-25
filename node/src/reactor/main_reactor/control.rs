use datasize::DataSize;
use log::debug;
use std::{sync::Arc, time::Duration};
use tracing::{info, warn};

use crate::{
    components::{
        blocks_accumulator::{StartingWith, SyncInstruction},
        consensus::EraReport,
        contract_runtime::ExecutionPreState,
        diagnostics_port, event_stream_server, rest_server, rpc_server, small_network,
        storage::FatalStorageError,
        sync_leaper, upgrade_watcher,
    },
    effect::{EffectBuilder, EffectExt, Effects},
    reactor::main_reactor::{
        utils::{enqueue_shutdown, initialize_component},
        MainEvent, MainReactor,
    },
    types::{BlockHash, BlockHeader, BlockPayload, FinalizedBlock},
    NodeRng,
};

use casper_execution_engine::core::{
    engine_state,
    engine_state::{GenesisSuccess, UpgradeSuccess},
};

use crate::types::{ActivationPoint, ChainspecRawBytes, Item};
use casper_hashing::Digest;
use casper_types::{EraId, PublicKey, Timestamp};

// put in the config
pub(crate) const WAIT_SEC: u64 = 5;

#[derive(DataSize, Debug)]
pub(crate) enum ReactorState {
    // get all components and reactor state set up on start
    Initialize,
    // orient to the network and attempt to catch up to tip
    CatchUp,
    // stay caught up with tip
    KeepUp,
    // node is currently caught up and is an active validator
    Validate,
}

enum CatchUpInstruction {
    Do(Effects<MainEvent>),
    CheckSoon(String),
    CheckLater(String, u64),
    Shutdown(String),
    CaughtUp,
    CommitGenesis,
    CommitUpgrade(Box<BlockHeader>),
}

enum KeepUpInstruction {}

impl MainReactor {
    pub(crate) fn crank(
        &mut self,
        effect_builder: EffectBuilder<MainEvent>,
        rng: &mut NodeRng,
    ) -> Effects<MainEvent> {
        match self.state {
            ReactorState::Initialize => {
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.diagnostics_port,
                    "diagnostics",
                    MainEvent::DiagnosticsPort(diagnostics_port::Event::Initialize),
                ) {
                    return effects;
                }
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.upgrade_watcher,
                    "upgrade_watcher",
                    MainEvent::UpgradeWatcher(upgrade_watcher::Event::Initialize),
                ) {
                    return effects;
                }
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.small_network,
                    "small_network",
                    MainEvent::Network(small_network::Event::Initialize),
                ) {
                    return effects;
                }
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.event_stream_server,
                    "event_stream_server",
                    MainEvent::EventStreamServer(event_stream_server::Event::Initialize),
                ) {
                    return effects;
                }
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.rest_server,
                    "rest_server",
                    MainEvent::RestServer(rest_server::Event::Initialize),
                ) {
                    return effects;
                }
                if let Some(effects) = initialize_component(
                    effect_builder,
                    &mut self.rpc_server,
                    "rpc_server",
                    MainEvent::RpcServer(rpc_server::Event::Initialize),
                ) {
                    return effects;
                }
                self.state = ReactorState::CatchUp;
                return effect_builder
                    .immediately()
                    .event(|_| MainEvent::ReactorCrank);
            }
            ReactorState::CatchUp => match self.catch_up_instructions(rng, effect_builder) {
                CatchUpInstruction::CommitGenesis => {
                    let mut ret = Effects::new();
                    match self.commit_genesis(effect_builder) {
                        Ok(effects) => {
                            ret.extend(effects);
                            ret.extend(
                                effect_builder
                                    .set_timeout(Duration::from_secs(WAIT_SEC))
                                    .event(|_| MainEvent::ReactorCrank),
                            );
                        }
                        Err(msg) => {
                            ret.extend(
                                effect_builder
                                    .immediately()
                                    .event(move |_| MainEvent::Shutdown(msg)),
                            );
                        }
                    }
                    return ret;
                }
                CatchUpInstruction::CommitUpgrade(block_header) => {
                    let mut ret = Effects::new();
                    match self.commit_upgrade(effect_builder, block_header) {
                        Ok(effects) => {
                            ret.extend(effects);
                            ret.extend(
                                effect_builder
                                    .set_timeout(Duration::from_secs(WAIT_SEC))
                                    .event(|_| MainEvent::ReactorCrank),
                            );
                        }
                        Err(msg) => {
                            ret.extend(
                                effect_builder
                                    .immediately()
                                    .event(move |_| MainEvent::Shutdown(msg)),
                            );
                        }
                    }
                    return ret;
                }
                CatchUpInstruction::Do(effects) => {
                    let mut ret = Effects::new();
                    ret.extend(effects);
                    ret.extend(
                        effect_builder
                            .set_timeout(Duration::from_secs(WAIT_SEC))
                            .event(|_| MainEvent::ReactorCrank),
                    );
                    return ret;
                }
                CatchUpInstruction::CheckLater(_, wait) => {
                    return effect_builder
                        .set_timeout(Duration::from_secs(wait))
                        .event(|_| MainEvent::ReactorCrank);
                }
                CatchUpInstruction::CheckSoon(_) => {
                    return effect_builder
                        .immediately()
                        .event(|_| MainEvent::ReactorCrank);
                }
                CatchUpInstruction::Shutdown(msg) => {
                    return effect_builder
                        .immediately()
                        .event(move |_| MainEvent::Shutdown(msg))
                }
                CatchUpInstruction::CaughtUp => {
                    self.state = ReactorState::KeepUp;
                    return effect_builder
                        .immediately()
                        .event(|_| MainEvent::ReactorCrank);
                }
            },
            ReactorState::KeepUp => {
                // TODO: if UpgradeWatcher announcement raised, keep track of era id's against
                // the new activation points detected upgrade to make this a stronger check

                // TODO: if sync to genesis == true, determine if cycles
                // are available and if so, queue up block sync to get next
                // missing historical block

                let current_block_hash = BlockHash::default();
                match self
                    .blocks_accumulator
                    .sync_instruction(StartingWith::Hash(current_block_hash))
                {
                    SyncInstruction::Leap => {
                        // we've fallen behind, go back to catch up mode
                        self.state = ReactorState::CatchUp;
                    }
                    SyncInstruction::BlockSync {
                        block_hash,
                        should_fetch_execution_state,
                    } => {
                        self.block_synchronizer.register_block_by_hash(
                            block_hash,
                            should_fetch_execution_state,
                            self.chainspec
                                .core_config
                                .sync_leap_simultaneous_peer_requests,
                        );
                        // need to crank or put something on the event-q
                        todo!();
                    }
                    SyncInstruction::BlockExec { block } => {
                        // need to crank or put something on the event-q
                        self.block_synchronizer.register_block_by_hash(
                            block.id(),
                            false,
                            self.chainspec
                                .core_config
                                .sync_leap_simultaneous_peer_requests,
                        );
                    }
                    SyncInstruction::CaughtUp => {
                        // if node is in validator set and era supervisor has what it needs
                        // to run, switch to validate mode
                        if self.consensus.is_active_validator() {
                            // push era_supervisor event onto queue
                            self.state = ReactorState::Validate;
                        }
                    }
                }
            }
            ReactorState::Validate => {
                if self.consensus.is_active_validator() == false {
                    // either consensus doesn't have enough protocol data
                    // or this node has been evicted or has naturally
                    // fallen out of the validator set in a new era.
                    // regardless, go back to keep up mode;
                    // the keep up logic will handle putting them back
                    // to catch up if necessary, or back to validate
                    // if they become able to validate again
                    self.state = ReactorState::KeepUp;
                }
                // TODO: it is unclear to me if era_supervisor should be cranked here
                // (either imperatively or via putting something on the event-q)
                // or if event-q would naturally handle it based upon consensus
                // messages...must consult with Andreas / Bart

                // TODO: invert era_supervisor idleness timeout and manage it
                // from here by asking era_supervisor.is_idle() or sth similar
            }
        }
        effect_builder
            .immediately()
            .event(|_| MainEvent::ReactorCrank)
    }

    fn catch_up_instructions(
        &mut self,
        rng: &mut NodeRng,
        effect_builder: EffectBuilder<MainEvent>,
    ) -> CatchUpInstruction {
        let mut effects = Effects::new();
        // check idleness & enforce re-attempts if necessary
        if let Some(timestamp) = self.block_synchronizer.last_progress() {
            if Timestamp::now().saturating_diff(timestamp) <= self.idle_tolerances {
                self.attempts = 0; // if any progress has been made, reset attempts
                return CatchUpInstruction::CheckLater(
                    "block_synchronizer is making progress".to_string(),
                    WAIT_SEC * 2,
                );
            }
            self.attempts += 1;
            if self.attempts > self.max_attempts {
                return CatchUpInstruction::Shutdown(
                    "catch up process exceeds idle tolerances".to_string(),
                );
            }
        }

        // determine which block / block_hash we should attempt to leap from
        let starting_with = match self.trusted_hash {
            None => {
                match self.linear_chain.highest_block() {
                    // no trusted hash provided use local tip if available
                    Some(block) => {
                        // -+ : leap w/ local tip
                        StartingWith::Block(Box::new(block.clone()))
                    }
                    None => {
                        if let ActivationPoint::Genesis(timestamp) =
                            self.chainspec.protocol_config.activation_point
                        {
                            if Timestamp::now() <= timestamp {
                                // the network is in pre-genesis
                                return CatchUpInstruction::CommitGenesis;
                            }
                        }
                        // we are post genesis, have no local blocks, and no trusted hash
                        // so we can't possibly catch up to the network and should shut down
                        return CatchUpInstruction::Shutdown(
                            "post-genesis; cannot proceed without trusted hash provided"
                                .to_string(),
                        );
                    }
                }
            }
            Some(trusted_hash) => {
                match self.storage.read_block(&trusted_hash) {
                    Ok(Some(trusted_block)) => {
                        match self.linear_chain.highest_block() {
                            Some(block) => {
                                // ++ : leap w/ the higher of local tip or trusted hash
                                if trusted_block.height() > block.height() {
                                    StartingWith::Hash(trusted_hash)
                                } else {
                                    StartingWith::Block(Box::new(block.clone()))
                                }
                            }
                            None => {
                                // should be unreachable if we've gotten this far
                                StartingWith::Hash(trusted_hash)
                            }
                        }
                    }
                    Ok(None) => {
                        // +- : leap w/ config hash
                        StartingWith::Hash(trusted_hash)
                    }
                    Err(_) => {
                        return CatchUpInstruction::Shutdown(
                                "fatal block store error when attempting to read block under trusted hash".to_string(),
                            );
                    }
                }
            }
        };

        // the block accumulator should be receiving blocks via gossiping
        // and usually has some awareness of the chain ahead of our tip
        let trusted_hash = *starting_with.block_hash();
        match self.blocks_accumulator.sync_instruction(starting_with) {
            SyncInstruction::Leap => {
                let peers_to_ask = self.small_network.peers_random_vec(
                    rng,
                    self.chainspec
                        .core_config
                        .sync_leap_simultaneous_peer_requests,
                );
                effects.extend(effect_builder.immediately().event(move |_| {
                    MainEvent::SyncLeaper(sync_leaper::Event::AttemptLeap {
                        trusted_hash,
                        peers_to_ask,
                    })
                }));
                return CatchUpInstruction::Do(effects);
            }
            SyncInstruction::BlockSync {
                block_hash,
                should_fetch_execution_state,
            } => {
                self.block_synchronizer.register_block_by_hash(
                    block_hash,
                    should_fetch_execution_state,
                    self.chainspec
                        .core_config
                        .sync_leap_simultaneous_peer_requests,
                );
                return CatchUpInstruction::CheckSoon(
                    "block_synchronizer is initialized".to_string(),
                );
            }
            SyncInstruction::BlockExec { block } => {
                let block_hash = block.id();
                debug!(
                    "BlockExec should be unreachable in CatchUp mode: {}",
                    block_hash
                );
                self.block_synchronizer.register_block_by_hash(
                    block_hash,
                    false,
                    self.chainspec
                        .core_config
                        .sync_leap_simultaneous_peer_requests,
                );
                return CatchUpInstruction::CheckSoon(
                    "block_synchronizer is initialized for potentially executable block"
                        .to_string(),
                );
            }
            SyncInstruction::CaughtUp => {
                match self.linear_chain.highest_block() {
                    Some(block) => {
                        if block.header().era_id() == block.header().next_block_era_id() {
                            return CatchUpInstruction::CommitUpgrade(Box::new(
                                block.header().clone(),
                            ));
                        }
                    }
                    None => {
                        // should be unreachable
                        return CatchUpInstruction::Shutdown(
                            "can't be caught up with no block in the block store".to_string(),
                        );
                    }
                }
            }
        }

        // there are no catch up or shutdown instructions, so we must be caught up
        CatchUpInstruction::CaughtUp
    }

    pub(crate) fn commit_genesis(
        &mut self,
        effect_builder: EffectBuilder<MainEvent>,
    ) -> Result<Effects<MainEvent>, String> {
        match self.contract_runtime.commit_genesis(
            self.chainspec.clone().as_ref(),
            self.chainspec_raw_bytes.clone().as_ref(),
        ) {
            Ok(success) => {
                let post_state_hash = success.post_state_hash;

                let genesis_timestamp = match self
                    .chainspec
                    .protocol_config
                    .activation_point
                    .genesis_timestamp()
                {
                    None => {
                        return Err("must have genesis timestamp".to_string());
                    }
                    Some(timestamp) => timestamp,
                };

                info!(
                    %post_state_hash,
                    %genesis_timestamp,
                    network_name = %self.chainspec.network_config.name,
                    "successfully ran genesis"
                );

                let next_block_height = 0;
                let initial_pre_state = ExecutionPreState::new(
                    next_block_height,
                    post_state_hash,
                    BlockHash::default(),
                    Digest::default(),
                );
                self.contract_runtime.set_initial_state(initial_pre_state);

                let finalized_block = FinalizedBlock::new(
                    BlockPayload::default(),
                    Some(EraReport::default()),
                    genesis_timestamp,
                    EraId::default(),
                    next_block_height,
                    PublicKey::System,
                );
                Ok(effect_builder
                    .enqueue_block_for_execution(finalized_block, vec![], vec![])
                    .ignore())
            }
            Err(err) => Err(format!("failed to commit genesis: {:?}", err)),
        }
    }

    pub(crate) fn commit_upgrade(
        &mut self,
        effect_builder: EffectBuilder<MainEvent>,
        previous_block_header: Box<BlockHeader>,
    ) -> Result<Effects<MainEvent>, String> {
        match self.chainspec.ee_upgrade_config(
            *previous_block_header.state_root_hash(),
            previous_block_header.protocol_version(),
            previous_block_header.era_id(),
            self.chainspec_raw_bytes.clone(),
        ) {
            Ok(cfg) => match self.contract_runtime.commit_upgrade(cfg) {
                Ok(success) => {
                    let post_state_hash = success.post_state_hash;
                    info!(
                        network_name = %self.chainspec.network_config.name,
                        %post_state_hash,
                        "upgrade committed"
                    );

                    let next_block_height = previous_block_header.height() + 1;
                    let initial_pre_state = ExecutionPreState::new(
                        next_block_height,
                        post_state_hash,
                        previous_block_header.hash(),
                        previous_block_header.accumulated_seed(),
                    );
                    self.contract_runtime.set_initial_state(initial_pre_state);

                    let finalized_block = FinalizedBlock::new(
                        BlockPayload::default(),
                        Some(EraReport::default()),
                        previous_block_header.timestamp(),
                        previous_block_header.next_block_era_id(),
                        next_block_height,
                        PublicKey::System,
                    );
                    Ok(effect_builder
                        .enqueue_block_for_execution(finalized_block, vec![], vec![])
                        .ignore())
                }
                Err(err) => Err(format!("failed to upgrade protocol: {:?}", err)),
            },
            Err(msg) => Err(msg),
        }
    }
}
