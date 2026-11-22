use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use async_channel::{Receiver, Sender, TrySendError};
use async_global_executor::Task;
use futures_lite::{StreamExt, future};

use super::{Ack, CallMode, CancelState, Command, Tokens, encode_error};
use crate::address::Address;
use crate::service::{DispatchError, PeerPolicy, ReplyBody, ServicePolicy};
use crate::stream::FeedSender;
use crate::wire::{self, Frame, Kind, Listener, Socket};
use crate::{
    Error, ErrorKind, MethodError, ProtocolProfile, Request, Router, Signal, SignalFilter,
};

const HUB: u32 = 0;
const OUTBOX_CAPACITY: usize = 128;
const EVENT_CAPACITY: usize = 128;
const MAX_CALLS: usize = 1024;
const MAX_PEERS: usize = 128;
const MAX_HANDLERS: usize = 128;
const MAX_LISTENERS: usize = 128;
const ALLOW_INBOUND: i32 = 1;
const ALLOW_OUTBOUND: i32 = 2;

pub(super) struct LocalService {
    pub name: String,
    pub router: Router,
    pub policy: ServicePolicy,
}

pub(super) struct Driver {
    hub: Link,
    listener: Option<Task<()>>,
    profile: ProtocolProfile,
    unique_name: String,
    peers: HashMap<u32, Peer>,
    routes: HashMap<String, u32>,
    lookups: HashMap<String, Vec<u32>>,
    calls: HashMap<u32, ActiveCall>,
    handlers: HashMap<(u32, u32), ActiveHandler>,
    watches: HashMap<SignalFilter, WatchGroup>,
    watch_count: usize,
    service: Option<Arc<LocalService>>,
    tokens: Arc<Tokens>,
    events: Sender<Event>,
    incoming: Receiver<Event>,
}

impl Driver {
    pub(super) fn new(
        socket: Socket,
        listener: Option<Listener>,
        unique_name: String,
        service: Option<LocalService>,
        tokens: Arc<Tokens>,
    ) -> Self {
        let (events, incoming) = async_channel::bounded(EVENT_CAPACITY);
        let profile = socket.profile();
        let listener = listener.map(|listener| {
            let events = events.clone();
            async_global_executor::spawn(async move {
                loop {
                    let event = match listener.accept().await {
                        Ok(socket) => Event::Accepted(socket),
                        Err(error) => {
                            let error = Error::io("accept Luna peer", error);
                            let _ = events.send(Event::Lost(HUB, error)).await;
                            break;
                        }
                    };
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
            })
        });
        Self {
            hub: Link::start(HUB, socket, &events),
            listener,
            profile,
            unique_name,
            peers: HashMap::new(),
            routes: HashMap::new(),
            lookups: HashMap::new(),
            calls: HashMap::new(),
            handlers: HashMap::new(),
            watches: HashMap::new(),
            watch_count: 0,
            service: service.map(Arc::new),
            tokens,
            events,
            incoming,
        }
    }

    pub(super) async fn run(
        mut self,
        commands: Receiver<Command>,
        cancellation: Receiver<()>,
        terminal: Arc<Mutex<Option<Error>>>,
    ) {
        let reason = loop {
            if commands.is_closed() {
                break Error::disconnected("Luna connection was closed");
            }
            let work = future::race(
                async { Work::Command(commands.recv().await.ok()) },
                future::race(
                    async { Work::Event(self.incoming.recv().await.ok()) },
                    async { Work::Cancellation(cancellation.recv().await.is_ok()) },
                ),
            )
            .await;
            let result = match work {
                Work::Command(Some(command)) => self.command(command),
                Work::Event(Some(event)) => self.event(event),
                Work::Cancellation(true) => self.cancellations(),
                _ => Err(Error::disconnected("Luna connection was closed")),
            };
            if let Err(error) = result {
                break error;
            }
        };

        *terminal.lock().expect("connection terminal mutex poisoned") = Some(reason.clone());
        commands.close();
        cancellation.close();
        while let Ok(command) = commands.try_recv() {
            command.reject(reason.clone());
        }
        for (_, call) in self.calls.drain() {
            call.fail(reason.clone());
        }
        for (_, group) in self.watches.drain() {
            group.fail(reason.clone());
        }
        let mut tasks = vec![self.hub.task];
        tasks.extend(self.listener);
        tasks.extend(self.peers.into_values().map(|peer| peer.link.task));
        tasks.extend(self.handlers.into_values().map(|handler| handler.task));
        for task in tasks {
            task.cancel().await;
        }
    }

    fn command(&mut self, command: Command) -> Result<(), Error> {
        match command {
            Command::Call {
                id,
                address,
                payload,
                replies,
                mode,
                state,
            } => {
                let call = ActiveCall {
                    replies,
                    mode,
                    state,
                    target: None,
                };
                self.start_call(id, address, &payload, call);
            }
            Command::Watch {
                id,
                filter,
                signals,
                ack,
                state,
            } => {
                self.start_watch(
                    id,
                    filter,
                    Watch {
                        signals,
                        ready: Some(ack),
                        state,
                    },
                );
            }
            Command::Emit {
                category,
                method,
                payload,
                ack,
            } => {
                let frame = self.tokens.next().and_then(|token| {
                    wire::signal(token, &category, &method, &payload).map_err(encode_error)
                });
                match frame {
                    Ok(frame) => self.hub.send_ack(frame, Some(ack)),
                    Err(error) => acknowledge(Some(ack), Err(error)),
                }
            }
        }
        Ok(())
    }

    fn start_call(&mut self, id: u32, address: Address, payload: &str, mut call: ActiveCall) {
        if call.state.is_cancelled() || call.replies.is_closed() {
            acknowledge(call.state.complete(), Ok(()));
            return;
        }
        if self.calls.len() >= MAX_CALLS {
            call.fail(overloaded("too many pending Luna calls"));
            return;
        }

        let to_hub = matches!(
            address.service.as_str(),
            "com.palm.bus" | "com.webos.service.bus"
        ) && !matches!(
            self.profile,
            ProtocolProfile::LocalListener | ProtocolProfile::InitiatorInfo
        );
        let frame = if to_hub {
            if address.category != "/" {
                call.fail(Error::invalid(
                    "use the signal API for hub signal registration",
                ));
                return;
            }
            wire::hub_method_call(id, &address.method, payload)
        } else {
            wire::method_call(
                self.profile,
                id,
                &address.category,
                &address.method,
                payload,
            )
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                call.fail(encode_error(error));
                return;
            }
        };

        let peer = if to_hub {
            Some(HUB)
        } else {
            self.routes.get(&address.service).copied()
        };
        if let Some(peer) = peer {
            self.send_call(id, peer, frame, call);
            return;
        }

        if let Err(error) = self.lookup(&address.service, id) {
            call.fail(error);
            return;
        }
        call.target = Some(Target::Lookup {
            service: address.service,
            frame,
        });
        self.calls.insert(id, call);
    }

    fn lookup(&mut self, service: &str, id: u32) -> Result<(), Error> {
        if let Some(waiting) = self.lookups.get_mut(service) {
            waiting.push(id);
            return Ok(());
        }
        if self.lookups.len() >= MAX_CALLS {
            return Err(overloaded("too many pending Luna service lookups"));
        }
        let query = wire::query_name(self.tokens.next()?, service).map_err(encode_error)?;
        self.hub.send(query)?;
        self.lookups.insert(service.to_owned(), vec![id]);
        Ok(())
    }

    fn send_call(&mut self, id: u32, peer: u32, frame: Frame, mut call: ActiveCall) {
        if call.state.is_cancelled() || call.replies.is_closed() {
            acknowledge(call.state.complete(), Ok(()));
            return;
        }
        match self.link(peer).and_then(|link| link.send(frame)) {
            Ok(()) => {
                call.target = Some(Target::Sent(peer));
                self.calls.insert(id, call);
            }
            Err(error) => call.fail(error),
        }
    }

    fn start_watch(&mut self, id: u32, filter: SignalFilter, mut watch: Watch) {
        if watch.state.is_cancelled() || watch.signals.is_closed() {
            acknowledge(watch.state.complete(), Ok(()));
            return;
        }
        if self.watch_count >= MAX_LISTENERS {
            watch.fail(overloaded("too many Luna signal listeners"));
            return;
        }
        if let Some(group) = self.watches.get_mut(&filter) {
            if group.pending.is_none() {
                acknowledge(watch.ready.take(), Ok(()));
            }
            group.listeners.insert(id, watch);
        } else {
            let frame = wire::signal_match(
                Kind::SignalRegister,
                id,
                filter.category_name(),
                filter.method(),
            )
            .map_err(encode_error)
            .and_then(|frame| self.hub.send(frame));
            if let Err(error) = frame {
                watch.fail(error);
                return;
            }
            self.watches.insert(
                filter,
                WatchGroup {
                    pending: Some(id),
                    listeners: HashMap::from([(id, watch)]),
                },
            );
        }
        self.watch_count += 1;
    }

    fn event(&mut self, event: Event) -> Result<(), Error> {
        match event {
            Event::Frame(HUB, frame) => self.hub_frame(frame),
            Event::Frame(peer, frame) => {
                if self.peers.contains_key(&peer) {
                    if let Err(error) = self.peer_frame(peer, frame) {
                        self.peer_lost(peer, error);
                    }
                }
                Ok(())
            }
            Event::Accepted(socket) => {
                if self.service.is_some() && self.peers.len() < MAX_PEERS {
                    let policy = Arc::new(PeerPolicy::from_legacy_listener());
                    self.insert_peer(socket, None, Some(policy), false)?;
                }
                Ok(())
            }
            Event::Lost(HUB, error) => Err(error),
            Event::Lost(peer, error) => {
                self.peer_lost(peer, error);
                Ok(())
            }
            Event::Service {
                peer,
                token,
                job,
                output,
            } => {
                if let Err(error) = self.service_reply(peer, token, job, output) {
                    self.peer_lost(peer, error);
                }
                Ok(())
            }
        }
    }

    fn hub_frame(&mut self, frame: Frame) -> Result<(), Error> {
        match frame.kind {
            Kind::QueryNameReply | Kind::Initiator => self.offer(frame),
            Kind::Reply | Kind::ReplyWithFd | Kind::Error | Kind::ErrorUnknownMethod => {
                self.handle_reply(HUB, &frame)
            }
            Kind::Signal | Kind::ServiceUpSignal | Kind::ServiceDownSignal => {
                self.deliver_signal(&frame)
            }
            Kind::Shutdown => Err(Error::disconnected("Luna hub shut down")),
            // Monitor connection announcements include an owned descriptor. Dropping
            // the frame closes it while keeping the hub channel in sync.
            _ => Ok(()),
        }
    }

    fn offer(&mut self, mut frame: Frame) -> Result<(), Error> {
        let fd = frame.fd.take();
        let offer = wire::peer_offer(self.profile, &frame).map_err(decode_error)?;
        let name = offer.name.filter(|name| !name.is_empty());
        let waiting = if offer.code != 0 || offer.permissions & ALLOW_OUTBOUND != 0 {
            name.and_then(|name| self.lookups.remove(name))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let peer = if offer.code != 0 {
            Err(lookup_error(offer.code, name))
        } else if let Some(fd) = fd {
            self.add_peer(
                offer.name,
                offer.unique_name,
                offer.groups,
                offer.trust,
                offer.permissions,
                fd,
            )
        } else {
            Err(Error::protocol(
                "successful Luna peer offer did not include a socket",
            ))
        };

        for id in waiting {
            let Some(mut call) = self.calls.remove(&id) else {
                continue;
            };
            match &peer {
                Ok(peer) => {
                    if let Some(Target::Lookup { frame, .. }) = call.target.take() {
                        self.send_call(id, *peer, frame, call);
                    }
                }
                Err(error) => call.fail(error.clone()),
            }
        }
        Ok(())
    }

    fn add_peer(
        &mut self,
        name: Option<&str>,
        unique: Option<&str>,
        groups: Option<&str>,
        trust: Option<&str>,
        permissions: i32,
        fd: std::os::fd::OwnedFd,
    ) -> Result<u32, Error> {
        if unique.is_none_or(str::is_empty) {
            return Err(Error::protocol(
                "Luna peer offer did not contain a unique name",
            ));
        }
        let socket = Socket::from_fd(fd, self.profile)
            .map_err(|error| Error::io("adopt Luna peer socket", error))?;
        let policy = if self.service.is_some() && permissions & ALLOW_INBOUND != 0 {
            Some(Arc::new(PeerPolicy::from_hub(groups, trust, permissions)?))
        } else {
            None
        };
        self.insert_peer(socket, name, policy, permissions & ALLOW_OUTBOUND != 0)
    }

    fn insert_peer(
        &mut self,
        socket: Socket,
        name: Option<&str>,
        policy: Option<Arc<PeerPolicy>>,
        can_call: bool,
    ) -> Result<u32, Error> {
        if self.peers.len() >= MAX_PEERS {
            return Err(overloaded("too many connected Luna peers"));
        }
        let id = self.tokens.next()?;
        let link = Link::start(id, socket, &self.events);
        if self.profile == ProtocolProfile::LocalListener && can_call {
            let local_name = self.service.as_ref().map(|local| local.name.as_str());
            let info = wire::client_info(self.tokens.next()?, local_name, &self.unique_name)
                .map_err(encode_error)?;
            link.send(info)?;
        }
        if self.profile.sends_client_connected() {
            self.hub.send(Frame::new(
                Kind::ClientConnected,
                self.tokens.next()?,
                false,
                Vec::new(),
            ))?;
            link.send(Frame::new(
                Kind::ClientConnected,
                self.tokens.next()?,
                false,
                Vec::new(),
            ))?;
        }
        self.peers.insert(
            id,
            Peer {
                link,
                name: name.map(str::to_owned),
                policy,
                can_call,
            },
        );
        if let Some(name) = name.filter(|_| can_call) {
            self.routes.insert(name.to_owned(), id);
        }
        Ok(id)
    }

    fn peer_frame(&mut self, peer: u32, frame: Frame) -> Result<(), Error> {
        match frame.kind {
            Kind::Reply | Kind::ReplyWithFd | Kind::Error | Kind::ErrorUnknownMethod => {
                self.handle_reply(peer, &frame)
            }
            Kind::MethodCall => self.dispatch(peer, &frame),
            Kind::CancelMethodCall => {
                let token = wire::parse_cancel(&frame).map_err(decode_error)?;
                self.handlers.remove(&(peer, token));
                Ok(())
            }
            Kind::Shutdown => Err(Error::disconnected("Luna peer shut down")),
            // Peer names and policy come only from the hub's descriptor offer.
            _ => Ok(()),
        }
    }

    fn handle_reply(&mut self, peer: u32, frame: &Frame) -> Result<(), Error> {
        let (id, response) = if frame.kind == Kind::ReplyWithFd {
            (
                wire::reply_token(frame).map_err(decode_error)?,
                Err(Error::protocol(
                    "Luna reply contains an unsupported file descriptor payload",
                )),
            )
        } else {
            let (id, payload) = wire::parse_reply(self.profile, frame).map_err(decode_error)?;
            let response = if frame.kind == Kind::Reply {
                Ok(payload)
            } else {
                let kind = if payload.starts_with("Denied method call") {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::Rejected
                };
                Err(Error::bus(kind, None, payload))
            };
            (id, response)
        };

        if peer == HUB && self.watch_reply(id, response.clone()) {
            return Ok(());
        }
        let Some(call) = self.calls.get(&id) else {
            return Ok(());
        };
        if !matches!(call.target, Some(Target::Sent(link)) if link == peer) {
            return Ok(());
        }
        if call.state.is_cancelled() || call.replies.is_closed() {
            if let Some(call) = self.calls.remove(&id) {
                self.send_cancel(peer, id, call.state.complete())?;
            }
            return Ok(());
        }
        let once = call.mode == CallMode::Once;
        let is_reply = response.is_ok();
        let result = response.and_then(|payload| call.replies.push(payload.to_owned()));
        if once || result.is_err() {
            if let Some(call) = self.calls.remove(&id) {
                let cancel_peer = is_reply && (once || result.is_err());
                match result {
                    Ok(()) => acknowledge(call.state.complete(), Ok(())),
                    Err(error) => call.fail(error),
                }
                if cancel_peer {
                    self.send_cancel(peer, id, None)?;
                }
            }
        }
        Ok(())
    }

    fn watch_reply(&mut self, id: u32, payload: Result<&str, Error>) -> bool {
        let filter = self
            .watches
            .iter()
            .find_map(|(filter, group)| (group.pending == Some(id)).then(|| filter.clone()));
        let Some(filter) = filter else { return false };
        let result = payload.and_then(signal_ack);
        if let Some(group) = self.watches.get_mut(&filter) {
            if result.is_ok() {
                group.pending = None;
                for watch in group.listeners.values_mut() {
                    acknowledge(watch.ready.take(), Ok(()));
                }
            } else if let Some(group) = self.watches.remove(&filter) {
                self.watch_count -= group.listeners.len();
                if let Err(error) = result {
                    group.fail(error);
                }
            }
        }
        true
    }

    fn deliver_signal(&mut self, frame: &Frame) -> Result<(), Error> {
        let message = wire::parse_signal(frame).map_err(decode_error)?;
        let mut empty = Vec::new();
        for (filter, group) in &mut self.watches {
            if !filter.matches(message.category, message.method) {
                continue;
            }
            let signal = Signal::new(
                message.category.to_owned(),
                message.method.to_owned(),
                message.payload.to_owned(),
            );
            let failed: Vec<_> = group
                .listeners
                .iter()
                .filter_map(|(&id, watch)| {
                    watch
                        .signals
                        .push(signal.clone())
                        .err()
                        .map(|error| (id, error))
                })
                .collect();
            for (id, error) in failed {
                if let Some(watch) = group.listeners.remove(&id) {
                    self.watch_count -= 1;
                    watch.fail(error);
                }
            }
            if group.listeners.is_empty() {
                empty.push(filter.clone());
            }
        }
        for filter in empty {
            self.unregister_watch(&filter, None)?;
        }
        Ok(())
    }

    fn cancellations(&mut self) -> Result<(), Error> {
        let cancelled: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(&id, call)| call.state.is_cancelled().then_some(id))
            .collect();
        for id in cancelled {
            if let Some(call) = self.calls.remove(&id) {
                let ack = call.state.complete();
                match call.target {
                    Some(Target::Sent(peer)) => {
                        if let Err(error) = self.send_cancel(peer, id, ack) {
                            if peer == HUB {
                                return Err(error);
                            }
                            self.peer_lost(peer, error);
                        }
                    }
                    Some(Target::Lookup { service, .. }) => {
                        if let Some(waiting) = self.lookups.get_mut(&service) {
                            waiting.retain(|token| *token != id);
                        }
                        acknowledge(ack, Ok(()));
                    }
                    None => acknowledge(ack, Ok(())),
                }
            }
        }
        let mut empty = Vec::new();
        for (filter, group) in &mut self.watches {
            let cancelled: Vec<_> = group
                .listeners
                .iter()
                .filter_map(|(&id, watch)| watch.state.is_cancelled().then_some(id))
                .collect();
            let mut remaining = cancelled.len();
            for id in cancelled {
                if let Some(watch) = group.listeners.remove(&id) {
                    self.watch_count -= 1;
                    remaining -= 1;
                    let ack = watch.state.complete();
                    if remaining == 0 && group.listeners.is_empty() {
                        empty.push((filter.clone(), ack));
                    } else {
                        acknowledge(ack, Ok(()));
                    }
                }
            }
        }
        for (filter, ack) in empty {
            self.unregister_watch(&filter, ack)?;
        }
        Ok(())
    }

    fn unregister_watch(&mut self, filter: &SignalFilter, ack: Option<Ack>) -> Result<(), Error> {
        self.watches.remove(filter);
        let frame = wire::signal_match(
            Kind::SignalUnregister,
            self.tokens.next()?,
            filter.category_name(),
            filter.method(),
        )
        .map_err(encode_error)?;
        self.hub.send_control(frame, ack)
    }

    fn send_cancel(&self, peer: u32, id: u32, ack: Option<Ack>) -> Result<(), Error> {
        if peer == HUB {
            acknowledge(ack, Ok(()));
            return Ok(());
        }
        match self.link(peer).and_then(|link| {
            let frame = wire::cancel(self.tokens.next()?, id).map_err(encode_error)?;
            Ok((link, frame))
        }) {
            Ok((link, frame)) => link.send_control(frame, ack),
            Err(error) => {
                acknowledge(ack, Err(error.clone()));
                Err(error)
            }
        }
    }

    fn dispatch(&mut self, peer: u32, frame: &Frame) -> Result<(), Error> {
        let call = wire::parse_method_call(self.profile, frame).map_err(decode_error)?;
        let Some(remote) = self.peers.get(&peer) else {
            return Ok(());
        };
        let (Some(service), Some(policy)) = (&self.service, &remote.policy) else {
            let message = denied(call.category, call.method);
            return self.send_response(
                peer,
                frame.token,
                frame.public,
                Kind::ErrorUnknownMethod,
                &message,
            );
        };
        let key = (peer, frame.token);
        if self.handlers.contains_key(&key) {
            return Err(Error::protocol("peer reused an active Luna call token"));
        }
        if self.handlers.len() >= MAX_HANDLERS {
            let message = MethodError::new(-1, "too many active service calls").into_payload();
            return self.send_response(peer, frame.token, frame.public, Kind::Reply, &message);
        }
        let job = self.tokens.next()?;
        let events = self.events.clone();
        let service = Arc::clone(service);
        let policy = Arc::clone(policy);
        let request = Request::new(
            call.category.to_owned(),
            call.method.to_owned(),
            call.payload.to_owned(),
            remote.name.clone(),
        );
        let token = frame.token;
        let public = frame.public;
        let task = async_global_executor::spawn(async move {
            let result = service
                .router
                .dispatch(request, &service.policy, &policy, public)
                .await;
            let send = |output| {
                events.send(Event::Service {
                    peer,
                    token,
                    job,
                    output,
                })
            };
            match result {
                Ok(reply) => match reply.into_body() {
                    ReplyBody::One(payload) => {
                        let _ = send(ServiceOutput::Final(payload)).await;
                    }
                    ReplyBody::Stream(mut updates) => {
                        while let Some(update) = updates.next().await {
                            let outcome = match update {
                                Ok(payload) => send(ServiceOutput::Update(payload)).await,
                                Err(error) => {
                                    let _ = send(ServiceOutput::Final(error.into_payload())).await;
                                    return;
                                }
                            };
                            if outcome.is_err() {
                                return;
                            }
                        }
                        let _ = send(ServiceOutput::Finished).await;
                    }
                },
                Err(error) => {
                    let _ = send(ServiceOutput::Rejected(error)).await;
                }
            }
        });
        self.handlers.insert(
            key,
            ActiveHandler {
                job,
                task,
                public,
                category: call.category.to_owned(),
                method: call.method.to_owned(),
            },
        );
        Ok(())
    }

    fn service_reply(
        &mut self,
        peer: u32,
        token: u32,
        job: u32,
        output: ServiceOutput,
    ) -> Result<(), Error> {
        let key = (peer, token);
        let Some(active) = self.handlers.get(&key).filter(|active| active.job == job) else {
            return Ok(());
        };
        let public = active.public;
        let (kind, mut payload, mut done) = match output {
            ServiceOutput::Update(payload) => (Kind::Reply, payload, false),
            ServiceOutput::Final(payload) => (Kind::Reply, payload, true),
            ServiceOutput::Finished => {
                self.handlers.remove(&key);
                return Ok(());
            }
            ServiceOutput::Rejected(error) => match error {
                DispatchError::UnknownMethod => (
                    Kind::ErrorUnknownMethod,
                    format!(
                        "Unknown method {:?} for category {:?}",
                        active.method, active.category
                    ),
                    true,
                ),
                DispatchError::PermissionDenied => (
                    Kind::ErrorUnknownMethod,
                    denied(&active.category, &active.method),
                    true,
                ),
                DispatchError::Method(error) => (Kind::Reply, error.into_payload(), true),
            },
        };
        if kind == Kind::Reply && wire::validate_reply_payload(&payload).is_err() {
            payload =
                MethodError::new(-1, "service produced an invalid Luna payload").into_payload();
            done = true;
        }
        if done {
            self.handlers.remove(&key);
        }
        self.send_response(peer, token, public, kind, &payload)
    }

    fn send_response(
        &self,
        peer: u32,
        reply_to: u32,
        public: bool,
        kind: Kind,
        payload: &str,
    ) -> Result<(), Error> {
        let token = self.tokens.next()?;
        let mut frame =
            wire::reply(self.profile, kind, token, reply_to, payload).map_err(encode_error)?;
        frame.public = public;
        self.link(peer)?.send(frame)
    }

    fn link(&self, peer: u32) -> Result<&Link, Error> {
        if peer == HUB {
            Ok(&self.hub)
        } else {
            self.peers
                .get(&peer)
                .map(|peer| &peer.link)
                .ok_or_else(|| Error::disconnected("Luna peer disconnected"))
        }
    }

    fn peer_lost(&mut self, id: u32, error: Error) {
        let Some(peer) = self.peers.remove(&id) else {
            return;
        };
        if let Some(name) = &peer.name {
            if self.routes.get(name) == Some(&id) {
                self.routes.remove(name);
                if let Some((&replacement, _)) = self
                    .peers
                    .iter()
                    .find(|(_, peer)| peer.can_call && peer.name.as_ref() == Some(name))
                {
                    self.routes.insert(name.clone(), replacement);
                }
            }
        }
        let failed: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(&call_id, call)| {
                matches!(call.target, Some(Target::Sent(peer)) if peer == id).then_some(call_id)
            })
            .collect();
        for call in failed {
            if let Some(call) = self.calls.remove(&call) {
                call.fail(error.clone());
            }
        }
        self.handlers.retain(|(peer, _), _| *peer != id);
    }
}

enum Work {
    Command(Option<Command>),
    Cancellation(bool),
    Event(Option<Event>),
}

struct ActiveCall {
    replies: FeedSender<String>,
    mode: CallMode,
    state: Arc<CancelState>,
    target: Option<Target>,
}

impl ActiveCall {
    fn fail(self, error: Error) {
        acknowledge(self.state.complete(), Err(error.clone()));
        self.replies.fail(error);
    }
}

enum Target {
    Lookup { service: String, frame: Frame },
    Sent(u32),
}

struct Peer {
    link: Link,
    policy: Option<Arc<PeerPolicy>>,
    name: Option<String>,
    can_call: bool,
}

struct ActiveHandler {
    job: u32,
    task: Task<()>,
    public: bool,
    category: String,
    method: String,
}

enum ServiceOutput {
    Update(String),
    Final(String),
    Rejected(DispatchError),
    Finished,
}

enum Event {
    Frame(u32, Frame),
    Accepted(Socket),
    Lost(u32, Error),
    Service {
        peer: u32,
        token: u32,
        job: u32,
        output: ServiceOutput,
    },
}

struct WatchGroup {
    pending: Option<u32>,
    listeners: HashMap<u32, Watch>,
}

impl WatchGroup {
    fn fail(self, error: Error) {
        for watch in self.listeners.into_values() {
            watch.fail(error.clone());
        }
    }
}

struct Watch {
    signals: FeedSender<Signal>,
    ready: Option<Ack>,
    state: Arc<CancelState>,
}

impl Watch {
    fn fail(self, error: Error) {
        acknowledge(self.state.complete(), Err(error.clone()));
        acknowledge(self.ready, Err(error.clone()));
        self.signals.fail(error);
    }
}

struct Link {
    outbound: Sender<Write>,
    task: Task<()>,
}

struct Write {
    frame: Frame,
    written: Option<Ack>,
}

impl Link {
    fn start(id: u32, socket: Socket, events: &Sender<Event>) -> Self {
        let (outbound, writes) = async_channel::bounded::<Write>(OUTBOX_CAPACITY);
        let events = events.clone();
        let task = async_global_executor::spawn(async move {
            let receive = async {
                loop {
                    let frame = match socket.recv().await {
                        Ok(frame) => frame,
                        Err(error) => return Error::io("read Luna socket", error),
                    };
                    if events.send(Event::Frame(id, frame)).await.is_err() {
                        return Error::disconnected("Luna driver stopped");
                    }
                }
            };
            let write = async {
                while let Ok(Write { frame, written }) = writes.recv().await {
                    let result = socket
                        .send(&frame)
                        .await
                        .map_err(|error| Error::io("write Luna socket", error));
                    acknowledge(written, result.clone());
                    if let Err(error) = result {
                        return error;
                    }
                }
                Error::disconnected("Luna outbound queue closed")
            };
            let reason = future::race(receive, write).await;
            writes.close();
            while let Ok(write) = writes.try_recv() {
                acknowledge(write.written, Err(reason.clone()));
            }
            let _ = events.send(Event::Lost(id, reason)).await;
        });
        Self { outbound, task }
    }

    fn send(&self, frame: Frame) -> Result<(), Error> {
        self.send_control(frame, None)
    }

    fn send_ack(&self, frame: Frame, ack: Option<Ack>) {
        let _ = self.send_control(frame, ack);
    }

    fn send_control(&self, frame: Frame, written: Option<Ack>) -> Result<(), Error> {
        self.outbound
            .try_send(Write { frame, written })
            .map_err(|failure| {
                let (write, error) = match failure {
                    TrySendError::Full(write) => {
                        (write, overloaded("Luna socket send queue is full"))
                    }
                    TrySendError::Closed(write) => {
                        (write, Error::disconnected("Luna socket is closed"))
                    }
                };
                acknowledge(write.written, Err(error.clone()));
                error
            })
    }
}

impl Command {
    fn reject(self, error: Error) {
        match self {
            Self::Call { replies, state, .. } => {
                acknowledge(state.complete(), Err(error.clone()));
                replies.fail(error);
            }
            Self::Watch {
                signals,
                ack,
                state,
                ..
            } => Watch {
                signals,
                ready: Some(ack),
                state,
            }
            .fail(error),
            Self::Emit { ack, .. } => acknowledge(Some(ack), Err(error)),
        }
    }
}

fn acknowledge(ack: Option<Ack>, result: Result<(), Error>) {
    if let Some(ack) = ack {
        let _ = ack.try_send(result);
    }
}

fn denied(category: &str, method: &str) -> String {
    format!("Denied method call {method:?} for category {category:?}")
}

fn decode_error(error: io::Error) -> Error {
    Error::io("decode Luna frame", error)
}

fn overloaded(message: &str) -> Error {
    Error::new(ErrorKind::Overloaded, message)
}

fn lookup_error(code: i32, name: Option<&str>) -> Error {
    let (kind, operation) = match code {
        -2 | -7 => (ErrorKind::PermissionDenied, "Luna peer lookup was denied"),
        -3 | -5 => (ErrorKind::Timeout, "Luna peer lookup timed out"),
        _ => (ErrorKind::Rejected, "Luna service is unavailable"),
    };
    Error::bus(
        kind,
        Some(code),
        format!("{operation} for {:?}", name.unwrap_or("")),
    )
}

fn signal_ack(payload: &str) -> Result<(), Error> {
    let ack =
        json::parse(payload).map_err(|_| Error::protocol("invalid Luna signal acknowledgment"))?;
    match ack["returnValue"].as_bool() {
        Some(true) => Ok(()),
        Some(false) => {
            let message = ack["errorText"]
                .as_str()
                .unwrap_or("Luna signal registration rejected");
            let kind = if message.eq_ignore_ascii_case("access denied") {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Rejected
            };
            Err(Error::bus(kind, ack["errorCode"].as_i32(), message))
        }
        None => Err(Error::protocol(
            "Luna signal acknowledgment is missing returnValue",
        )),
    }
}
