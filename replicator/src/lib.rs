///! Replicate a set of hypercores over a set of connections
use anyhow::Result;
use async_std::sync::{Arc, Mutex};
use async_std::task;
use async_std::task::JoinHandle;
use futures::channel::mpsc::{
    unbounded as channel, UnboundedReceiver as Receiver, UnboundedSender as Sender,
};
use futures::future::FutureExt;
use futures::io::{AsyncRead, AsyncWrite};
use futures::prelude::*;
use futures::stream::StreamExt;
use hypercore::Feed;
use hypercore_protocol::{Event as ProtocolEvent, Protocol, ProtocolBuilder};

pub use hypercore_protocol::{discovery_key, DiscoveryKey, Key};
use log::*;
use std::collections::HashMap;

mod peer;
pub use peer::{Peer, PeeredFeed, Stats};

pub type RemotePublicKey = [u8; 32];

#[derive(Clone, Debug)]
pub enum ReplicatorEvent {
    Feed(DiscoveryKey),
    DiscoveryKey(DiscoveryKey),
}

#[derive(Debug)]
enum Event {
    Protocol(ProtocolEvent),
    Replicator(ReplicatorEvent),
    Error(anyhow::Error),
}

/// A replicator for hypercore feeds
#[derive(Clone)]
pub struct Replicator {
    feeds: Arc<Mutex<HashMap<DiscoveryKey, PeeredFeed>>>,
    subscribers: Arc<Mutex<Vec<Sender<ReplicatorEvent>>>>,
    tasks: Arc<Mutex<Vec<JoinHandle<Result<()>>>>>,
}

impl Replicator {
    /// Create a new replicator
    pub fn new() -> Self {
        Self {
            feeds: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(vec![])),
            subscribers: Arc::new(Mutex::new(vec![])),
        }
    }

    /// Add a feed to the replicator
    pub async fn add_feed(&mut self, feed: Arc<Mutex<Feed>>) {
        let key = feed.lock().await.public_key().as_bytes().to_vec();
        let dkey = discovery_key(&key);
        let peered_feed = PeeredFeed::new(feed);
        let mut feeds = self.feeds.lock().await;
        feeds.insert(dkey.clone(), peered_feed);
        self.emit(ReplicatorEvent::Feed(dkey)).await;
    }

    async fn emit(&self, event: ReplicatorEvent) {
        let mut subscribers = self.subscribers.lock().await;
        let futs = subscribers.iter_mut().map(|s| s.send(event.clone()));
        let _ = futures::future::join_all(futs).await;
    }

    /// Subscribe to events on the replicator
    pub async fn subscribe(&mut self) -> Receiver<ReplicatorEvent> {
        let (send, recv) = channel();
        self.subscribers.lock().await.push(send);
        recv
    }

    /// Add a new connection to the replicator
    pub async fn add_stream<S>(&mut self, stream: S, is_initiator: bool)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let proto = ProtocolBuilder::new(is_initiator).connect(stream);
        self.run_peer(proto).await;
    }

    pub async fn add_io<R, W>(&mut self, reader: R, writer: W, is_initiator: bool)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let proto = ProtocolBuilder::new(is_initiator).connect_rw(reader, writer);
        self.run_peer(proto).await;
    }

    /// Get stats on the replication status for each feed
    pub async fn stats(&mut self) -> Vec<(DiscoveryKey, Vec<Stats>)> {
        let mut feeds = self.feeds.lock().await;
        let futs = feeds
            .iter_mut()
            .map(|(dkey, peered_feed)| peered_feed.stats().map(move |stats| (*dkey, stats)));
        let stats = futures::future::join_all(futs).await;
        stats
    }

    /// Wait for all connections and feeds to close
    pub async fn join_all(&mut self) -> Result<()> {
        let mut feeds = self.feeds.lock().await;
        let futs = feeds.values_mut().map(|f| f.join_all());
        futures::future::join_all(futs).await;
        Ok(())
    }

    async fn run_peer<IO>(&mut self, proto: Protocol<IO>)
    where
        IO: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let mut this = self.clone();
        let task = task::spawn(async move {
            let replicator_events = this.subscribe().await.map(|e| Event::Replicator(e));
            let mut proto_command = proto.commands();
            let proto_events = proto.map(|e| match e {
                Ok(ev) => Event::Protocol(ev),
                Err(err) => Event::Error(err.into()),
            });
            let mut events = futures::stream::select(proto_events, replicator_events);

            let mut remote_public_key = None;
            while let Some(event) = events.next().await {
                debug!("incoming event {:?}", event);
                match event {
                    Event::Protocol(ProtocolEvent::Handshake(key)) => {
                        remote_public_key = Some(key);

                        let feeds = this.feeds.lock().await;
                        for peered_feed in feeds.values() {
                            let feed = peered_feed.feed.lock().await;
                            let public_key = feed.public_key().to_bytes();
                            proto_command.open(public_key).await.unwrap();
                        }
                    }
                    Event::Protocol(ProtocolEvent::Channel(channel)) => {
                        let mut feeds = this.feeds.lock().await;
                        if let Some(peered_feed) = feeds.get_mut(channel.discovery_key()) {
                            let remote_public_key = remote_public_key.clone().unwrap();
                            peered_feed.add_peer(remote_public_key, channel).await;
                        } else {
                        }
                    }
                    Event::Protocol(ProtocolEvent::DiscoveryKey(dkey)) => {
                        this.emit(ReplicatorEvent::DiscoveryKey(dkey)).await;
                    }
                    Event::Error(err) => {
                        error!("protocol error: {:?}", err);
                        return Err(err);
                    }
                    Event::Replicator(ReplicatorEvent::Feed(key)) => {
                        if let Some(_) = remote_public_key {
                            proto_command.open(key).await.unwrap();
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        });
        self.tasks.lock().await.push(task);
    }
}
