//! Client proxy nodes that expose a GET/PUT-based interface to users.

pub use self::interactive::run_interactive;
use anna_api::{
    messages::{
        request::KeyOperation,
        response::{ClientResponseValue, ResponseTuple},
        AddressRequest, AddressResponse, Request, Response, TcpMessage,
    },
    topics::{ClientThread, KvsThread, RoutingThread},
    AnnaError, ClientKey,
};
use client_request::{ClientRequest, PendingRequest};
use eyre::{anyhow, bail, eyre, Context, ContextCompat};
use futures::{
    stream::{self, FusedStream, FuturesUnordered},
    AsyncReadExt, Future, FutureExt, Stream, StreamExt, TryStreamExt,
};
use rand::prelude::IteratorRandom;
use smol::{channel, io::AsyncWriteExt, net::TcpStream};
use std::{
    collections::{HashMap, HashSet},
    iter::Extend,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use zenoh::prelude::SplitBuffer;

mod client_request;
mod interactive;

/// Receives a [`TcpMessage`] from the given stream.
///
/// This function requires that all messages are sent using [`send_tcp_message`],
/// otherwise parsing the messages will fail.
pub async fn receive_tcp_message(
    stream: &mut (impl futures::AsyncRead + Unpin),
) -> eyre::Result<Option<TcpMessage>> {
    const MAX_MSG_LEN: u64 = u32::MAX as u64;

    let mut len_raw = [0; 8];
    if let Err(err) = stream.read_exact(&mut len_raw).await {
        if err.kind() == std::io::ErrorKind::UnexpectedEof
            || err.kind() == std::io::ErrorKind::ConnectionReset
        {
            return Ok(None);
        } else {
            return Err(eyre::Error::new(err).wrap_err("failed to read message length"));
        }
    }
    let len = u64::from_le_bytes(len_raw);

    if len > MAX_MSG_LEN {
        bail!("Message is too long (length: {} bytes)", len);
    }

    let mut buf = vec![0; len.try_into().unwrap()];
    if let Err(err) = stream.read_exact(&mut buf).await {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            log::warn!("receive tcp message failed: {}", err);
            return Ok(None);
        } else {
            return Err(eyre::Error::new(err).wrap_err("failed to read message"));
        }
    }
    rmp_serde::from_slice(&buf)
        .with_context(|| {
            format!(
                "failed to deserialize message: `{}`",
                String::from_utf8_lossy(&buf)
            )
        })
        .map(Some)
}

/// Sends the given message on the given tcp stream.
///
/// TCP messages should only be sent using this method, to ensure that all
/// messages are sent in the same format.
pub async fn send_tcp_message(
    message: &TcpMessage,
    connection: &mut TcpStream,
) -> eyre::Result<()> {
    let serialized =
        rmp_serde::to_vec_named(&message).context("failed to serialize tcp message")?;
    let len = (serialized.len() as u64).to_le_bytes();
    connection
        .write_all(&len)
        .await
        .context("failed to send message length")?;
    connection
        .write_all(&serialized)
        .await
        .context("failed to send message")?;
    Ok(())
}

/// Client nodes interact with KVS nodes to serve user requests.
///
/// This client proxy provides GET/SET commands with different consistency levels, which
/// abstract over the lattice types that are used behind the scenes.
pub struct ClientNode {
    /// The workspace used for communicating with KVS and routing nodes.
    zenoh: Arc<zenoh::Session>,
    zenoh_prefix: String,

    /// Stream of incoming [`AddressResponse`] messages that are sent to this client node.
    address_responses: channel::Receiver<zenoh::prelude::Sample>,
    /// Stream of incoming [`Response`] messages that are sent to this client node.
    responses: channel::Receiver<zenoh::prelude::Sample>,

    incoming_tcp_messages: Pin<Box<dyn TcpMessageStream>>,

    receive_tasks: FuturesUnordered<Pin<Box<dyn Future<Output = eyre::Result<()>> + Send + Sync>>>,

    /// The node and thread ID of this client node.
    ///
    /// Allows to determine the zenoh topics that map to the [`Self::address_response_stream`]
    /// and [`Self::response_stream`].
    ut: ClientThread,

    /// The configured set ouf routing nodes that should be used for querying addresses.
    ///
    /// The client node will send [`AddressRequest`] messages to these routing nodes.
    routing_threads: Vec<RoutingThread>,
    routing_thread_connections: HashMap<RoutingThread, TcpStream>,

    /// Keeps track of the KVS node threads that are responsible for each key.
    key_address_cache: HashMap<ClientKey, HashSet<KvsThread>>,

    /// Buffers requests that cannot be sent yet because we're still missing the target address.
    ///
    /// Requests are added to this map if we don't know any KVS node that is responsible for
    /// their key. As soon as we receive this information from a routing node, we send the
    /// requests out.
    pending_requests: HashMap<ClientKey, (Instant, Vec<ClientRequest>)>,

    /// Keeps track of requests that we haven't received a reply to yet.
    pending_responses: HashMap<String, PendingRequest>,

    /// Counter that is used for creating unique request IDs in [`Self::generate_request_id`].
    request_id: u32,

    /// Configured timeout after which a pending request/response should be considered as failed.
    timeout: Duration,

    node_connections: HashMap<KvsThread, TcpStream>,
    tcp_incoming: stream::SelectAll<
        Pin<Box<dyn Stream<Item = eyre::Result<TcpMessage>> + Unpin + Send + Sync>>,
    >,
}

impl ClientNode {
    /// Creates a new client node.
    ///
    /// The following arguments are expected:
    ///
    /// - The `node_id` and `thread_id` arguments are used for uniqely identifying the new client
    ///   node.
    /// - The given `routing_threads` list specifies the routing nodes that should be used for
    ///   querying the KVS nodes that are responsible for each key.
    /// - The `timeout` duration specifies after which period pending requests and responses
    ///   should be considered as failed.
    /// - All communication with the routing and KVS nodes is done using the given
    ///   `zenoh` session, prefixed with the given prefix.
    /// - Status messages, return values, and input prompts are written to the given `stdout`
    ///   handle. The `stderr` handle is used for writing error messages.
    pub fn new(
        node_id: String,
        thread_id: u32,
        routing_threads: Vec<RoutingThread>,
        timeout: Duration,
        zenoh: Arc<zenoh::Session>,
        zenoh_prefix: String,
    ) -> eyre::Result<Self> {
        let ut = ClientThread::new(node_id, thread_id);

        let receive_tasks: FuturesUnordered<
            Pin<Box<dyn Future<Output = eyre::Result<()>> + Send + Sync>>,
        > = FuturesUnordered::new();

        let subscribe_to = |topic: String| {
            let zenoh = zenoh.clone();
            let (tx, rx) = channel::bounded(10);
            receive_tasks.push(Box::pin(async move {
                let mut changes = zenoh
                    .subscribe(topic)
                    .await
                    .map_err(|e| eyre!(e))
                    .context("failed to declare subscriber")?;

                loop {
                    let change = match changes.receiver().next().await {
                        Some(c) => c,
                        None => break,
                    };
                    if tx.send(change).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }));
            rx
        };

        let responses = subscribe_to(ut.response_topic(&zenoh_prefix));
        let address_responses = subscribe_to(ut.address_response_topic(&zenoh_prefix));

        Ok(ClientNode {
            zenoh,
            zenoh_prefix,
            address_responses,
            responses,
            receive_tasks,

            ut,

            key_address_cache: HashMap::new(),
            routing_threads,
            routing_thread_connections: Default::default(),
            incoming_tcp_messages: Box::pin(stream::empty()),

            pending_requests: Default::default(),
            pending_responses: Default::default(),
            request_id: 0,
            timeout,

            node_connections: Default::default(),
            tcp_incoming: stream::SelectAll::new(),
        })
    }

    /// Opens TCP connections to all routing nodes with known public socket addresses.
    ///
    /// This allows the client to communicate with the routing nodes more efficiently.
    /// Compared to the default communication through `zenoh`, the latency of requests
    /// is reduced significantly when using direct TCP connections.
    pub async fn init_tcp_connections(&mut self) -> eyre::Result<()> {
        let zenoh = self.zenoh.clone();
        let tasks = FuturesUnordered::new();

        for routing_thread in self.routing_threads.clone() {
            tasks.push(async {
                let reply = loop {
                    let topic = routing_thread.tcp_addr_topic(&self.zenoh_prefix);
                    let mut receiver = zenoh
                        .get(&topic)
                        .await
                        .map_err(|e| eyre!(e))
                        .context("failed to query tcp address of routing thread")?;
                    match receiver.next().await {
                        Some(reply) => break reply.sample.value,
                        None => {
                            log::info!("failed to receive tcp address reply");
                            futures_timer::Delay::new(Duration::from_secs(1)).await;
                        }
                    }
                };

                let reply_buf = reply.payload.contiguous();
                let parsed: Option<smol::net::SocketAddr> = rmp_serde::from_slice(&reply_buf)
                    .context("failed to deserialize tcp addr reply")?;
                let connection = match parsed {
                    Some(addr) => {
                        let connection = TcpStream::connect(addr)
                            .await
                            .context("failed to connect to tcp stream")?;
                        connection
                            .set_nodelay(true)
                            .context("failed to set nodelay for tcpstream")?;
                        Some(connection)
                    }
                    None => None,
                };

                Result::<_, eyre::Error>::Ok((routing_thread, connection))
            });
        }

        self.routing_thread_connections = tasks
            .try_filter_map(|(thread, connection)| async { Ok(connection.map(|c| (thread, c))) })
            .try_collect()
            .await?;

        self.incoming_tcp_messages = Box::pin(stream::select_all(
            self.routing_thread_connections
                .values()
                .cloned()
                .map(|stream| {
                    stream::try_unfold(stream, |mut stream| {
                        Box::pin(async {
                            let message = receive_tcp_message(&mut stream).await?;
                            Result::<_, eyre::Error>::Ok(message.map(|m| (m, stream)))
                        })
                    })
                }),
        ));

        Ok(())
    }

    /// Returns the value stored for the given key.
    ///
    /// Awaits until the KVS sends the requested value.
    ///
    /// To only send out the `GET` request without waiting for acknowledgement use the
    /// [`Self::get_async`] method.
    ///
    /// **Note:** This method drops all unknown responses, so be careful when using this method
    /// together with the `*_async` methods.
    pub async fn get(&mut self, key: ClientKey) -> eyre::Result<ClientResponseValue> {
        let request_id = self.get_async(key).await?;
        let tuple = self.wait_for_matching_response(request_id).await?;

        tuple
            .lattice
            .ok_or_else(|| anyhow!("response has no lattice value in tuple"))
    }

    /// Starts a request to add the given [`Vec<u8>`] to the given key, that value is a [`LastWriteWinlattice<Vec<u8>>`][crate::lattice::LastWriterWinsLattice].
    ///
    /// Returns the ID of the request, which can be used to find the matching response.
    ///
    /// The request might not be sent immediately. This happens when the client node does not
    /// which KVS nodes are responsible for the key. In this case, it sends a [`AddressRequest`]
    /// message to one of the configured routing nodes first.
    ///
    /// Each `PUT` request is acknowledged by the KVS node with a [`Response`] message. To receive
    /// this response, the [`Self::wait_for_matching_response`] or [`Self::receive_async`]
    /// function can be used.
    pub async fn put_lww_async(&mut self, key: ClientKey, bytes: Vec<u8>) -> eyre::Result<String> {
        let request_id = self.generate_request_id();
        let request = ClientRequest {
            operation: KeyOperation::Put(key, bytes),
            response_address: self.ut.response_topic(&self.zenoh_prefix).to_string(),
            request_id: request_id.clone(),
            address_cache_size: HashMap::new(),
            timestamp: Instant::now(),
        };

        self.try_request(request).await?;

        Ok(request_id)
    }

    /// Starts a request to increase the given [`i64`] to the given key, that value is a [`CounterLattice`][crate::lattice::CounterLattice].
    ///
    /// Returns the ID of the request, which can be used to find the matching response.
    ///
    /// The request might not be sent immediately. This happens when the client node does not
    /// which KVS nodes are responsible for the key. In this case, it sends a [`AddressRequest`]
    /// message to one of the configured routing nodes first.
    ///
    /// Each `PUT` request is acknowledged by the KVS node with a [`Response`] message. To receive
    /// this response, the [`Self::wait_for_matching_response`] or [`Self::receive_async`]
    /// function can be used.
    pub async fn inc_async(&mut self, key: ClientKey, value: i64) -> eyre::Result<String> {
        let request_id = self.generate_request_id();
        let request = ClientRequest {
            operation: KeyOperation::Inc(key, value),
            response_address: self.ut.response_topic(&self.zenoh_prefix).to_string(),
            request_id: request_id.clone(),
            address_cache_size: HashMap::new(),
            timestamp: Instant::now(),
        };

        self.try_request(request).await?;

        Ok(request_id)
    }

    /// Starts a request to add the given [`HashSet<Vec<u8>>`] to the given key, that value is a Set.
    ///
    /// Returns the ID of the request, which can be used to find the matching response.
    ///
    /// The request might not be sent immediately. This happens when the client node does not
    /// which KVS nodes are responsible for the key. In this case, it sends a [`AddressRequest`]
    /// message to one of the configured routing nodes first.
    ///
    /// Each `PUT` request is acknowledged by the KVS node with a [`Response`] message. To receive
    /// this response, the [`Self::wait_for_matching_response`] or [`Self::receive_async`]
    /// function can be used.
    pub async fn add_set_async(
        &mut self,
        key: ClientKey,
        lattice: HashSet<Vec<u8>>,
    ) -> eyre::Result<String> {
        let request_id = self.generate_request_id();
        let request = ClientRequest {
            operation: KeyOperation::SetAdd(key, lattice),
            response_address: self.ut.response_topic(&self.zenoh_prefix).to_string(),
            request_id: request_id.clone(),
            address_cache_size: HashMap::new(),
            timestamp: Instant::now(),
        };

        self.try_request(request).await?;

        Ok(request_id)
    }

    /// Starts a request to add the given `lattice` to the given key.
    ///
    /// Returns the ID of the request, which can be used to find the matching response.
    ///
    /// The request might not be sent immediately. This happens when the client node does not
    /// which KVS nodes are responsible for the key. In this case, it sends a [`AddressRequest`]
    /// message to one of the configured routing nodes first.
    ///
    /// Each `PUT` request is acknowledged by the KVS node with a [`Response`] message. To receive
    /// this response, the [`Self::wait_for_matching_response`] or [`Self::receive_async`]
    /// function can be used.
    pub async fn add_map_async(
        &mut self,
        key: ClientKey,
        lattice: HashMap<String, Vec<u8>>,
    ) -> eyre::Result<String> {
        let request_id = self.generate_request_id();
        let request = ClientRequest {
            operation: KeyOperation::MapAdd(key, lattice),
            response_address: self.ut.response_topic(&self.zenoh_prefix).to_string(),
            request_id: request_id.clone(),
            address_cache_size: HashMap::new(),
            timestamp: Instant::now(),
        };

        self.try_request(request).await?;

        Ok(request_id)
    }

    /// Requests the value stored for the given key.
    ///
    /// Returns the ID of the request, which can be used to find the matching response.
    ///
    /// The request might not be sent immediately. This happens when the client node does not
    /// which KVS nodes are responsible for the key. In this case, it sends a [`AddressRequest`]
    /// message to one of the configured routing nodes first.
    ///
    /// The result is sent in an asynchronous [`Response`] message. To receive this
    /// response, the the [`Self::wait_for_matching_response`] or [`Self::receive_async`]
    /// function can be used.
    pub async fn get_async(&mut self, key: ClientKey) -> eyre::Result<String> {
        let request_id = self.generate_request_id();
        let request = ClientRequest {
            operation: KeyOperation::Get(key),
            response_address: self.ut.response_topic(&self.zenoh_prefix).to_string(),
            request_id: request_id.clone(),
            address_cache_size: HashMap::new(),
            timestamp: Instant::now(),
        };
        self.try_request(request).await?;

        Ok(request_id)
    }

    /// Awaits a single [`Response`] or [`AddressResponse`] message.
    ///
    /// In addition to the received response, the returned list might also contain
    /// responses to requests that timed out. Received [`AddressResponse`]s are handled
    /// internally (by issuing buffered pending requests) and not added to the list, so
    /// an empty return list is possible too.
    ///
    /// When waiting for a specific response, this method is typically called repeatedly
    /// until the desired response was received (or a timeout occured).
    pub async fn receive_async(&mut self) -> eyre::Result<Vec<Response>> {
        let mut results = Vec::new();
        let mut timeout = futures_timer::Delay::new(Duration::from_secs(3)).fuse();

        futures::select! {
            message = self.incoming_tcp_messages.select_next_some() => {
                self.handle_tcp_message(message?, &mut results).await?;
            }
            message = self.tcp_incoming.select_next_some() => {
                self.handle_tcp_message(message?, &mut results).await?;
            }
            sample = self.address_responses.select_next_some() => {
                let serialized = sample.value.payload.contiguous();

                let response: AddressResponse = rmp_serde::from_slice(&serialized)
                    .context("failed to deserialize KeyAddressResponse")?;

                self.handle_address_response(response).await?;
            },
            sample = self.responses.select_next_some() => {
                let serialized = sample.value.payload.contiguous();
                let response: Response =
                    rmp_serde::from_slice(&serialized).context("failed to deserialize KeyResponse")?;
                results.extend(self.handle_response(response).await?);
            },
            task_result = self.receive_tasks.select_next_some() => {
                task_result?;
            },
            () = timeout => {
                // query routing info again for requests that have been waiting for too long
                let now = Instant::now();
                let waiting_keys = self.pending_requests.iter_mut().filter(|(_k, p)| {
                    (now - p.0) > Duration::from_secs(3)
                }).map(|(k, _p)| k.to_owned()).collect::<Vec<_>>();
                for key in waiting_keys {
                    if let Some(pending) = self.pending_requests.get_mut(&key) {
                        pending.0 = Instant::now(); // update timestamp
                    }
                    self.query_routing_async(key).await?;
                }
            },
        }

        // GC the pending request map
        let mut to_remove = HashSet::new();
        for (key, (time, requests)) in &self.pending_requests {
            if Instant::now() - *time > self.timeout {
                // query to the routing tier timed out
                for req in requests {
                    results.push(generate_bad_response(&req.clone().into()));
                }

                to_remove.insert(key.clone());
            }
        }

        for key in to_remove {
            self.pending_requests.remove(&key);
        }

        // GC the pending get response map
        let mut to_remove = HashSet::new();
        for (request_id, req) in &self.pending_responses {
            if Instant::now() - req.tp > self.timeout {
                // query to server timed out
                results.push(generate_bad_response(&req.request.clone().into()));
                to_remove.insert(request_id.clone());
                Self::invalidate_cache_for_worker(&mut self.key_address_cache, &req.node);
            }
        }

        for request_id in to_remove {
            self.pending_responses.remove(&request_id);
        }

        Ok(results)
    }

    async fn handle_tcp_message(
        &mut self,
        message: TcpMessage,
        results: &mut Vec<Response>,
    ) -> eyre::Result<()> {
        match message {
            TcpMessage::AddressResponse(response) => self.handle_address_response(response).await,
            TcpMessage::Response(response) => {
                results.extend(self.handle_response(response).await?);
                Ok(())
            }
            other => bail!("unexpected tcp message {:?}", other),
        }
    }
    async fn handle_address_response(&mut self, response: AddressResponse) -> eyre::Result<()> {
        if let Some(err) = response.error {
            match err {
                AnnaError::NoServers => {
                    log::error!("No servers have joined the cluster yet. Retrying request.");

                    futures_timer::Delay::new(Duration::from_secs(1)).await;

                    for key in response.addresses.into_iter().map(|a| a.key) {
                        if let Some(pending) = self.pending_requests.get_mut(&key) {
                            pending.0 = Instant::now();
                        }
                        self.query_routing_async(key).await?;
                    }
                }
                other => log::error!("Unexpected error `{:?}` in key address response", other),
            }
        } else {
            for (kvs_thread, socket) in response.tcp_sockets {
                match self.node_connections.entry(kvs_thread) {
                    std::collections::hash_map::Entry::Occupied(_) => {} // already connected
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let connection = TcpStream::connect(socket)
                            .await
                            .context("failed to connect to tcp stream")?;
                        connection
                            .set_nodelay(true)
                            .context("failed to set nodelay for tcpstream")?;
                        entry.insert(connection.clone());

                        let unfold_stream = |mut stream: TcpStream| {
                            Box::pin(async {
                                let message = receive_tcp_message(&mut stream).await?;
                                Result::<_, eyre::Error>::Ok(message.map(|m| (m, stream)))
                            })
                        };
                        self.tcp_incoming
                            .push(Box::pin(stream::try_unfold(connection, unfold_stream)));
                    }
                }
            }

            for entry in response.addresses {
                let key = entry.key;

                // populate cache
                for node in entry.nodes {
                    self.key_address_cache
                        .entry(key.clone())
                        .or_default()
                        .insert(node);
                }

                // handle stuff in pending request map
                if let Some((_, pending_requests)) = self.pending_requests.remove(&key) {
                    for req in pending_requests {
                        log::trace!(
                            "Retrying request `{}` after receiving worker thread info (after {:?})",
                            req.request_id,
                            Instant::now() - req.timestamp,
                        );
                        self.try_request(req).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn get_lww(&mut self, key: ClientKey) -> eyre::Result<Vec<u8>> {
        let lattice = self.get(key).await?;
        match lattice {
            ClientResponseValue::Bytes(bytes) => Ok(bytes),
            other => Err(anyhow!("expected a bytes, got `{:?}`", other)),
        }
    }

    async fn put_lww(&mut self, key: ClientKey, value: Vec<u8>) -> eyre::Result<()> {
        self.put_lww_async(key, value)
            .await
            .context("failed to send put_lww")?;
        Ok(())
    }

    async fn inc(&mut self, key: ClientKey, value: i64) -> eyre::Result<i64> {
        let request_id = self
            .inc_async(key, value)
            .await
            .context("failed to send inc")?;
        let tuple = self.wait_for_matching_response(request_id).await?;
        let response = tuple
            .lattice
            .ok_or_else(|| anyhow!("response has no lattice value in tuple"))?;
        match response {
            ClientResponseValue::Int(n) => Ok(n),
            other => Err(anyhow!("expected a int, got `{:?}`", other)),
        }
    }

    async fn get_set(&mut self, key: ClientKey) -> eyre::Result<HashSet<Vec<u8>>> {
        let lattice = self.get(key).await?;

        match lattice {
            ClientResponseValue::Set(set) => Ok(set),
            other => Err(anyhow!("expected a set, got `{:?}`", other)),
        }
    }

    async fn add_set(&mut self, key: ClientKey, set: HashSet<Vec<u8>>) -> eyre::Result<()> {
        let request_id = self
            .add_set_async(key, set)
            .await
            .context("failed to send add_set")?;
        self.wait_for_matching_response(request_id).await?;

        Ok(())
    }

    async fn get_map(&mut self, key: ClientKey) -> eyre::Result<HashMap<String, Vec<u8>>> {
        let lattice = self.get(key).await?;

        match lattice {
            ClientResponseValue::Map(map) => Ok(map),
            other => Err(anyhow!("expected a map, got `{:?}`", other)),
        }
    }

    async fn add_map(&mut self, key: ClientKey, map: HashMap<String, Vec<u8>>) -> eyre::Result<()> {
        let request_id = self
            .add_map_async(key, map)
            .await
            .context("failed to send add_map")?;
        self.wait_for_matching_response(request_id).await?;

        Ok(())
    }

    /// Awaits until a [`Response`] message with the given `request_id` is received.
    ///
    /// **Drops all other received messages.** Use [`Self::receive_async`] to handle all
    /// received messages.
    pub async fn wait_for_matching_response(
        &mut self,
        request_id: String,
    ) -> eyre::Result<ResponseTuple> {
        let response = loop {
            let responses = self
                .receive_async()
                .await
                .context("failed to receive put reply")?;

            let response = match responses.as_slice() {
                [] => continue,
                [response] => response,
                _ => bail!("multiple responses received for GET"),
            };

            if response.response_id.as_ref() == Some(&request_id) {
                break response.clone();
            } else {
                log::warn!(
                    "Ignoring response with unexpected ID `{}`",
                    response.response_id.as_deref().unwrap_or("<none>")
                );
            }
        };
        response.error?;

        let tuple = response
            .tuples
            .get(0)
            .ok_or_else(|| anyhow!("response has no tuples"))?;
        tuple.error.map(Err).unwrap_or(Ok(()))?;

        Ok(tuple.clone())
    }

    async fn handle_response(&mut self, response: Response) -> eyre::Result<Option<Response>> {
        let mut result = None;

        if let Some(response_id) = &response.response_id {
            if let Some(pending) = self.pending_responses.get_mut(response_id) {
                if check_tuple(response.tuples.get(0).unwrap(), &mut self.key_address_cache) {
                    // error no == 2, so re-issue request
                    pending.tp = Instant::now();
                    let request = pending.request.clone();
                    self.try_request(request).await?;
                } else {
                    // error no == 0 or 1
                    self.pending_responses.remove(response_id);
                    result = Some(response);
                }
            } else {
                log::info!("Ignoring response with unknown ID `{}`", response_id);
            }
        } else {
            log::warn!("Ignoring response with no ID");
        }

        Ok(result)
    }

    async fn try_request(&mut self, mut request: ClientRequest) -> eyre::Result<()> {
        let key = request.operation.key();
        // we only get NULL back for the worker thread if the query to the routing
        // tier timed out, which should never happen.
        let worker = match self
            .get_worker_thread(&key)
            .await
            .context("failed to get worker thread")?
        {
            Some(worker) => worker.clone(),
            None => {
                // this means a key addr request is issued asynchronously
                if let Some((_, pending)) = self.pending_requests.get_mut(&key) {
                    pending.push(request.clone());
                } else {
                    self.pending_requests
                        .insert(key.clone(), (Instant::now(), vec![request.clone()]));
                }

                return Ok(());
            }
        };

        request
            .address_cache_size
            .insert(key.clone(), self.key_address_cache[&key].len());

        self.send_request(&worker, &request.clone().into())
            .await
            .context("failed to send request")?;

        self.pending_responses.insert(
            request.request_id.clone(),
            PendingRequest {
                tp: Instant::now(),
                node: worker,
                request: request.clone(),
            },
        );

        Ok(())
    }

    async fn get_worker_thread(&mut self, key: &ClientKey) -> eyre::Result<Option<&KvsThread>> {
        let mut rng = rand::thread_rng();
        Ok(self
            .get_all_worker_threads(key)
            .await
            .context("failed to get worker threads")?
            .iter()
            .choose(&mut rng))
    }

    async fn get_all_worker_threads(
        &mut self,
        key: &ClientKey,
    ) -> eyre::Result<&HashSet<KvsThread>> {
        if let Some(set) = self.key_address_cache.get(key) {
            if !set.is_empty() {
                // reborrow here work around borrow checker limitations (self remains borrowed
                // for the whole function otherwise)
                let set = self.key_address_cache.get(key).unwrap();
                return Ok(set);
            }
        }

        self.query_routing_async(key.clone()).await?;

        Ok(self.key_address_cache.entry(key.clone()).or_default())
    }

    fn get_routing_thread(&self) -> Option<&RoutingThread> {
        let mut rng = rand::thread_rng();
        self.routing_threads.iter().choose(&mut rng)
    }

    async fn send_request(&mut self, target: &KvsThread, request: &Request) -> eyre::Result<()> {
        if let Some(connection) = self.node_connections.get_mut(target) {
            send_tcp_message(&TcpMessage::Request(request.clone()), connection).await
        } else {
            self.zenoh
                .put(
                    &target.request_topic(&self.zenoh_prefix),
                    rmp_serde::to_vec_named(request).context("failed to serialize Request")?,
                )
                .await
                .map_err(|e| eyre!(e))
                .context("zenoh put failed")
        }
    }

    fn generate_request_id(&mut self) -> String {
        self.request_id = (self.request_id + 1) % 10000;
        format!(
            "{}:{}_{}",
            self.ut.node_id, self.ut.thread_id, self.request_id
        )
    }

    /// Send a query to the routing tier asynchronously.
    async fn query_routing_async(&mut self, key: ClientKey) -> eyre::Result<()> {
        // populate request with response address, request id, etc.
        let request = AddressRequest {
            request_id: self.generate_request_id(),
            response_address: self
                .ut
                .address_response_topic(&self.zenoh_prefix)
                .to_string(),
            keys: vec![key.clone()],
        };

        let rt_thread = self
            .get_routing_thread()
            .context("no routing threads")?
            .clone();

        if let Some(connection) = self.routing_thread_connections.get_mut(&rt_thread) {
            send_tcp_message(&TcpMessage::AddressRequest(request), connection).await?;
        } else {
            let serialized = rmp_serde::to_vec_named(&request)
                .context("failed to serialize KeyAddressRequest")?;
            self.zenoh
                .put(
                    &rt_thread.address_request_topic(&self.zenoh_prefix),
                    serialized,
                )
                .await
                .map_err(|e| eyre!(e))?;
        }

        Ok(())
    }

    /// Invalidate the key caches for any key that previously had this worker node in
    /// its cache. The underlying assumption is that if the worker timed out, it
    /// might have failed, and so we don't want to rely on it being alive for both
    /// the key we were querying and any other key.
    fn invalidate_cache_for_worker(
        key_address_cache: &mut HashMap<ClientKey, HashSet<KvsThread>>,
        worker: &KvsThread,
    ) {
        for addresses in key_address_cache.values_mut() {
            addresses.remove(worker);
        }
    }

    /// Clears cache that stores which KVS nodes are responsible for which key.
    ///
    /// Subsequent requests will need to query the responsible KVS nodes from the
    /// routing node again.
    pub fn clear_cache(&mut self) {
        self.key_address_cache.clear();
    }
}

/// A helper method to check for the default failure modes for a request that
/// retrieves a response.
///
/// It returns true if the caller method should reissue
/// the request (this happens if errno == 2). Otherwise, it returns false. It
/// invalidates the local cache if the information is out of date.
fn check_tuple(
    tuple: &ResponseTuple,
    key_address_cache: &mut HashMap<ClientKey, HashSet<KvsThread>>,
) -> bool {
    let key: ClientKey = tuple.key.clone().into();
    if let Some(AnnaError::WrongThread) = tuple.error {
        log::info!(
            "Server ordered invalidation of key address cache for key {}.
      Retrying request.",
            key
        );

        key_address_cache.remove(&key);
        return true;
    }

    if tuple.invalidate {
        key_address_cache.remove(&key);

        log::info!(
            "Server ordered invalidation of key address cache for key {}",
            key
        );
    }
    false
}

fn generate_bad_response(req: &Request) -> Response {
    Response {
        response_id: req.request_id.clone(),
        error: Err(AnnaError::Timeout),
        tuples: req
            .client_operations
            .iter()
            .map(|key_operation| {
                ResponseTuple::new(
                    key_operation.key().clone(),
                    None,
                    key_operation.response_ty(),
                    None,
                    false,
                )
            })
            .collect(),
    }
}

trait TcpMessageStream: Stream<Item = eyre::Result<TcpMessage>> + FusedStream + Send + Sync {}

impl<T> TcpMessageStream for T where
    T: Stream<Item = eyre::Result<TcpMessage>> + FusedStream + Send + Sync
{
}
