use std::{collections::VecDeque, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    pin, select,
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::{spawn, JoinHandle},
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::trace;

const MESSAGE_HEADER_SIZE: usize = 2;
const MESSAGE_TRAILER_SIZE: usize = 3;
pub const MESSAGE_LENGTH_MIN: usize = MESSAGE_HEADER_SIZE + MESSAGE_TRAILER_SIZE;
pub const MESSAGE_LENGTH_MAX: usize = 64;
pub const MESSAGE_LENGTH_PAYLOAD_MAX: usize = MESSAGE_LENGTH_MAX - MESSAGE_LENGTH_MIN;
const MESSAGE_POSITION_SEQ: usize = 1;
const MESSAGE_TRAILER_CRC: usize = 3;
const MESSAGE_VALUE_SYNC: u8 = 0x7E;
const MESSAGE_DEST: u8 = 0x10;
const MESSAGE_SEQ_MASK: u8 = 0x0F;

#[derive(thiserror::Error, Debug)]
pub enum ReceiverError {
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum TransmitterError {
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("connection closed")]
    ConnectionClosed,
}

#[derive(Debug)]
pub(crate) struct Transport {
    _task_rdr: JoinHandle<()>,
    _task_wr: JoinHandle<()>,
    task_inner: JoinHandle<()>,
    outbox_tx: UnboundedSender<TransportCommand>,
}

#[derive(Debug)]
enum TransportCommand {
    SendMessage(Vec<u8>),
    Exit,
}

pub(crate) type TransportReceiver = UnboundedReceiver<Result<Vec<u8>, TransportError>>;

impl Transport {
    pub(crate) async fn connect<R>(stream: R) -> (Transport, TransportReceiver)
    where
        R: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (rdr, wr) = tokio::io::split(stream);

        let (raw_send_tx, raw_send_rx) = unbounded_channel();
        let (raw_recv_tx, raw_recv_rx) = unbounded_channel();
        let (app_inbox_tx, app_inbox_rx) = unbounded_channel();
        let (app_outbox_tx, app_outbox_rx) = unbounded_channel();

        let cancel_token = CancellationToken::new();

        let cancel = cancel_token.clone();
        let ait = app_inbox_tx.clone();
        let task_rdr = spawn(async move {
            if let Err(e) = LowlevelReader::run(raw_recv_tx, rdr, cancel).await {
                let _ = ait.send(Err(TransportError::Receiver(e)));
            }
        });

        let cancel = cancel_token.clone();
        let ait = app_inbox_tx.clone();
        let task_wr = spawn(async move {
            if let Err(e) = LowlevelWriter::run(raw_send_rx, wr, cancel).await {
                let _ = ait.send(Err(TransportError::Transmitter(e)));
            }
        });

        let task_inner = spawn(async move {
            let mut ts = TransportState::new(
                raw_recv_rx,
                raw_send_tx,
                app_inbox_tx,
                app_outbox_rx,
                cancel_token,
            );
            if let Err(e) = ts.protocol_handler().await {
                let _ = ts.app_inbox.send(Err(e));
            }
        });

        (
            Transport {
                _task_rdr: task_rdr,
                _task_wr: task_wr,
                task_inner,
                outbox_tx: app_outbox_tx,
            },
            app_inbox_rx,
        )
    }

    pub(crate) fn send(&self, msg: &[u8]) -> Result<(), TransmitterError> {
        self.outbox_tx
            .send(TransportCommand::SendMessage(msg.into()))
            .map_err(|_| TransmitterError::ConnectionClosed)
    }

    pub(crate) async fn close(self) {
        let _ = self.outbox_tx.send(TransportCommand::Exit);
        let _ = self.task_inner.await;
    }
}

struct LowlevelReader<R> {
    rdr: BufReader<R>,
    synced: bool,
}

impl<R: AsyncRead + Unpin> LowlevelReader<R> {
    async fn read_frame(&mut self) -> Result<Option<Frame>, ReceiverError> {
        let mut buf = [0u8; MESSAGE_LENGTH_MAX];
        let next_byte = self.rdr.read_u8().await?;
        if next_byte == MESSAGE_VALUE_SYNC {
            if !self.synced {
                self.synced = true;
            }
            return Ok(None);
        }
        if !self.synced {
            return Ok(None);
        }

        let receive_time = Instant::now();
        let len = next_byte as usize;

        if !(MESSAGE_LENGTH_MIN..=MESSAGE_LENGTH_MAX).contains(&len) {
            self.synced = false;
            return Ok(None);
        }

        self.rdr.read_exact(&mut buf[1..len]).await?;
        buf[0] = len as u8;
        let buf = &buf[..len];
        trace!(frame = ?buf, "Received frame");

        let seq = buf[MESSAGE_POSITION_SEQ];
        if seq & !MESSAGE_SEQ_MASK != MESSAGE_DEST {
            self.synced = false;
            return Ok(None);
        }

        let actual_crc = crc16(&buf[0..len - MESSAGE_TRAILER_SIZE]);
        let frame_crc = (buf[len - MESSAGE_TRAILER_CRC] as u16) << 8
            | (buf[len - MESSAGE_TRAILER_CRC + 1] as u16);
        if frame_crc != actual_crc {
            self.synced = false;
            return Ok(None);
        }

        Ok(Some(Frame {
            receive_time,
            sequence: seq & MESSAGE_SEQ_MASK,
            payload: buf[MESSAGE_HEADER_SIZE..len - MESSAGE_TRAILER_SIZE].into(),
        }))
    }

    async fn run(
        outbox: UnboundedSender<Frame>,
        rdr: R,
        cancel_token: CancellationToken,
    ) -> Result<(), ReceiverError>
    where
        R: AsyncRead + Unpin,
    {
        let mut state = Self {
            rdr: BufReader::new(rdr),
            synced: false,
        };

        loop {
            match state.read_frame().await {
                Ok(None) => {}
                Ok(Some(frame)) => {
                    if outbox.send(frame).is_err() {
                        break Ok(());
                    }
                }
                Err(_) if cancel_token.is_cancelled() => break Ok(()),
                Err(e) => break Err(e),
            }
        }
    }
}

fn crc16(buf: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for b in buf {
        let b = *b ^ ((crc & 0xFF) as u8);
        let b = b ^ (b << 4);
        let b16 = b as u16;
        crc = (b16 << 8 | crc >> 8) ^ (b16 >> 4) ^ (b16 << 3);
    }
    crc
}

#[derive(Debug)]
struct Frame {
    receive_time: Instant,
    sequence: u8,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct InflightFrame {
    sent_at: Instant,
    #[allow(dead_code)]
    sequence: u64,
    payload: Arc<Vec<u8>>,
    is_retransmit: bool,
}

#[derive(thiserror::Error, Debug)]
pub enum MessageEncodeError {
    #[error("message would exceed the maximum packet length of {MESSAGE_LENGTH_MAX} bytes")]
    MessageTooLong,
}

fn encode_frame(sequence: u64, payload: &[u8]) -> Result<Vec<u8>, MessageEncodeError> {
    let len = MESSAGE_LENGTH_MIN + payload.len();
    if len > MESSAGE_LENGTH_MAX {
        return Err(MessageEncodeError::MessageTooLong);
    }
    let mut buf = Vec::with_capacity(len);
    buf.push(len as u8);
    buf.push(MESSAGE_DEST | ((sequence as u8) & MESSAGE_SEQ_MASK));
    buf.extend_from_slice(payload);
    let crc = crc16(&buf[0..len - MESSAGE_TRAILER_SIZE]);
    buf.push(((crc >> 8) & 0xFF) as u8);
    buf.push((crc & 0xFF) as u8);
    buf.push(MESSAGE_VALUE_SYNC);
    Ok(buf)
}

struct LowlevelWriter {}

impl LowlevelWriter {
    async fn run<W>(
        mut inbox: UnboundedReceiver<Arc<Vec<u8>>>,
        mut wr: W,
        cancel_token: CancellationToken,
    ) -> Result<(), TransmitterError>
    where
        W: AsyncWrite + Unpin,
    {
        loop {
            select! {
                msg = inbox.recv() => {
                    let msg = match msg {
                        Some(msg) => msg,
                        None => break,
                    };
                    trace!(payload = ?msg, "Sent frame");
                    wr.write_all(&msg).await?;
                    wr.flush().await?;
                }

                _ = cancel_token.cancelled() => {
                    break;
                }
            }
        }

        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum TransportError {
    #[error("message encoding failed: {0}")]
    MessageEncode(#[from] MessageEncodeError),
    #[error("receiver error: {0}")]
    Receiver(#[from] ReceiverError),
    #[error("transmitter error: {0}")]
    Transmitter(#[from] TransmitterError),
}

const MIN_RTO: f32 = 0.025;
const MAX_RTO: f32 = 5.000;

#[derive(Debug)]
struct RttState {
    srtt: f32,
    rttvar: f32,
    rto: f32,
}

impl RttState {
    fn new() -> Self {
        Self {
            srtt: 0.0,
            rttvar: 0.0,
            rto: MIN_RTO,
        }
    }

    fn rto(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f32(self.rto)
    }

    fn update(&mut self, rtt: std::time::Duration) {
        let r = rtt.as_secs_f32();
        if self.srtt == 0.0 {
            self.rttvar = r / 2.0;
            self.srtt = r * 10.0; // Klipper uses this, we'll copy it
        } else {
            self.rttvar = (3.0 * self.rttvar + (self.srtt - r).abs()) / 4.0;
            self.srtt = (7.0 * self.srtt + r) / 8.0;
        }
        let rttvar4 = (self.rttvar * 4.0).max(0.001);
        self.rto = (self.srtt + rttvar4).clamp(MIN_RTO, MAX_RTO);
    }
}

#[derive(Debug)]
struct TransportState {
    link_inbox: UnboundedReceiver<Frame>,
    link_outbox: UnboundedSender<Arc<Vec<u8>>>,
    app_inbox: UnboundedSender<Result<Vec<u8>, TransportError>>,
    app_outbox: UnboundedReceiver<TransportCommand>,
    cancel: CancellationToken,

    is_synchronized: bool,
    rtt_state: RttState,
    receive_sequence: u64,
    send_sequence: u64,
    last_ack_sequence: u64,
    ignore_nak_seq: u64,
    retransmit_seq: u64,
    retransmit_now: bool,

    corked_until: Option<Instant>,

    inflight_messages: VecDeque<InflightFrame>,
    ready_messages: MessageQueue<Vec<u8>>,
}

impl TransportState {
    fn new(
        link_inbox: UnboundedReceiver<Frame>,
        link_outbox: UnboundedSender<Arc<Vec<u8>>>,
        app_inbox: UnboundedSender<Result<Vec<u8>, TransportError>>,
        app_outbox: UnboundedReceiver<TransportCommand>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            link_inbox,
            link_outbox,
            app_inbox,
            app_outbox,
            cancel,

            is_synchronized: false,
            rtt_state: RttState::new(),
            receive_sequence: 1,
            send_sequence: 1,
            last_ack_sequence: 0,
            ignore_nak_seq: 0,
            retransmit_seq: 0,
            retransmit_now: false,

            corked_until: None,

            inflight_messages: VecDeque::new(),
            ready_messages: MessageQueue::new(),
        }
    }

    fn update_receive_seq(&mut self, receive_time: Instant, sequence: u64) {
        let mut sent_seq = self.receive_sequence;

        loop {
            if let Some(msg) = self.inflight_messages.pop_front() {
                sent_seq += 1;
                if sequence == sent_seq {
                    // Found the matching sent message
                    if !msg.is_retransmit {
                        let elapsed = receive_time.saturating_duration_since(msg.sent_at);
                        self.rtt_state.update(elapsed);
                    }
                    break;
                }
            } else {
                // Ack with no outstanding messages, happens during connection init
                self.send_sequence = sequence;
                break;
            }
        }

        self.receive_sequence = sequence;
        self.is_synchronized = true;
    }

    fn handle_frame(&mut self, frame: Frame) {
        let rseq = self.receive_sequence;
        let mut sequence = (rseq & !(MESSAGE_SEQ_MASK as u64)) | (frame.sequence as u64);
        if sequence < rseq {
            sequence += (MESSAGE_SEQ_MASK as u64) + 1;
        }
        if !self.is_synchronized || sequence != rseq {
            if sequence > self.send_sequence && self.is_synchronized {
                // Ack for unsent message
                return;
            }
            self.update_receive_seq(frame.receive_time, sequence);
        }

        if frame.payload.is_empty() {
            if self.last_ack_sequence < sequence {
                self.last_ack_sequence = sequence;
            } else if sequence > self.ignore_nak_seq && !self.inflight_messages.is_empty() {
                // Trigger retransmit from NAK
                self.retransmit_now = true;
            }
        } else {
            // Data message, we deliver this directly to the application as the MCU can't actually
            // retransmit anyway.
            let _ = self.app_inbox.send(Ok(frame.payload));
        }
    }

    fn can_send(&self) -> bool {
        self.corked_until.is_none() && self.inflight_messages.len() < 12
    }

    fn send_new_frame(&mut self, mut initial: Vec<u8>) -> Result<(), TransportError> {
        while let Some(next) = self.ready_messages.try_peek() {
            if initial.len() + next.len() <= MESSAGE_LENGTH_PAYLOAD_MAX {
                // Add to the end of the message. Unwrap is safe because we already peeked.
                let mut next = self.ready_messages.try_recv().unwrap();
                initial.append(&mut next);
            } else {
                break;
            }
        }
        let frame = Arc::new(encode_frame(self.send_sequence, &initial)?);
        self.send_sequence += 1;
        self.inflight_messages.push_back(InflightFrame {
            sent_at: Instant::now(),
            sequence: self.send_sequence,
            payload: frame.clone(),
            is_retransmit: false,
        });
        let _ = self.link_outbox.send(frame);
        Ok(())
    }

    fn send_more_frames(&mut self) -> Result<(), TransportError> {
        while self.can_send() && self.ready_messages.try_peek().is_some() {
            let msg = self.ready_messages.try_recv().unwrap();
            self.send_new_frame(msg)?;
        }
        Ok(())
    }

    fn retransmit_pending(&mut self) {
        let len: usize = self
            .inflight_messages
            .iter()
            .map(|msg| msg.payload.len())
            .sum();
        let mut buf = Vec::with_capacity(1 + len);
        buf.push(MESSAGE_VALUE_SYNC);
        let now = Instant::now();
        for msg in self.inflight_messages.iter_mut() {
            buf.extend_from_slice(&msg.payload);
            msg.is_retransmit = true;
            msg.sent_at = now;
        }
        let _ = self.link_outbox.send(Arc::new(buf));

        if self.retransmit_now {
            self.ignore_nak_seq = self.receive_sequence;
            if self.receive_sequence < self.retransmit_seq {
                self.ignore_nak_seq = self.retransmit_seq;
            }
            self.retransmit_now = false;
        } else {
            self.rtt_state.rto = (self.rtt_state.rto * 2.0).clamp(MIN_RTO, MAX_RTO);
            self.ignore_nak_seq = self.send_sequence;
        }
        self.retransmit_seq = self.send_sequence;
    }

    async fn protocol_handler(&mut self) -> Result<(), TransportError> {
        loop {
            if self.retransmit_now {
                self.retransmit_pending();
            }

            let retransmit_deadline = self
                .inflight_messages
                .front()
                .map(|msg| msg.sent_at + self.rtt_state.rto());
            let retransmit_timeout: futures::future::OptionFuture<_> =
                retransmit_deadline.map(sleep_until).into();
            pin!(retransmit_timeout);

            let corked_timeout: futures::future::OptionFuture<_> =
                self.corked_until.map(sleep_until).into();
            pin!(corked_timeout);

            select! {
                frame = self.link_inbox.recv() => {
                    let frame = match frame {
                        Some(frame) => frame,
                        None => break,
                    };
                    self.handle_frame(frame);
                },

                msg = self.app_outbox.recv() => {
                    match msg {
                        Some(TransportCommand::SendMessage(msg)) => {
                            self.ready_messages.send(msg);
                        },
                        Some(TransportCommand::Exit) => {
                            self.cancel.cancel();
                        }
                        None => break,
                    };
                },

                _ = self.ready_messages.recv_peek(), if self.can_send() => {
                    self.send_more_frames()?;
                },

                _ = &mut retransmit_timeout, if retransmit_deadline.is_some() => {
                    self.retransmit_now = true;
                },

                _ = &mut corked_timeout, if self.corked_until.is_some() => {
                    // Timeout for when we are able to send again
                }

                _ = self.cancel.cancelled() => {
                    break;
                },
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct MessageQueue<T> {
    sender: UnboundedSender<T>,
    receiver: UnboundedReceiver<T>,
    peeked: Option<T>,
}

impl<T> MessageQueue<T> {
    fn new() -> Self {
        let (sender, receiver) = unbounded_channel();
        Self {
            sender,
            receiver,
            peeked: None,
        }
    }

    async fn recv_peek(&mut self) -> Option<&T> {
        if self.peeked.is_some() {
            self.peeked.as_ref()
        } else {
            match self.receiver.recv().await {
                Some(msg) => {
                    self.peeked = Some(msg);
                    self.peeked.as_ref()
                }
                None => None,
            }
        }
    }

    fn try_recv(&mut self) -> Option<T> {
        if let Some(msg) = self.peeked.take() {
            Some(msg)
        } else {
            self.receiver.try_recv().ok()
        }
    }

    fn try_peek(&mut self) -> Option<&T> {
        if self.peeked.is_some() {
            self.peeked.as_ref()
        } else {
            match self.receiver.try_recv() {
                Ok(msg) => {
                    self.peeked = Some(msg);
                    self.peeked.as_ref()
                }
                Err(_) => None,
            }
        }
    }

    fn send(&mut self, msg: T) {
        let _ = self.sender.send(msg);
    }
}
