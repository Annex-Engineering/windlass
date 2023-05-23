use std::{
    any::TypeId,
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::{atomic::AtomicUsize, Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot, OnceCell,
    },
    task::{spawn, JoinHandle},
};
use tracing::{debug, trace};

use crate::encoding::{next_byte, MessageDecodeError};
use crate::messages::{format_command_args, EncodedMessage, Message, WithOid, WithoutOid};
use crate::transport::{Transport, TransportReceiver};
use crate::{
    dictionary::{Dictionary, DictionaryError, RawDictionary},
    transport::TransportError,
};

mcu_command!(Identify, "identify" = 1, offset: u32, count: u8);
mcu_reply!(
    IdentifyResponse,
    "identify_response" = 0,
    offset: u32,
    data: Vec<u8>,
);

/// MCU Connection Errors
#[derive(thiserror::Error, Debug)]
pub enum McuConnectionError {
    /// Encoding a message was attempted but a command with a matching name doesn't exist in the
    /// dictionary.
    #[error("unknown message ID for command '{0}'")]
    UnknownMessageId(&'static str),
    /// An issue was encountered while decoding the received frame
    #[error("error decoding message with ID '{0}'")]
    DecodingError(#[from] MessageDecodeError),
    /// An error was encountered while fetching the dictionary
    #[error("error obtaining identify data: {0}")]
    DictionaryFetch(Box<dyn std::error::Error + Send + Sync>),
    /// An error was encountered while parsing the dictionary
    #[error("dictionary issue: {0}")]
    Dictionary(#[from] DictionaryError),
    /// Received an unknown command from the MCU
    #[error("unknown command {0}")]
    UnknownCommand(u8),
    /// There was a mismatch between the command arguments on the remote and local sides.
    #[error("mismatched command {0}: '{1}' vs '{2}'")]
    CommandMismatch(&'static str, String, String),
    /// There was a transport-level issue
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
}

#[derive(thiserror::Error, Debug)]
pub enum SendReceiveError {
    #[error("timeout")]
    Timeout,
    #[error("mcu connection error: {0}")]
    McuConnection(#[from] McuConnectionError),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct HandlerId(u8, Option<u8>);

trait Handler: Send + Sync {
    fn handle(&mut self, data: &mut &[u8]) -> HandlerResult;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RegisteredResponseHandle(usize);

#[derive(Default)]
struct Handlers {
    handlers: std::sync::Mutex<BTreeMap<HandlerId, (usize, Box<dyn Handler>)>>,
    next_handler: AtomicUsize,
}

impl std::fmt::Debug for Handlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handlers").finish()
    }
}

impl Handlers {
    fn register(
        &self,
        command: u8,
        oid: Option<u8>,
        handler: Box<dyn Handler>,
    ) -> RegisteredResponseHandle {
        let id = HandlerId(command, oid);
        let uniq = self
            .next_handler
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.handlers.lock().unwrap().insert(id, (uniq, handler));
        RegisteredResponseHandle(uniq)
    }

    fn call(&self, command: u8, oid: Option<u8>, data: &mut &[u8]) -> bool {
        let id = HandlerId(command, oid);
        let mut handlers = self.handlers.lock().unwrap();
        let handler = handlers.get_mut(&id);
        if let Some((_, handler)) = handler {
            if matches!(handler.handle(data), HandlerResult::Deregister) {
                handlers.remove(&id);
            }
            true
        } else {
            false
        }
    }

    fn remove_handler(&self, command: u8, oid: Option<u8>, uniq: Option<RegisteredResponseHandle>) {
        let id = HandlerId(command, oid);
        let mut handlers = self.handlers.lock().unwrap();
        if let Some(RegisteredResponseHandle(uniq)) = uniq {
            match handlers.get(&id) {
                None => return,
                Some((u, _)) if *u != uniq => return,
                _ => {}
            }
        }
        handlers.remove(&id);
    }
}

#[derive(Debug)]
struct McuConnectionInner {
    dictionary: OnceCell<Dictionary>,
    handlers: Handlers,
    good_types: Mutex<BTreeSet<TypeId>>,
}

impl McuConnectionInner {
    async fn receive_loop(&self, mut inbox: TransportReceiver) -> Result<(), McuConnectionError> {
        while let Some(frame) = inbox.recv().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(e) => return Err(McuConnectionError::Transport(e)),
            };
            let frame = &mut frame.as_slice();
            if let Err(e) = self.parse_frame(frame) {
                return Err(e);
            }
        }
        Ok(())
    }

    fn parse_frame(&self, frame: &mut &[u8]) -> Result<(), McuConnectionError> {
        while !frame.is_empty() {
            let cmd = next_byte(frame)?;

            // If a raw handler is registered for this, call it
            if self.handlers.call(cmd, None, frame) {
                continue;
            }

            // There was no registered raw handler, check the dictionary to decode
            if let Some(dict) = self.dictionary.get() {
                if let Some(parser) = dict.message_parsers.get(&cmd) {
                    let mut curmsg = &frame[..];
                    let oid = parser.skip_with_oid(frame)?;
                    if tracing::enabled!(tracing::Level::TRACE) {
                        let mut tmp = &curmsg[..];
                        trace!(
                            cmd_id = cmd,
                            cmd_name = parser.name,
                            data = ?parser.parse(&mut tmp).unwrap(), // Safe because we skipped before and it passed
                            "Received message",
                        );
                    }
                    if !self.handlers.call(cmd, oid, &mut curmsg) {
                        debug!(
                            cmd_id = cmd,
                            cmd_name = parser.name,
                            // Safe because we skipped before and it passed. Since call returned
                            // false, we know no handler took the data and can safely take curmsg
                            // here.
                            data = ?parser.parse(&mut curmsg).unwrap(),
                            "Unhandled message",
                        );
                    }
                } else {
                    return Err(McuConnectionError::UnknownCommand(cmd));
                }
            }
        }
        Ok(())
    }
}

/// Manages a connection to a Klipper MCU
///
///
#[derive(Debug)]
pub struct McuConnection {
    inner: Arc<McuConnectionInner>,
    transport: Transport,
    _receiver: JoinHandle<()>,
    exit_code: ExitCode,
}

#[derive(Debug)]
enum ExitCode {
    Waiting(oneshot::Receiver<Result<(), McuConnectionError>>),
    Exited(Result<(), McuConnectionError>),
}

impl McuConnection {
    /// Connect to an MCU
    ///
    /// Attempts to connect to a MCU on the given data stream interface. The MCU is contacted and
    /// the dictionary is loaded and applied. The returned [McuConnection] is fully ready to
    /// communicate with the MCU.
    pub async fn connect<R>(stream: R) -> Result<Self, McuConnectionError>
    where
        R: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (transport, inbox) = Transport::connect(stream).await;

        let (exit_tx, exit_rx) = oneshot::channel();
        let inner = Arc::new(McuConnectionInner {
            dictionary: OnceCell::new(),
            handlers: Handlers::default(),
            good_types: Mutex::default(),
        });

        let receiver_inner = inner.clone();
        let receiver = tokio::spawn(async move {
            let _ = exit_tx.send(receiver_inner.receive_loop(inbox).await);
        });

        let conn = McuConnection {
            inner,
            transport,
            _receiver: receiver,
            exit_code: ExitCode::Waiting(exit_rx),
        };

        let mut identify_data = Vec::new();
        let identify_start = Instant::now();
        loop {
            let data = conn
                .send_receive(
                    Identify::encode(identify_data.len() as u32, 40),
                    IdentifyResponse,
                )
                .await;
            let mut data = match data {
                Ok(data) => data,
                Err(e) => {
                    if identify_start.elapsed() > Duration::from_secs(10) {
                        return Err(McuConnectionError::DictionaryFetch(Box::new(e)));
                    }
                    continue;
                }
            };
            if data.offset as usize == identify_data.len() {
                if data.data.is_empty() {
                    break;
                }
                identify_data.append(&mut data.data);
            }
        }
        let mut decoder = flate2::read::ZlibDecoder::new(identify_data.as_slice());
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .map_err(|err| McuConnectionError::DictionaryFetch(Box::new(err)))?;
        let raw_dict: RawDictionary = serde_json::from_slice(&buf)
            .map_err(|err| McuConnectionError::DictionaryFetch(Box::new(err)))?;
        let dict =
            Dictionary::from_raw_dictionary(raw_dict).map_err(McuConnectionError::Dictionary)?;
        conn.inner
            .dictionary
            .set(dict)
            .expect("Dictionary already set");

        Ok(conn)
    }

    fn verify_command_matches<C: Message>(&self) -> Result<(), McuConnectionError> {
        // Special handling for identify/identify_response
        if C::get_id(None) == Some(0) || C::get_id(None) == Some(1) {
            return Ok(());
        }

        if self
            .inner
            .good_types
            .lock()
            .unwrap()
            .contains(&TypeId::of::<C>())
        {
            return Ok(());
        }

        // We can only get here if we _have_ a dictionary as only Identify/IdentifyResponse are
        // tested before then.
        let dictionary = self.inner.dictionary.get().unwrap();
        let id = C::get_id(Some(dictionary))
            .ok_or_else(|| McuConnectionError::UnknownMessageId(C::get_name()))?;

        // Must exist because we know the tag
        let parser = dictionary.message_parsers.get(&id).unwrap();
        let remote_fields = parser.fields.iter().map(|(s, t)| (s.as_str(), *t));
        let local_fields = C::fields().into_iter();

        if !remote_fields.eq(local_fields) {
            return Err(McuConnectionError::CommandMismatch(
                C::get_name(),
                format_command_args(parser.fields.iter().map(|(s, t)| (s.as_str(), *t))),
                format_command_args(C::fields().into_iter()),
            ));
        }

        self.inner
            .good_types
            .lock()
            .unwrap()
            .insert(TypeId::of::<C>());

        Ok(())
    }

    fn encode_command<C: Message>(
        &self,
        command: EncodedMessage<C>,
    ) -> Result<Vec<u8>, McuConnectionError> {
        let id = command
            .message_id(self.inner.dictionary.get())
            .ok_or_else(|| McuConnectionError::UnknownMessageId(command.message_name()))?;
        let mut payload = command.payload;
        payload[0] = id;
        Ok(payload)
    }

    /// Sends a command to the MCU
    pub async fn send<C: Message>(
        &self,
        command: EncodedMessage<C>,
    ) -> Result<(), McuConnectionError> {
        let cmd = self.encode_command(command)?;
        self.transport
            .send(&cmd)
            .map_err(|e| McuConnectionError::Transport(TransportError::Transmitter(e)))
    }

    async fn send_receive_impl<C: Message, R: Message>(
        &self,
        command: EncodedMessage<C>,
        reply: R,
        oid: Option<u8>,
    ) -> Result<R::PodOwned, SendReceiveError> {
        struct RespHandler<R: Message>(
            Option<oneshot::Sender<Result<R::PodOwned, McuConnectionError>>>,
        );

        impl<R: Message> Handler for RespHandler<R> {
            fn handle(&mut self, data: &mut &[u8]) -> HandlerResult {
                if let Some(tx) = self.0.take() {
                    let _ = match R::decode(data) {
                        Ok(msg) => tx.send(Ok(msg.into())),
                        Err(e) => tx.send(Err(McuConnectionError::DecodingError(e))),
                    };
                }
                HandlerResult::Deregister
            }
        }

        let cmd = self.encode_command(command)?;

        self.verify_command_matches::<C>()?;
        self.verify_command_matches::<R>()?;

        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<R::PodOwned, _>>();
        self.register_raw_response(reply, oid, Box::new(RespHandler::<R>(Some(tx))))?;

        let mut retry_delay = 0.01;
        for _retry in 0..=5 {
            self.transport
                .send(&cmd)
                .map_err(|e| McuConnectionError::Transport(TransportError::Transmitter(e)))?;

            let sleep = tokio::time::sleep(Duration::from_secs_f32(retry_delay));
            tokio::pin!(sleep);

            select! {
                reply = &mut rx => {
                    return match reply {
                        Ok(Err(e)) => Err(SendReceiveError::McuConnection(e)),
                        Ok(Ok(v)) => Ok(v),
                        Err(_) => Err(SendReceiveError::Timeout)
                    };
                },
                _ = &mut sleep => {},
            }

            retry_delay *= 2.0;
        }

        Err(SendReceiveError::Timeout)
    }

    /// Sends a message to the MCU, and await a reply
    ///
    /// This sends a message to the MCU and awaits a reply matching the given `reply`.
    /// This version works with replies that have an `oid` field.
    pub async fn send_receive_oid<C: Message, R: Message + WithOid>(
        &self,
        command: EncodedMessage<C>,
        reply: R,
        oid: u8,
    ) -> Result<R::PodOwned, SendReceiveError> {
        self.send_receive_impl(command, reply, Some(oid)).await
    }

    /// Sends a message to the MCU, and await a reply
    ///
    /// This sends a message to the MCU and awaits a reply matching the given `reply`.
    /// This version works with replies that do not have an `oid` field.
    pub async fn send_receive<C: Message, R: Message + WithoutOid>(
        &self,
        command: EncodedMessage<C>,
        reply: R,
    ) -> Result<R::PodOwned, SendReceiveError> {
        self.send_receive_impl(command, reply, None).await
    }

    fn register_raw_response<R: Message>(
        &self,
        _reply: R,
        oid: Option<u8>,
        handler: Box<dyn Handler>,
    ) -> Result<RegisteredResponseHandle, McuConnectionError> {
        let id = R::get_id(self.inner.dictionary.get())
            .ok_or_else(|| McuConnectionError::UnknownMessageId(R::get_name()))?;
        Ok(self.inner.handlers.register(id, oid, handler))
    }

    fn register_response_impl<R: Message>(
        &self,
        reply: R,
        oid: Option<u8>,
    ) -> Result<UnboundedReceiver<R::PodOwned>, McuConnectionError> {
        struct RespHandler<R: Message>(UnboundedSender<R::PodOwned>, Option<oneshot::Sender<()>>);

        impl<R: Message> Drop for RespHandler<R> {
            fn drop(&mut self) {
                self.1.take();
            }
        }

        impl<R: Message> Handler for RespHandler<R> {
            fn handle(&mut self, data: &mut &[u8]) -> HandlerResult {
                let msg = R::decode(data)
                    .expect("Parser should already have assured this could parse")
                    .into();
                match self.0.send(msg) {
                    Ok(_) => HandlerResult::Continue,
                    Err(_) => HandlerResult::Deregister,
                }
            }
        }

        let (tx, rx) = unbounded_channel();
        let tx_closed = tx.clone();
        let (closer_tx, closer_rx) = oneshot::channel();
        let uniq = self.register_raw_response(
            reply,
            oid,
            Box::new(RespHandler::<R>(tx, Some(closer_tx))),
        )?;

        // Safe because register_raw_response already verified this
        let id = R::get_id(self.inner.dictionary.get()).unwrap();

        let inner = self.inner.clone();
        spawn(async move {
            select! {
                _ = tx_closed.closed() => {},
                _ = closer_rx => {},
            }
            inner.handlers.remove_handler(id, oid, Some(uniq));
        });

        Ok(rx)
    }

    /// Registers a subscriber for a reply message
    ///
    /// Received replies matching the type will be delivered to the returned channel.
    /// To end the subscription, simply drop the returned receiver.
    /// This version works with replies that have an `oid` field.
    pub fn register_response_oid<R: Message + WithOid>(
        &self,
        reply: R,
        oid: u8,
    ) -> Result<UnboundedReceiver<R::PodOwned>, McuConnectionError> {
        self.register_response_impl(reply, Some(oid))
    }

    /// Registers a subscriber for a reply message
    ///
    /// Received replies matching the type will be delivered to the returned channel.
    /// To end the subscription, simply drop the returned receiver.
    /// This version works with replies that do not have an `oid` field.
    pub fn register_response<R: Message + WithoutOid>(
        &self,
        reply: R,
    ) -> Result<UnboundedReceiver<R::PodOwned>, McuConnectionError> {
        self.register_response_impl(reply, None)
    }

    /// Returns a reference the dictionary the MCU returned during initial handshake
    pub fn dictionary(&self) -> &Dictionary {
        self.inner.dictionary.get().unwrap()
    }

    /// Closes the transport and ends all subscriptions. Returns when the transport is closed.
    pub async fn close(self) {
        self.transport.close().await;
    }

    /// Waits for the connection to close, returning the error that closed it if any.
    pub async fn closed(&mut self) -> &Result<(), McuConnectionError> {
        if let ExitCode::Waiting(chan) = &mut self.exit_code {
            self.exit_code = ExitCode::Exited(match chan.await {
                Ok(r) => r,
                Err(_) => Ok(()),
            });
        }
        match self.exit_code {
            ExitCode::Exited(ref val) => val,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
enum HandlerResult {
    Continue,
    Deregister,
}
