//! Supervisor and Handle implementation.

use async_channel as channel;

use tendermint::evidence::{ConflictingHeadersEvidence, Evidence};

use crate::errors::Error;
use crate::evidence::EvidenceReporter;
use crate::fork_detector::{Fork, ForkDetection, ForkDetector};
use crate::light_client::LightClient;
use crate::peer_list::PeerList;
use crate::state::State;
use crate::types::{Height, LatestStatus, LightBlock, PeerId, Status};
use async_recursion::async_recursion;
use async_trait::async_trait;

/// Provides an interface to the supervisor for use in downstream code.
#[async_trait]
pub trait Handle: Send + Sync {
    /// Get latest trusted block.
    async fn latest_trusted(&self) -> Result<Option<LightBlock>, Error>;

    /// Get the latest status.
    async fn latest_status(&self) -> Result<LatestStatus, Error>;

    /// Verify to the highest block.
    async fn verify_to_highest(&self) -> Result<LightBlock, Error>;

    /// Verify to the block at the given height.
    async fn verify_to_target(&self, _height: Height) -> Result<LightBlock, Error>;

    /// Terminate the underlying [`Supervisor`].
    async fn terminate(&self) -> Result<(), Error>;
}

/// Input events sent by the [`Handle`]s to the [`Supervisor`]. They carry a [`Callback`] which is
/// used to communicate back the responses of the requests.
#[derive(Debug)]
enum HandleInput {
    /// Terminate the supervisor process
    Terminate(channel::Sender<()>),

    /// Verify to the highest height, call the provided callback with result
    VerifyToHighest(channel::Sender<Result<LightBlock, Error>>),

    /// Verify to the given height, call the provided callback with result
    VerifyToTarget(Height, channel::Sender<Result<LightBlock, Error>>),

    /// Get the latest trusted block.
    LatestTrusted(channel::Sender<Option<LightBlock>>),

    /// Get the current status of the LightClient
    GetStatus(channel::Sender<LatestStatus>),
}

/// A light client `Instance` packages a `LightClient` together with its `State`.
#[derive(Debug)]
pub struct Instance {
    /// The light client for this instance
    pub light_client: LightClient,

    /// The state of the light client for this instance
    pub state: State,
}

impl Instance {
    /// Constructs a new instance from the given light client and its state.
    pub fn new(light_client: LightClient, state: State) -> Self {
        Self {
            light_client,
            state,
        }
    }

    /// Get the latest trusted block.
    pub fn latest_trusted(&self) -> Option<LightBlock> {
        self.state.light_store.highest(Status::Trusted)
    }

    /// Trust the given block.
    pub fn trust_block(&mut self, lb: &LightBlock) {
        self.state.light_store.update(lb, Status::Trusted);
    }
}

/// The supervisor manages multiple light client instances, of which one
/// is deemed to be the primary instance through which blocks are retrieved
/// and verified. The other instances are considered as witnesses
/// which are consulted to perform fork detection.
///
/// If primary verification fails, the primary client is removed and a witness
/// is promoted to primary. If a witness is deemed faulty, then the witness is
/// removed.
///
/// The supervisor is intended to be ran in its own thread, and queried
/// via a `Handle`.
///
/// ## Example
///
/// ```rust,ignore
/// let mut supervisor: Supervisor = todo!();
/// let mut handle = supervisor.handle();
///
/// // Spawn the supervisor in its own thread.
/// std::thread::spawn(|| supervisor.run());
///
/// loop {
///     // Asynchronously query the supervisor via a handle
///     let maybe_block = handle.verify_to_highest();
///     match maybe_block {
///         Ok(light_block) => {
///             println!("[info] synced to block {}", light_block.height());
///         }
///         Err(e) => {
///             println!("[error] sync failed: {}", e);
///         }
///     };
///
///     std::thread::sleep(Duration::from_millis(800));
/// }
/// ```
pub struct Supervisor {
    /// List of peers and their instances (primary, witnesses, full and faulty nodes)
    peers: PeerList<Instance>,
    /// An instance of the fork detector
    fork_detector: Box<dyn ForkDetector>,
    /// Reporter of fork evidence
    evidence_reporter: Box<dyn EvidenceReporter>,
    /// Channel through which to reply to `Handle`s
    sender: channel::Sender<HandleInput>,
    /// Channel through which to receive events from the `Handle`s
    receiver: channel::Receiver<HandleInput>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("peers", &self.peers)
            .finish()
    }
}

// Ensure the `Supervisor` can be sent across thread boundaries.
// static_assertions::assert_impl_all!(Supervisor: Send);

impl Supervisor {
    /// Constructs a new supervisor from the given list of peers and fork detector instance.
    pub fn new(
        peers: PeerList<Instance>,
        fork_detector: impl ForkDetector + 'static,
        evidence_reporter: impl EvidenceReporter + 'static,
    ) -> Self {
        let (sender, receiver) = channel::unbounded::<HandleInput>();

        Self {
            peers,
            sender,
            receiver,
            fork_detector: Box::new(fork_detector),
            evidence_reporter: Box::new(evidence_reporter),
        }
    }

    /// Create a new handle to this supervisor.
    pub fn handle(&self) -> SupervisorHandle {
        SupervisorHandle::new(self.sender.clone())
    }

    /// Get the latest trusted state of the primary peer, if any
    pub fn latest_trusted(&self) -> Option<LightBlock> {
        self.peers.primary().latest_trusted()
    }

    /// Verify to the highest block.
    pub async fn verify_to_highest(&mut self) -> Result<LightBlock, Error> {
        self.verify(None).await
    }

    /// Return latest trusted status summary.
    fn latest_status(&mut self) -> LatestStatus {
        let latest_trusted = self.peers.primary().latest_trusted();
        let mut connected_nodes = vec![self.peers.primary_id()];
        connected_nodes.append(&mut self.peers.witnesses_ids().iter().copied().collect());

        match latest_trusted {
            Some(trusted) => LatestStatus::new(
                Some(trusted.signed_header.header.height.value()),
                Some(trusted.signed_header.header.hash()),
                Some(trusted.next_validators.hash()),
                connected_nodes,
            ),
            // only return connected nodes to see what is going on:
            None => LatestStatus::new(None, None, None, connected_nodes),
        }
    }

    /// Verify to the block at the given height.
    pub async fn verify_to_target(&mut self, height: Height) -> Result<LightBlock, Error> {
        self.verify(Some(height)).await
    }

    /// Verify either to the latest block (if `height == None`) or to a given block (if `height ==
    /// Some(height)`).
    #[async_recursion(?Send)]
    async fn verify(&mut self, height: Option<Height>) -> Result<LightBlock, Error> {
        let primary = self.peers.primary_mut();

        // Perform light client core verification for the given height (or highest).
        let verdict = match height {
            None => primary.light_client.verify_to_highest(&mut primary.state).await,
            Some(height) => primary
                .light_client
                .verify_to_target(height, &mut primary.state).await,
        };

        match verdict {
            // Verification succeeded, let's perform fork detection
            Ok(verified_block) => {
                let trusted_block = primary
                    .latest_trusted()
                    .ok_or_else(|| Error::no_trusted_state(Status::Trusted))?;

                // Perform fork detection with the highest verified block and the trusted block.
                let outcome = self.detect_forks(&verified_block, &trusted_block).await?;

                match outcome {
                    // There was a fork or a faulty peer
                    ForkDetection::Detected(forks) => {
                        let forked = self.process_forks(forks).await?;
                        if !forked.is_empty() {
                            // Fork detected, exiting
                            return Err(Error::fork_detected(forked));
                        }

                        // If there were no hard forks, perform verification again
                        self.verify(height).await
                    }
                    ForkDetection::NotDetected => {
                        // We need to re-ask for the primary here as the compiler
                        // is not smart enough to realize that we do not mutate
                        // the `primary` field of `PeerList` between the initial
                        // borrow of the primary and here (can't blame it, it's
                        // not that obvious).
                        self.peers.primary_mut().trust_block(&verified_block);

                        // No fork detected, exiting
                        Ok(verified_block)
                    }
                }
            }
            // Verification failed
            Err(err) => {
                // Swap primary, and continue with new primary, if there is any witness left.
                self.peers.replace_faulty_primary(Some(err))?;
                self.verify(height).await
            }
        }
    }

    async fn process_forks(&mut self, forks: Vec<Fork>) -> Result<Vec<PeerId>, Error> {
        let mut forked = Vec::with_capacity(forks.len());

        for fork in forks {
            match fork {
                // An actual fork was detected, report evidence and record forked peer.
                // TODO: also report to primary
                Fork::Forked { primary, witness } => {
                    let provider = witness.provider;
                    self.report_evidence(provider, &primary, &witness).await?;

                    forked.push(provider);
                }
                // A witness has timed out, remove it from the peer list.
                Fork::Timeout(provider, _error) => {
                    self.peers.replace_faulty_witness(provider);
                    // TODO: Log/record the error
                }
                // A witness has been deemed faulty, remove it from the peer list.
                Fork::Faulty(block, _error) => {
                    self.peers.replace_faulty_witness(block.provider);
                    // TODO: Log/record the error
                }
            }
        }

        Ok(forked)
    }

    /// Report the given evidence of a fork.
    async fn report_evidence(
        &mut self,
        provider: PeerId,
        primary: &LightBlock,
        witness: &LightBlock,
    ) -> Result<(), Error> {
        let evidence = ConflictingHeadersEvidence::new(
            primary.signed_header.clone(),
            witness.signed_header.clone(),
        );

        self.evidence_reporter
            .report(Evidence::ConflictingHeaders(Box::new(evidence)), provider).await
            .map_err(Error::io)?;

        Ok(())
    }

    /// Perform fork detection with the given verified block and trusted block.
    async fn detect_forks(
        &self,
        verified_block: &LightBlock,
        trusted_block: &LightBlock,
    ) -> Result<ForkDetection, Error> {
        if self.peers.witnesses_ids().is_empty() {
            return Err(Error::no_witnesses());
        }

        let witnesses = self
            .peers
            .witnesses_ids()
            .iter()
            .filter_map(|id| self.peers.get(id))
            .collect();

        self.fork_detector
            .detect_forks(verified_block, trusted_block, witnesses).await
    }

    /// Run the supervisor event loop in the same thread.
    ///
    /// This method should typically be called within a new thread with `std::thread::spawn`.
    pub async fn run(mut self) -> Result<(), Error> {
        loop {
            let event = self.receiver.recv().await.map_err(Error::recv)?;

            match event {
                HandleInput::LatestTrusted(sender) => {
                    let outcome = self.latest_trusted();
                    sender.send(outcome).await.map_err(Error::send)?;
                }
                HandleInput::Terminate(sender) => {
                    sender.send(()).await.map_err(Error::send)?;
                    return Ok(());
                }
                HandleInput::VerifyToTarget(height, sender) => {
                    let outcome = self.verify_to_target(height).await;
                    sender.send(outcome).await.map_err(Error::send)?;
                }
                HandleInput::VerifyToHighest(sender) => {
                    let outcome = self.verify_to_highest().await;
                    sender.send(outcome).await.map_err(Error::send)?;
                }
                HandleInput::GetStatus(sender) => {
                    let outcome = self.latest_status();
                    sender.send(outcome).await.map_err(Error::send)?;
                }
            }
        }
    }
}

/// A [`Handle`] to the [`Supervisor`] which allows to communicate with
/// the supervisor across thread boundaries via message passing.
#[derive(Clone)]
pub struct SupervisorHandle {
    sender: channel::Sender<HandleInput>,
}

impl SupervisorHandle {
    /// Crate a new handle that sends events to the supervisor via
    /// the given channel. For internal use only.
    fn new(sender: channel::Sender<HandleInput>) -> Self {
        Self { sender }
    }

    async fn verify(
        &self,
        make_event: impl FnOnce(channel::Sender<Result<LightBlock, Error>>) -> HandleInput,
    ) -> Result<LightBlock, Error> {
        let (sender, receiver) = channel::bounded::<Result<LightBlock, Error>>(1);

        let event = make_event(sender);
        self.sender.send(event).await.map_err(Error::send)?;

        receiver.recv().await.map_err(Error::recv)?
    }
}

#[async_trait]
impl Handle for SupervisorHandle {
    async fn latest_trusted(&self) -> Result<Option<LightBlock>, Error> {
        let (sender, receiver) = channel::bounded::<Option<LightBlock>>(1);

        self.sender
            .send(HandleInput::LatestTrusted(sender)).await
            .map_err(Error::send)?;

        receiver.recv().await.map_err(Error::recv)
    }

    async fn latest_status(&self) -> Result<LatestStatus, Error> {
        let (sender, receiver) = channel::bounded::<LatestStatus>(1);
        self.sender
            .send(HandleInput::GetStatus(sender)).await
            .map_err(Error::send)?;
        receiver.recv().await.map_err(Error::recv)
    }

    async fn verify_to_highest(&self) -> Result<LightBlock, Error> {
        self.verify(HandleInput::VerifyToHighest).await
    }

    async fn verify_to_target(&self, height: Height) -> Result<LightBlock, Error> {
        self.verify(|sender| HandleInput::VerifyToTarget(height, sender)).await
    }

    async fn terminate(&self) -> Result<(), Error> {
        let (sender, receiver) = channel::bounded::<()>(1);

        self.sender
            .send(HandleInput::Terminate(sender))
            .await.map_err(Error::send)?;

        receiver.recv().await.map_err(Error::recv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{Error, ErrorDetail};
    use crate::light_client::Options;
    use crate::operations::ProdHasher;
    use crate::{
        components::{
            io::{self, AtHeight, Io},
            scheduler,
            verifier::ProdVerifier,
        },
        fork_detector::ProdForkDetector,
        store::{memory::MemoryStore, LightStore},
        tests::{MockClock, MockEvidenceReporter, MockIo, TrustOptions},
        types::Time,
    };
    use std::{collections::HashMap, convert::TryFrom, time::Duration};
    use tendermint::block::Height;
    use tendermint::evidence::Duration as DurationStr;
    use tendermint::trust_threshold::TrustThresholdFraction;
    use tendermint_rpc::{
        self as rpc,
        response_error::{Code, ResponseError},
    };
    use tendermint_testgen::helpers::get_time;
    use tendermint_testgen::{
        Commit, Generator, Header, LightBlock as TestgenLightBlock, LightChain, ValidatorSet,
    };

    async fn make_instance(
        peer_id: PeerId,
        trust_options: TrustOptions,
        io: MockIo,
        now: Time,
    ) -> Instance {
        let trusted_height = trust_options.height;
        let trusted_state = io
            .fetch_light_block(AtHeight::At(trusted_height)).await
            .expect("could not 'request' light block");

        let mut light_store = MemoryStore::new();
        light_store.insert(trusted_state, Status::Trusted);

        let state = State {
            light_store: Box::new(light_store),
            verification_trace: HashMap::new(),
        };

        let options = Options {
            trust_threshold: trust_options.trust_level,
            trusting_period: trust_options.period.into(),
            clock_drift: Duration::from_secs(0),
        };

        let verifier = ProdVerifier::default();
        let clock = MockClock { now };
        let scheduler = scheduler::basic_bisecting_schedule;
        let hasher = ProdHasher::default();

        let light_client =
            LightClient::new(peer_id, options, clock, scheduler, verifier, hasher, io);

        Instance::new(light_client, state)
    }

    fn run_bisection_test(
        peer_list: PeerList<Instance>,
        height_to_verify: u64,
    ) -> (Result<LightBlock, Error>, LatestStatus) {
        let supervisor = Supervisor::new(
            peer_list,
            ProdForkDetector::default(),
            MockEvidenceReporter::new(),
        );

        let handle = supervisor.handle();
        tokio::task::spawn_local(async move {
            supervisor.run().await
        });

        let target_height = Height::try_from(height_to_verify).expect("Error while making height");

        crate::utils::block_on(None,async move{
            (
                handle.verify_to_target(target_height).await,
                handle.latest_status().await.unwrap(),
            )
        }).unwrap()

    }

    async fn make_peer_list(
        primary: Option<Vec<LightBlock>>,
        witnesses: Option<Vec<Vec<LightBlock>>>,
        now: Time,
    ) -> PeerList<Instance> {
        let trust_options = TrustOptions {
            period: DurationStr(Duration::new(604800, 0)),
            height: Height::try_from(1_u64).expect("Error while making height"),
            trust_level: TrustThresholdFraction::TWO_THIRDS,
        };

        let mut peer_list = PeerList::builder();

        if let Some(primary) = primary {
            let io = MockIo::new(
                primary[0].signed_header.header.chain_id.to_string(),
                primary.clone(),
            );

            let primary_instance =
                make_instance(primary[0].provider, trust_options.clone(), io, now).await;

            peer_list.primary(primary[0].provider, primary_instance);
        }

        if let Some(witnesses) = witnesses {
            for provider in witnesses.into_iter() {
                let peer_id = provider[0].provider;
                let io = MockIo::new(
                    provider[0].signed_header.header.chain_id.to_string(),
                    provider,
                );
                let instance = make_instance(peer_id, trust_options.clone(), io.clone(), now).await;
                peer_list.witness(peer_id, instance);
            }
        }
        peer_list.build()
    }

    fn change_provider(
        mut light_blocks: Vec<LightBlock>,
        peer_id: Option<&str>,
    ) -> Vec<LightBlock> {
        let provider = peer_id.unwrap_or("0BEFEEDC0C0ADEADBEBADFADADEFC0FFEEFACADE");
        for lb in light_blocks.iter_mut() {
            lb.provider = provider.parse().unwrap();
        }
        light_blocks
    }

    fn make_conflicting_witness(
        length: u64,
        val_ids: Option<Vec<&str>>,
        chain_id: Option<&str>,
        provider: Option<&str>,
    ) -> Vec<LightBlock> {
        let vals = match val_ids {
            Some(val_ids) => ValidatorSet::new(val_ids).validators.unwrap(),
            None => ValidatorSet::new(vec!["1"]).validators.unwrap(),
        };

        let chain = chain_id.unwrap_or("other-chain");

        let peer_id = provider.unwrap_or("0BEFEEDC0C0ADEADBEBADFADADEFC0FFEEFACADE");

        let header = Header::new(&vals).height(1).chain_id(chain).time(1);
        let commit = Commit::new(header.clone(), 1);
        let mut lb = TestgenLightBlock::new(header, commit).provider(peer_id);

        let mut witness: Vec<LightBlock> = vec![lb.generate().unwrap().into()];

        for _ in 1..length {
            lb = lb.next();
            let tm_lb = lb.generate().unwrap().into();
            witness.push(tm_lb);
        }

        witness
    }

    #[test]
    fn test_bisection_happy_path() {
        let chain = LightChain::default_with_length(10);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let witness = change_provider(primary.clone(), None);

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary.clone()), Some(vec![witness]), get_time(11)));

        let (result, _) = run_bisection_test(peer_list, 10);

        let expected_state = primary[9].clone();
        let new_state = result.unwrap();

        assert_eq!(expected_state, new_state);
    }

    #[test]
    fn test_bisection_no_witnesses() {
        let chain = LightChain::default_with_length(10);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary), None, get_time(11)));

        let (result, _) = run_bisection_test(peer_list, 10);

        match result {
            Err(Error(ErrorDetail::NoWitnesses(_), _)) => {}
            _ => panic!("expected NoWitnesses error, instead got {:?}", result),
        }
    }

    #[test]
    fn test_bisection_io_error() {
        let chain = LightChain::default_with_length(10);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let mut light_blocks = primary.clone();
        light_blocks.truncate(9);
        let witness = change_provider(light_blocks, None);

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary), Some(vec![witness]), get_time(11)));

        let (result, _) = run_bisection_test(peer_list, 10);

        match result {
            Err(Error(ErrorDetail::Io(e), _)) => match e.source {
                io::IoErrorDetail::Rpc(e) => match e.source {
                    rpc::error::ErrorDetail::Response(e) => {
                        assert_eq!(e.source, ResponseError::new(Code::InvalidRequest, None))
                    }
                    _ => panic!("expected Response error"),
                },
                _ => panic!("expected Rpc error"),
            },
            _ => panic!("expected Io error"),
        }
    }

    #[test]
    fn test_bisection_no_witness_left() {
        let chain = LightChain::default_with_length(5);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let witness = make_conflicting_witness(5, None, None, None);

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary), Some(vec![witness]), get_time(11)));

        let (result, _) = run_bisection_test(peer_list, 10);

        // FIXME: currently this test does not test what it is supposed to test,
        // because MockIo returns an InvalidRequest error. This was previously
        // treated as a NoWitnessLeft error, which was misclassified.
        match result {
            Err(Error(ErrorDetail::Io(e), _)) => match e.source {
                crate::components::io::IoErrorDetail::Rpc(e) => match e.source {
                    rpc::error::ErrorDetail::Response(e) => {
                        assert_eq!(e.source.code(), rpc::Code::InvalidRequest)
                    }
                    _ => {
                        panic!("expected Response error, instead got {:?}", e)
                    }
                },
                _ => {
                    panic!("expected Rpc error, instead got {:?}", e)
                }
            },
            _ => panic!("expected Io error, instead got {:?}", result),
        }
    }

    #[test]
    fn test_bisection_fork_detected() {
        let mut chain = LightChain::default_with_length(5);
        let primary = chain
            .light_blocks
            .clone()
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let mut header = chain.light_blocks[4].header.clone().unwrap();
        let mut time = header.time.unwrap();
        time += 3;
        header.time = Some(time);
        chain.light_blocks[4].header = Some(header.clone());
        chain.light_blocks[4].commit = Some(Commit::new(header, 1));

        let witness = change_provider(
            chain
                .light_blocks
                .into_iter()
                .map(|lb| lb.generate().unwrap().into())
                .collect::<Vec<LightBlock>>(),
            None,
        );

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary), Some(vec![witness]), get_time(11)));

        let (result, _) = run_bisection_test(peer_list, 5);

        match result {
            Err(Error(ErrorDetail::ForkDetected(_), _)) => {}
            _ => panic!("expected ForkDetected error"),
        }
    }

    #[test]
    fn test_bisection_no_initial_trusted_state() {
        let chain = LightChain::default_with_length(10);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let witness1 = change_provider(primary.clone(), None);
        let witness2 = change_provider(
            witness1.clone(),
            Some("EDC0C0ADEADBEBA0BEFEDFADADEFC0FFEEFACADE"),
        );

        let mut peer_list = async_std::task::block_on(make_peer_list(
            Some(primary.clone()),
            Some(vec![witness1.clone(), witness2]),
            get_time(11),
        ));
        peer_list
            .get_mut(&primary[0].provider)
            .expect("cannot find instance")
            .state
            .light_store
            .remove(
                Height::try_from(1_u64).expect("bad height"),
                Status::Trusted,
            );

        let (result, latest_status) = run_bisection_test(peer_list, 10);

        // In the case where there is no initial trusted state found from a primary peer,
        // the primary node is marked as faulty and replaced with a witness node (if available)
        // and continues verification

        let expected_state = &witness1[9];
        let new_state = result.unwrap();

        assert_eq!(expected_state, &new_state);

        // Check that we successfully disconnected from the "faulty" primary node
        assert!(!latest_status
            .connected_nodes
            .iter()
            .any(|&peer| peer == primary[0].provider));
    }

    #[test]
    fn test_bisection_trusted_state_outside_trusting_period() {
        let chain = LightChain::default_with_length(10);
        let primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        let witness = change_provider(primary.clone(), None);

        let peer_list =
            async_std::task::block_on(make_peer_list(Some(primary.clone()), Some(vec![witness]), get_time(604801)));

        let (_, latest_status) = run_bisection_test(peer_list, 2);

        // In the case where trusted state of a primary peer is outside the trusting period,
        // the primary node is marked as faulty and replaced with a witness node (if available)
        // and continues verification
        // Check if the node was removed from the list

        assert!(!latest_status
            .connected_nodes
            .iter()
            .any(|&peer| peer == primary[0].provider));
    }

    #[test]
    fn test_bisection_invalid_light_block() {
        let chain = LightChain::default_with_length(10);
        let mut primary = chain
            .light_blocks
            .into_iter()
            .map(|lb| lb.generate().unwrap().into())
            .collect::<Vec<LightBlock>>();

        primary[9].signed_header.commit.round = primary[9].signed_header.commit.round.increment();

        let witness = change_provider(primary.clone(), None);

        let peer_list = async_std::task::block_on(make_peer_list(Some(primary.clone()), Some(vec![witness]), get_time(11)));

        let (_, latest_status) = run_bisection_test(peer_list, 10);

        // In the case where a primary peer provides an invalid light block
        // i.e. verification for the light block failed,
        // the primary node is marked as faulty and replaced with a witness node (if available)
        // and continues verification
        // Check if the node was removed from the list

        assert!(!latest_status
            .connected_nodes
            .iter()
            .any(|&peer| peer == primary[0].provider));
    }
}
