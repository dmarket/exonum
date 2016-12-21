use std::collections::{VecDeque, BinaryHeap, HashSet};
use std::iter::FromIterator;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::net::SocketAddr;
use std::ops::Drop;

use time::{Timespec, Duration};

use exonum::node::{NodeHandler, Configuration, ExternalMessage, NodeTimeout, ListenerConfig};
use exonum::blockchain::{Blockchain, ConsensusConfig, GenesisConfig, Block, StoredConfiguration};
use exonum::storage::{MemoryDB, Error as StorageError};
use exonum::messages::{Any, Message, RawMessage, Connect};
use exonum::events::{Reactor, Event, EventsConfiguration, NetworkConfiguration, InternalEvent,
                     Channel, EventHandler, Result as EventsResult};
use exonum::crypto::{hash, Hash, PublicKey, SecretKey, gen_keypair};
use exonum::node::state::{Round, Height};

use timestamping::TimestampingBlockchain;

use super::TimestampingTxGenerator;

#[derive(PartialEq, Eq)]
struct TimerPair(Timespec, NodeTimeout);
type SandboxEvent<B> = InternalEvent<ExternalMessage<B>, NodeTimeout>;

impl PartialOrd for TimerPair {
    fn partial_cmp(&self, other: &Self) -> Option<::std::cmp::Ordering> {
        Some((&self.0, &self.1).cmp(&(&other.0, &other.1)).reverse())
    }
}

impl Ord for TimerPair {
    fn cmp(&self, other: &Self) -> ::std::cmp::Ordering {
        (&self.0, &self.1).cmp(&(&other.0, &other.1)).reverse()
    }
}

struct SandboxInner<B: Blockchain> {
    address: SocketAddr,
    time: Timespec,
    sended: VecDeque<(SocketAddr, RawMessage)>,
    events: VecDeque<SandboxEvent<B>>,
    timers: BinaryHeap<TimerPair>,
}

pub struct SandboxReactor<B: Blockchain> {
    inner: Arc<Mutex<SandboxInner<B>>>,
    handler: NodeHandler<B, SandboxChannel<B>>,
}

#[derive(Clone)]
pub struct SandboxChannel<B: Blockchain> {
    inner: Arc<Mutex<SandboxInner<B>>>,
}

impl<B: Blockchain> SandboxChannel<B> {
    fn send_event(&self, event: SandboxEvent<B>) {
        self.inner.lock().unwrap().events.push_back(event);
    }

    fn send_message(&self, address: &SocketAddr, message: RawMessage) {
        self.inner.lock().unwrap().sended.push_back((address.clone(), message));
    }
}

impl<B> Channel for SandboxChannel<B>
    where B: Blockchain
{
    type ApplicationEvent = ExternalMessage<B>;
    type Timeout = NodeTimeout;

    fn address(&self) -> SocketAddr {
        self.inner.lock().unwrap().address
    }

    fn get_time(&self) -> Timespec {
        self.inner.lock().unwrap().time
    }

    fn post_event(&self, event: Self::ApplicationEvent) -> EventsResult<()> {
        let msg = InternalEvent::Application(event);
        self.send_event(msg);
        Ok(())
    }

    fn send_to(&mut self, address: &SocketAddr, message: RawMessage) {
        // TODO handle attempts to send message to offline nodes
        self.send_message(address, message);
    }

    fn connect(&mut self, address: &SocketAddr) {
        let event = InternalEvent::Node(Event::Connected(*address));
        self.send_event(event);
    }

    fn add_timeout(&mut self, timeout: Self::Timeout, time: Timespec) {
        // assert!(time < self.inner.borrow().time, "Tring to add timeout for the past");
        let pair = TimerPair(time, timeout);
        self.inner.lock().unwrap().timers.push(pair);
    }
}

impl<B> Reactor<NodeHandler<B, SandboxChannel<B>>> for SandboxReactor<B>
    where B: Blockchain
{
    type Channel = SandboxChannel<B>;

    fn bind(&mut self) -> ::std::io::Result<()> {
        Ok(())
    }
    fn run(&mut self) -> ::std::io::Result<()> {
        unreachable!();
    }
    fn run_once(&mut self, _: Option<usize>) -> ::std::io::Result<()> {
        loop {
            let result = self.inner.lock().unwrap().events.pop_front();
            if let Some(event) = result {
                match event {
                    InternalEvent::Node(event) => {
                        self.handler.handle_event(event);
                    }
                    InternalEvent::Application(event) => {
                        self.handler.handle_application_event(event);
                    }
                    InternalEvent::Invoke(_) => {}
                }
            } else {
                break;
            }
        }
        Ok(())
    }
    fn get_time(&self) -> Timespec {
        self.inner.lock().unwrap().time
    }
    fn channel(&self) -> SandboxChannel<B> {
        SandboxChannel { inner: self.inner.clone() }
    }
}

impl<B> SandboxReactor<B>
    where B: Blockchain
{
    pub fn is_leader(&self) -> bool {
        self.handler.is_leader()
    }

    pub fn last_block(&self) -> Result<Block, StorageError> {
        self.handler.blockchain.last_block()
    }

    pub fn last_hash(&self) -> Result<Hash, StorageError> {
        self.handler.blockchain.last_hash()
    }

    pub fn actual_config(&self) -> Result<StoredConfiguration, StorageError> {
        B::get_actual_configuration(&self.handler.blockchain.view())
    }

    pub fn handle_message(&mut self, msg: RawMessage) {
        let event = Event::Incoming(msg);
        self.handler.handle_event(event);
    }

    pub fn handle_timeout(&mut self, timeout: NodeTimeout) {
        self.handler.handle_timeout(timeout);
    }
}

pub struct Sandbox<B, G>
    where B: Blockchain,
          G: Iterator<Item = B::Transaction>
{
    inner: Arc<Mutex<SandboxInner<B>>>,
    reactor: RefCell<SandboxReactor<B>>,
    tx_generator: RefCell<G>,
    validators: Vec<(PublicKey, SecretKey)>,
    addresses: Vec<SocketAddr>,
}

impl<B, G> Sandbox<B, G>
    where B: Blockchain,
          G: Iterator<Item = B::Transaction>
{
    fn initialize(&self) {
        let connect = Connect::new(self.p(0), self.a(0), self.time(), self.s(0));

        self.recv(Connect::new(self.p(1), self.a(1), self.time(), self.s(1)));
        self.send(self.a(1), connect.clone());

        self.recv(Connect::new(self.p(2), self.a(2), self.time(), self.s(2)));
        self.send(self.a(2), connect.clone());

        self.recv(Connect::new(self.p(3), self.a(3), self.time(), self.s(3)));
        self.send(self.a(3), connect.clone());

        self.check_unexpected_message()
    }

    fn check_unexpected_message(&self) {
        let sended = self.inner.lock().unwrap().sended.pop_front();
        if let Some((addr, msg)) = sended {
            let any_msg = Any::<B::Transaction>::from_raw(msg.clone())
                .expect("Send incorrect message");
            panic!("Send unexpected message {:?} to {}", any_msg, addr);
        }
    }

    pub fn gen_tx(&self) -> B::Transaction {
        self.tx_generator.borrow_mut().next().unwrap()
    }

    pub fn gen_txs(&self, count: usize) -> Vec<B::Transaction> {
        let mut v = Vec::new();
        let mut tx_generator = self.tx_generator.borrow_mut();
        for _ in 0..count {
            v.push(tx_generator.next().unwrap())
        }
        v
    }

    pub fn p(&self, id: usize) -> &PublicKey {
        &self.validators[id].0
    }

    pub fn s(&self, id: usize) -> &SecretKey {
        &self.validators[id].1
    }

    pub fn a(&self, id: usize) -> SocketAddr {
        self.addresses[id].clone()
    }

    pub fn n_validators(&self) -> usize {
        self.validators.len()
    }

    pub fn time(&self) -> Timespec {
        self.inner.lock().unwrap().time.clone()
    }

    pub fn recv<T: Message>(&self, msg: T) {
        self.check_unexpected_message();
        let mut reactor = self.reactor.borrow_mut();
        reactor.handle_message(msg.raw().clone());
        reactor.run_once(None).unwrap();
    }

    pub fn send<T: Message>(&self, addr: SocketAddr, msg: T) {
        let any_expected_msg = Any::<B::Transaction>::from_raw(msg.raw().clone()).unwrap();
        let sended = self.inner.lock().unwrap().sended.pop_front();
        if let Some((real_addr, real_msg)) = sended {
            let any_real_msg = Any::from_raw(real_msg.clone()).expect("Send incorrect message");
            if real_addr != addr || any_real_msg != any_expected_msg {
                panic!("Expected to send the message {:?} to {} instead sending {:?} to {}",
                       any_expected_msg,
                       addr,
                       any_real_msg,
                       real_addr)
            }
        } else {
            panic!("Expected to send the message {:?} to {} but nothing happened",
                   any_expected_msg,
                   addr);
        }
    }

    // TODO: add self-test for broadcasting?
    pub fn broadcast<T: Message>(&self, msg: T) {
        let any_expected_msg = Any::<B::Transaction>::from_raw(msg.raw().clone()).unwrap();
        let mut set = HashSet::from_iter(self.addresses
            .iter()
            .skip(1)
            .map(Clone::clone)): HashSet<SocketAddr>;
        for _ in 0..self.validators.len() - 1 {
            let sended = self.inner.lock().unwrap().sended.pop_front();
            if let Some((real_addr, real_msg)) = sended {
                let any_real_msg = Any::from_raw(real_msg.clone()).expect("Send incorrect message");
                if any_real_msg != any_expected_msg {
                    panic!("Expected to broadcast the message {:?} instead sending {:?} to {}",
                           any_expected_msg,
                           any_real_msg,
                           real_addr)
                }
                if !set.contains(&real_addr) {
                    panic!("Double send the same message {:?} to {:?} during broadcasting",
                           any_expected_msg,
                           real_addr)
                } else {
                    set.remove(&real_addr);
                }
            } else {
                panic!("Expected to broadcast the message {:?} but someone don't recieve \
                        messages: {:?}",
                       any_expected_msg,
                       set);
            }
        }
    }

    pub fn add_time(&self, duration: Duration) {
        self.check_unexpected_message();
        let now = {
            let mut inner = self.inner.lock().unwrap();
            inner.time = inner.time + duration;
            inner.time
        };
        // handle timeouts if occurs
        loop {
            let timeout = {
                let ref mut timers = self.inner.lock().unwrap().timers;
                if let Some(TimerPair(time, timeout)) = timers.pop() {
                    if time > now {
                        timers.push(TimerPair(time, timeout));
                        break;
                    } else {
                        timeout
                    }
                } else {
                    break;
                }
            };
            let mut reactor = self.reactor.borrow_mut();
            reactor.handle_timeout(timeout);
            reactor.run_once(None).unwrap();
        }
    }

    pub fn is_leader(&self) -> bool {
        let reactor = self.reactor.borrow();
        reactor.is_leader()
    }

    pub fn last_block(&self) -> Block {
        let reactor = self.reactor.borrow();
        reactor.last_block().unwrap()
    }

    pub fn last_hash(&self) -> Hash {
        let reactor = self.reactor.borrow();
        reactor.last_hash().unwrap()
    }

    pub fn cfg(&self) -> StoredConfiguration {
        let reactor = self.reactor.borrow();
        reactor.actual_config().unwrap()
    }

    pub fn propose_timeout(&self) -> i64 {
        self.cfg().consensus.propose_timeout
    }

    pub fn round_timeout(&self) -> i64 {
        self.cfg().consensus.round_timeout
    }

    pub fn transactions_hashes(&self) -> Vec<Hash> {
        self.reactor.borrow().handler.state().transactions().keys().cloned().collect()
    }

    pub fn current_round(&self) -> Round {
        self.reactor.borrow().handler.state().round()
    }

    pub fn current_height(&self) -> Height {
        self.reactor.borrow().handler.state().height()
    }

    pub fn current_leader(&self) -> Round {
        self.reactor.borrow().handler.state().leader(self.current_round())
    }

    pub fn assert_state(&self, height: Height, round: Round) {
        let reactor = self.reactor.borrow();
        let ref state = reactor.handler.state();

        let achual_height = state.height();
        let actual_round = state.round();
        assert!(achual_height == height,
                "Incorrect height, actual={}, expected={}",
                achual_height,
                height);
        assert!(actual_round == round,
                "Incorrect round, actual={}, expected={}",
                actual_round,
                round);
    }

    pub fn assert_lock(&self, round: Round, hash: Option<Hash>) {
        let reactor = self.reactor.borrow();
        let state = reactor.handler.state();

        let actual_round = state.locked_round();
        let actual_hash = state.locked_propose();
        assert!(actual_round == round,
                "Incorrect round, actual={}, expected={}",
                actual_round,
                round);
        assert!(actual_hash == hash,
                "Incorrect hash, actual={:?}, expected={:?}",
                actual_hash,
                hash);
    }
}

impl<B, G> Drop for Sandbox<B, G>
    where B: Blockchain,
          G: Iterator<Item = B::Transaction>
{
    fn drop(&mut self) {
        if !::std::thread::panicking() {
            self.check_unexpected_message();
        }
    }
}

pub fn timestamping_sandbox
    ()
    -> Sandbox<TimestampingBlockchain<MemoryDB>, TimestampingTxGenerator>
{
    let validators = vec![gen_keypair(), gen_keypair(), gen_keypair(), gen_keypair()];
    let addresses = vec!["1.1.1.1:1".parse().unwrap(),
                         "2.2.2.2:2".parse().unwrap(),
                         "3.3.3.3:3".parse().unwrap(),
                         "4.4.4.4:4".parse().unwrap()]: Vec<SocketAddr>;

    let blockchain = TimestampingBlockchain { db: MemoryDB::new() };
    let consensus = ConsensusConfig {
        round_timeout: 1000,
        status_timeout: 50000,
        peers_timeout: 50000,
        propose_timeout: 200,
        txs_block_limit: 1000,
    };
    let genesis = GenesisConfig::new_with_consensus(consensus, validators.iter().map(|x| x.0));
    blockchain.create_genesis_block(genesis).unwrap();

    let config = Configuration {
        listener: ListenerConfig {
            address: addresses[0].clone(),
            public_key: validators[0].0.clone(),
            secret_key: validators[0].1.clone(),
        },
        network: NetworkConfiguration::default(),
        events: EventsConfiguration::new(),
        peer_discovery: Vec::new(),
    };

    // TODO use factory or other solution like set_handler or run

    let inner = Arc::new(Mutex::new(SandboxInner {
        address: addresses[0].clone(),
        time: blockchain.last_block().unwrap().time(),
        sended: VecDeque::new(),
        events: VecDeque::new(),
        timers: BinaryHeap::new(),
    }));

    let channel = SandboxChannel { inner: inner.clone() };
    let node = NodeHandler::new(blockchain.clone(), channel, config.clone());

    let mut reactor = SandboxReactor {
        inner: inner.clone(),
        handler: node,
    };
    let tx_gen = TimestampingTxGenerator::new(64);

    reactor.handler.initialize();
    let sandbox = Sandbox {
        inner: inner.clone(),
        reactor: RefCell::new(reactor),
        tx_generator: RefCell::new(tx_gen),
        validators: validators,
        addresses: addresses,
    };

    sandbox.initialize();
    assert!(sandbox.propose_timeout() < sandbox.round_timeout()); //general assumption; necessary for correct work of consensus algorithm
    sandbox
}

#[test]
fn test_sandbox_init() {
    timestamping_sandbox();
}

#[test]
fn test_sandbox_recv_and_send() {
    let s = timestamping_sandbox();
    let (public, secret) = gen_keypair();
    s.recv(Connect::new(&public, s.a(2), s.time(), &secret));
    s.send(s.a(2), Connect::new(s.p(0), s.a(0), s.time(), s.s(0)));
}

#[test]
fn test_sandbox_assert_status() {
    // TODO: remove this?
    let s = timestamping_sandbox();
    s.assert_state(1, 1);
    s.add_time(Duration::milliseconds(999));
    s.assert_state(1, 1);
    s.add_time(Duration::milliseconds(1));
    s.assert_state(1, 2);
}

#[test]
#[should_panic(expected = "Expected to send the message")]
fn test_sandbox_expected_to_send_but_nothing_happened() {
    let s = timestamping_sandbox();
    s.send(s.a(1), Connect::new(s.p(0), s.a(0), s.time(), s.s(0)));
}

#[test]
#[should_panic(expected = "Expected to send the message")]
fn test_sandbox_expected_to_send_another_message() {
    let s = timestamping_sandbox();
    let (public, secret) = gen_keypair();
    s.recv(Connect::new(&public, s.a(2), s.time(), &secret));
    s.send(s.a(1), Connect::new(s.p(0), s.a(0), s.time(), s.s(0)));
}

#[test]
#[should_panic(expected = "Send unexpected message")]
fn test_sandbox_unexpected_message_when_drop() {
    let s = timestamping_sandbox();
    let (public, secret) = gen_keypair();
    s.recv(Connect::new(&public, s.a(2), s.time(), &secret));
}

#[test]
#[should_panic(expected = "Send unexpected message")]
fn test_sandbox_unexpected_message_when_handle_another_message() {
    let s = timestamping_sandbox();
    let (public, secret) = gen_keypair();
    s.recv(Connect::new(&public, s.a(2), s.time(), &secret));
    s.recv(Connect::new(&public, s.a(3), s.time(), &secret));
    panic!("Oops! We don't catch unexpected message");
}

#[test]
#[should_panic(expected = "Send unexpected message")]
fn test_sandbox_unexpected_message_when_time_changed() {
    let s = timestamping_sandbox();
    let (public, secret) = gen_keypair();
    s.recv(Connect::new(&public, s.a(2), s.time(), &secret));
    s.add_time(Duration::milliseconds(1000));
    panic!("Oops! We don't catch unexpected message");
}
