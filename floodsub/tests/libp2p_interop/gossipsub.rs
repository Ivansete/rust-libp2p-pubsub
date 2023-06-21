use std::convert::Infallible;
use std::fmt::Debug;
use std::time::Duration;

use assert_matches::assert_matches;
use bytes::Bytes;
use futures::StreamExt;
use libp2p::gossipsub::{
    Behaviour as Libp2pGossipsubBehaviour, Config as Libp2pGossipsubConfig,
    ConfigBuilder as Libp2pGossipsubConfigBuilder, Event as Libp2pGossipsubEvent,
    IdentTopic as Libp2pGossipsubIdentTopic,
    MessageAuthenticity as Libp2pGossipsubMessageAuthenticity,
    ValidationMode as Libp2pGossipsubValidationMode,
};
use libp2p::identity::{Keypair, PeerId};
use libp2p::swarm::{NetworkBehaviour, SwarmBuilder, SwarmEvent};
use libp2p::{Multiaddr, Swarm};
use rand::Rng;
use tokio::time::timeout;
use void::Void;

use floodsub::{Behaviour, Config, Event, IdentTopic};

use crate::testlib;
use crate::testlib::any_memory_addr;
use crate::testlib::keys::{TEST_KEYPAIR_A, TEST_KEYPAIR_B};

fn new_test_topic() -> IdentTopic {
    IdentTopic::new(format!(
        "/pubsub/2/it-pubsub-test-{}",
        rand::thread_rng().gen::<u32>()
    ))
}

fn new_libp2p_topic(raw: &str) -> Libp2pGossipsubIdentTopic {
    Libp2pGossipsubIdentTopic::new(raw)
}

fn new_test_node(keypair: &Keypair, config: Config) -> Swarm<Behaviour> {
    let peer_id = PeerId::from(keypair.public());
    let transport = testlib::test_transport(keypair).expect("create the transport");
    let behaviour = Behaviour::new(config);
    SwarmBuilder::with_tokio_executor(transport, behaviour, peer_id).build()
}

fn new_libp2p_gossipsus_node(
    keypair: &Keypair,
    privacy: Libp2pGossipsubMessageAuthenticity,
    config: Libp2pGossipsubConfig,
) -> Swarm<Libp2pGossipsubBehaviour> {
    let peer_id = PeerId::from(keypair.public());
    let transport = testlib::test_transport(keypair).expect("create the transport");
    let behaviour =
        Libp2pGossipsubBehaviour::new(privacy, config).expect("valid gossipsub configuration");
    SwarmBuilder::with_tokio_executor(transport, behaviour, peer_id).build()
}

async fn poll_nodes<
    B1: NetworkBehaviour<OutEvent = E1>,
    E1: Debug,
    B2: NetworkBehaviour<OutEvent = E2>,
    E2: Debug,
>(
    duration: Duration,
    swarm1: &mut Swarm<B1>,
    swarm2: &mut Swarm<B2>,
) {
    timeout(
        duration,
        futures::future::join(testlib::swarm::poll(swarm1), testlib::swarm::poll(swarm2)),
    )
    .await
    .expect_err("timeout to be reached");
}

async fn wait_for_start_listening<
    B1: NetworkBehaviour<OutEvent = E1>,
    E1: Debug,
    B2: NetworkBehaviour<OutEvent = E2>,
    E2: Debug,
>(
    publisher: &mut Swarm<B1>,
    subscriber: &mut Swarm<B2>,
) -> (Multiaddr, Multiaddr) {
    tokio::join!(
        testlib::swarm::wait_for_new_listen_addr(publisher),
        testlib::swarm::wait_for_new_listen_addr(subscriber)
    )
}

async fn wait_for_connection_establishment<
    B1: NetworkBehaviour<OutEvent = E1>,
    E1: Debug,
    B2: NetworkBehaviour<OutEvent = E2>,
    E2: Debug,
>(
    dialer: &mut Swarm<B1>,
    receiver: &mut Swarm<B2>,
) {
    tokio::join!(
        testlib::swarm::wait_for_connection_established(dialer),
        testlib::swarm::wait_for_connection_established(receiver)
    );
}

async fn wait_for_message_event(
    swarm: &mut Swarm<Behaviour>,
) -> Vec<SwarmEvent<Event, Infallible>> {
    let mut events = Vec::new();

    loop {
        let event = swarm.select_next_some().await;
        log::trace!("Event emitted: {event:?}");
        events.push(event);

        if matches!(
            events.last(),
            Some(SwarmEvent::Behaviour(Event::Message { .. }))
        ) {
            break;
        }
    }

    events
}

async fn wait_for_libp2p_gossipsub_message_event(
    swarm: &mut Swarm<Libp2pGossipsubBehaviour>,
) -> Vec<SwarmEvent<Libp2pGossipsubEvent, Void>> {
    let mut events = Vec::new();

    loop {
        let event = swarm.select_next_some().await;
        log::trace!("Event emitted: {event:?}");
        events.push(event);

        if matches!(
            events.last(),
            Some(SwarmEvent::Behaviour(Libp2pGossipsubEvent::Message { .. }))
        ) {
            break;
        }
    }

    events
}

async fn wait_mesh_message_propagation(
    duration: Duration,
    swarm1: &mut Swarm<Behaviour>,
    swarm2: &mut Swarm<Libp2pGossipsubBehaviour>,
) -> Vec<SwarmEvent<Libp2pGossipsubEvent, Void>> {
    tokio::select! {
        _ = timeout(duration, testlib::swarm::poll(swarm1)) => panic!("timeout reached"),
        res = wait_for_libp2p_gossipsub_message_event(swarm2) => res,
    }
}

async fn wait_mesh_libp2p_gossipsub_message_propagation(
    duration: Duration,
    swarm1: &mut Swarm<Libp2pGossipsubBehaviour>,
    swarm2: &mut Swarm<Behaviour>,
) -> Vec<SwarmEvent<Event, Infallible>> {
    tokio::select! {
        _ = timeout(duration, testlib::swarm::poll(swarm1)) => panic!("timeout reached"),
        res = wait_for_message_event(swarm2) => res,
    }
}

/// Interoperability test where a Floodsub node acts publisher and a Libp2p Gosssipsub Node (with
/// Floodsub support enabled) acts as subscriber.
///
/// The publisher sends a message to the pubsub topic, the subscriber asserts the propagation and
/// reception of the message.
#[tokio::test]
async fn floodsub_node_publish_and_gossipsub_node_subscribes() {
    testlib::init_logger();

    //// Given
    let pubsub_topic = new_test_topic();
    let libp2p_pubsub_topic = new_libp2p_topic(pubsub_topic.hash().as_str());

    let message_payload = Bytes::from_static(b"test-payload");

    let publisher_key = testlib::secp256k1_keypair(TEST_KEYPAIR_A);
    let subscriber_key = testlib::secp256k1_keypair(TEST_KEYPAIR_B);

    let publisher_config = Config::default();
    let subscriber_config = Libp2pGossipsubConfigBuilder::default()
        .validation_mode(Libp2pGossipsubValidationMode::Permissive)
        .support_floodsub()
        .build()
        .expect("valid gossipsub configuration");

    let mut publisher = new_test_node(&publisher_key, publisher_config.clone());
    publisher
        .listen_on(any_memory_addr())
        .expect("listen on address");

    let mut libp2p_subscriber = new_libp2p_gossipsus_node(
        &subscriber_key,
        Libp2pGossipsubMessageAuthenticity::Anonymous,
        subscriber_config.clone(),
    );
    libp2p_subscriber
        .listen_on(any_memory_addr())
        .expect("listen on address");

    let (_publisher_addr, subscriber_addr) = timeout(
        Duration::from_secs(5),
        wait_for_start_listening(&mut publisher, &mut libp2p_subscriber),
    )
    .await
    .expect("listening to start");

    // Subscribe to the topic
    publisher
        .behaviour_mut()
        .subscribe(&pubsub_topic)
        .expect("subscribe to topic");
    libp2p_subscriber
        .behaviour_mut()
        .subscribe(&libp2p_pubsub_topic)
        .expect("subscribe to topic");

    // Dial the publisher node
    publisher.dial(subscriber_addr).expect("dial to succeed");
    timeout(
        Duration::from_secs(5),
        wait_for_connection_establishment(&mut publisher, &mut libp2p_subscriber),
    )
    .await
    .expect("publisher to dial the subscriber");

    poll_nodes(
        Duration::from_millis(50),
        &mut publisher,
        &mut libp2p_subscriber,
    )
    .await;

    //// When
    publisher
        .behaviour_mut()
        .publish(&pubsub_topic, message_payload.clone())
        .expect("publish the message");

    let sub_events = wait_mesh_message_propagation(
        Duration::from_millis(50),
        &mut publisher,
        &mut libp2p_subscriber,
    )
    .await;

    //// Then
    let last_event = sub_events.last().expect("at least one event");
    assert_matches!(last_event, SwarmEvent::Behaviour(Libp2pGossipsubEvent::Message { message, .. }) => {
        assert!(message.sequence_number.is_some());
        assert!(message.source.is_none());
        assert_eq!(message.topic.as_str(), pubsub_topic.hash().as_str());
        assert_eq!(message.data[..], message_payload[..]);
    });
}

/// Interoperability test where a Libp2p Gossipsub node (with Floodsub support enabled) acts
/// publisher and a Floodsub node acts as subscriber.
///
/// The publisher sends a message to the pubsub topic, the subscriber asserts the propagation and
/// reception of the message.
#[tokio::test]
async fn gossipsub_node_publish_and_floodsub_node_subscribes() {
    testlib::init_logger();

    //// Given
    let pubsub_topic = new_test_topic();
    let libp2p_pubsub_topic = new_libp2p_topic(pubsub_topic.hash().as_str());

    let message_payload = Bytes::from_static(b"test-payload");

    let publisher_key = testlib::secp256k1_keypair(TEST_KEYPAIR_A);
    let subscriber_key = testlib::secp256k1_keypair(TEST_KEYPAIR_B);

    let libp2p_publisher_config = Libp2pGossipsubConfigBuilder::default()
        .validation_mode(Libp2pGossipsubValidationMode::Permissive)
        .support_floodsub()
        .build()
        .expect("valid gossipsub configuration");
    let subscriber_config = Config::default();

    let mut libp2p_publisher = new_libp2p_gossipsus_node(
        &subscriber_key,
        Libp2pGossipsubMessageAuthenticity::Anonymous,
        libp2p_publisher_config.clone(),
    );
    libp2p_publisher
        .listen_on(any_memory_addr())
        .expect("listen on address");
    let mut subscriber = new_test_node(&publisher_key, subscriber_config.clone());
    subscriber
        .listen_on(any_memory_addr())
        .expect("listen on address");

    let (libp2p_publisher_addr, _subscriber_addr) = timeout(
        Duration::from_secs(5),
        wait_for_start_listening(&mut libp2p_publisher, &mut subscriber),
    )
    .await
    .expect("listening to start");

    // Subscribe to the topic
    libp2p_publisher
        .behaviour_mut()
        .subscribe(&libp2p_pubsub_topic)
        .expect("subscribe to topic");
    subscriber
        .behaviour_mut()
        .subscribe(&pubsub_topic)
        .expect("subscribe to topic");

    // Dial the publisher node
    subscriber
        .dial(libp2p_publisher_addr)
        .expect("dial to succeed");
    timeout(
        Duration::from_secs(5),
        wait_for_connection_establishment(&mut subscriber, &mut libp2p_publisher),
    )
    .await
    .expect("publisher to dial the subscriber");

    poll_nodes(
        Duration::from_millis(50),
        &mut subscriber,
        &mut libp2p_publisher,
    )
    .await;

    //// When
    libp2p_publisher
        .behaviour_mut()
        .publish(libp2p_pubsub_topic.hash(), message_payload.clone())
        .expect("publish the message");

    let sub_events = wait_mesh_libp2p_gossipsub_message_propagation(
        Duration::from_millis(50),
        &mut libp2p_publisher,
        &mut subscriber,
    )
    .await;

    //// Then
    let last_event = sub_events.last().expect("at least one event");
    assert_matches!(last_event, SwarmEvent::Behaviour(Event::Message { message, .. }) => {
        assert!(message.sequence_number().is_none());
        assert!(message.source().is_none());
        assert_eq!(message.topic_str(), pubsub_topic.hash().as_str());
        assert_eq!(message.data()[..], message_payload[..]);
    });
}
