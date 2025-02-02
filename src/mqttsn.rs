use heapless::String;
use crate::socket::{SendBytes, ReceiveBytes, SocketError};
use mqtt_sn::defs::*;
use byte::{TryRead, TryWrite};
use embassy_sync::pubsub::subscriber::DynSubscriber;
use embassy_sync::pubsub::publisher::DynPublisher;
use embassy_time::{with_timeout, Duration, TimeoutError};
use crate::topics::Topics;

#[cfg(feature = "std")]
use log::*;

#[cfg(feature = "no_std")]
use defmt::*;

const T_RETRY: u8 = 10;
const N_RETRY: u8 = 10;

type Error = MqttSnClientError;

#[derive(Hash, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
#[repr(u8)]
pub enum TopicIdType {
    Id,
    PreDef,
    Short
}

pub enum AckResult {
    Success,
    TopicId(u16),
    None
}

impl TryFrom<u8> for TopicIdType {
    type Error = MqttSnClientError;
    fn try_from(i: u8) -> Result<Self, Error> {
        match i {
            0 => Ok(TopicIdType::Id),
            1 => Ok(TopicIdType::PreDef),
            2 => Ok(TopicIdType::Short),
            _ => Err(Error::ParseError)
        }
    }
}

pub struct MqttSnClient<S> {
    client_id: ClientId,
    msg_id: MsgId,
    socket: S,
    topics: Topics,
    rx: DynSubscriber<'static, MqttMessage>,
    tx: DynPublisher<'static, MqttMessage>,
    buffer: [u8; 1024],
}

impl<S> MqttSnClient<S>
where
    S: SendBytes + ReceiveBytes
{
    pub fn new(
        client_id: &str,
        rx: DynSubscriber<'static, MqttMessage>,
        tx: DynPublisher<'static, MqttMessage>,
        socket: S
    ) -> Result<MqttSnClient<S>, Error> {
        Ok(MqttSnClient {
            client_id: client_id.into(),
            msg_id: MsgId {last_id: 0},
            topics: Topics::new(),
            socket, rx, tx,
            buffer: [0u8; 1024]
        })
    }

    pub async fn run(
        &mut self,
        sleep: u16,
    ) {
        loop {
            match with_timeout(
                Duration::from_secs(sleep.into()),
                self.rx.next_message_pure()
            ).await {
                Ok(msg) => {
                    // Handle message received from the user (via DynSubscriber)
                    self.connect(sleep).await.unwrap();
                    self.publish(msg).await.unwrap();
                    // Publish aditional msg if queued
                    while let Some(msg) = self.rx.try_next_message_pure() {
                        self.publish(msg).await.unwrap();
                    }
                    self.disconnect(Some(sleep)).await.unwrap();
                },
                _ => {
                    self.ping().await.unwrap();
                }
            }
        }
    }

    pub async fn receive(&mut self) -> Result<Option<Message>, Error> {
        loop {
            match Message::try_read(
                with_timeout(
                    Duration::from_secs(T_RETRY.into()),
                    self.socket.recv(&mut self.buffer)).await??, ()
                ) {
                Ok((Message::Publish(msg), _)) => self.recieve_publish(msg).await?,
                Ok((msg, _)) => return Ok(Some(msg)),
                _ => return Err(MqttSnClientError::AckError)
            }
        }
    }

    async fn recieve_publish(&mut self, msg: Publish) -> Result<(), Error> {
        let msg = MqttMessage::from_publish(msg, &self.topics)?;
        if msg.qos > Some(0) {
            if let Some(ack) = msg.get_ack() {
                self.send(Message::PubAck(ack)).await?;
            }
        }
        self.tx.publish_immediate(msg);
        Ok(())
    }

    pub async fn send(&mut self, msg: Message) -> Result<(), Error> {
        let len = msg.try_write(&mut self.buffer, ())?;
        self.socket.send(&self.buffer[..len]).await?;
        Ok(())
    }

    pub async fn send_ack<F>(
        &mut self, packet: Message, ack_handler: F
    ) -> Result<AckResult, Error>
    where
        F: Fn(Message) -> AckResult
    {
        let len = packet.try_write(&mut self.buffer, ())?;
        
        for _ in 1..N_RETRY {
            self.socket.send(&self.buffer[..len]).await?;

            match with_timeout(
                Duration::from_secs(T_RETRY.into()),
                async {
                    loop{
                        if let Ok(Some(msg)) = self.receive().await {
                            match ack_handler(msg) {
                                AckResult::None => (),
                                result => return result,
                            }
                        }
                    }
                }).await
            {
                Ok(result) => return Ok(result),
                _ => ()
            }
        }
        Err(Error::AckError)
    }

    pub async fn ping(&mut self) -> Result<(), Error>{
        debug!("ping");
        let packet = Message::PingReq(PingReq {
            client_id: self.client_id.clone()
        });
        let ack_handler = |msg| {
            match msg {
                Message::PingResp(_) => AckResult::Success,
                _ => AckResult::None
            }
        };

        self.send_ack(packet, ack_handler).await?;
        Ok(())
    }

    pub async fn publish(&mut self, msg: MqttMessage) -> Result<(), Error> {
        debug!("publish");
        let mut flags = Flags::default();
        if let Some(qos) = msg.qos {
            flags.set_qos(qos)
        }

        let topic_id;
        if let Some((topic_type, id)) = self.topics.get_by_topic(&msg.topic) {
            topic_id = *id;
            flags.set_topic_id_type(*topic_type as u8);
        } else {
            topic_id = self.register(&msg.topic).await?;
            self.topics.insert(msg.topic, TopicIdType::Id, topic_id)?;
        }
        let next_msg_id = self.msg_id.next();

        let mut data = PublishData::new();
        data.push_str(&msg.payload)?;
        let packet = Message::Publish(
            Publish {flags, topic_id, msg_id: next_msg_id, data}
        );

        // Get ACK for QoS 1 & 2
        match msg.qos {
            Some(qos) if qos > 0 => {
                let ack_handler = |msg| {
                    match msg {
                        Message::PubAck(PubAck {
                            msg_id, code: ReturnCode::Accepted, ..
                        }) if msg_id == next_msg_id => AckResult::Success,
                        _ => AckResult::None
                    }
                };
                self.send_ack(packet, ack_handler).await?;
            },
            _ => {
                self.send(packet.into()).await?;
            },
        }
        Ok(())
    }

    async fn register(&mut self, topic: &String<256>) -> Result<u16, Error> {
        debug!("register");
        let msg_id = self.msg_id.next();
        let packet = Message::Register(Register {
            topic_id: 0,
            msg_id,
            topic_name: TopicName::from(&topic)
        });
        let ack_handler = |msg| {
            match msg {
                Message::RegAck(RegAck {
                    topic_id, code: ReturnCode::Accepted, ..
                }) => AckResult::TopicId(topic_id),
                _ => AckResult::None
            }
        };
        
        match self.send_ack(packet, ack_handler).await {
            Ok(AckResult::TopicId(id)) => return Ok(id),
            _ => Err(Error::AckError)
        }
    }

    pub async fn connect(&mut self, duration: u16) -> Result<(), Error> {
        debug!("connect");
        let packet = Message::Connect(Connect {
            flags: Flags::default(),
            duration,
            client_id: self.client_id.clone()
        });
        let ack_handler = |msg| {
            match msg {
                Message::ConnAck(
                    ConnAck{code: ReturnCode::Accepted}
                ) => AckResult::Success,
                _ => AckResult::None
            }
        };

        self.send_ack(packet, ack_handler).await?;
        Ok(())
    }

    pub async fn subscribe(&mut self, topic: &str) -> Result<(), Error> {
        debug!("subscribe");
        let mut flags = Flags::default();
        let topic_id;
        let topic = String::<256>::try_from(topic)?;
        if let Some((topic_type, id)) = self.topics.get_by_topic(&topic) {
            topic_id = *id;
            flags.set_topic_id_type(*topic_type as u8);
        } else {
            topic_id = self.register(&topic).await?;
            self.topics.insert(topic, TopicIdType::Id, topic_id)?;
        }
        let msg_id = self.msg_id.next();
        let mut flags = Flags::default();
        flags.set_topic_id_type(1);

        let packet = Message::Subscribe(Subscribe {
            flags,
            msg_id,
            topic: TopicNameOrId::Id(topic_id),
        });
        let ack_handler = |msg| {
            match msg {
                Message::SubAck(SubAck {
                    code: ReturnCode::Accepted, ..
                }) => AckResult::Success,
                _ => AckResult::None
            }
        };

        self.send_ack(packet, ack_handler).await?;
        Ok(())
    }

    /// If duration is set, then client will go to sleep, with keep-alive < duration
    pub async fn disconnect(&mut self, duration: Option<u16>) -> Result<(), Error> {
        debug!("disconnect");
        let packet = Message::Disconnect(Disconnect {
            duration
        });
        let ack_handler = |msg| {
            match msg {
                Message::Disconnect(_) => AckResult::Success,
                _ => AckResult::None
            }
        };

        self.send_ack(packet, ack_handler).await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MqttMessage {
    topic_id: Option<u16>,
    msg_id: Option<u16>,
    qos: Option<u8>,
    pub topic: String<256>,
    pub payload: String<256>,
}

impl MqttMessage {
    pub fn new(
        topic: &str,
        payload: &str,
        qos: Option<u8>
    ) -> Result<Self, Error> {
        Ok(Self {
            topic_id: None,
            msg_id: None,
            topic: String::try_from(topic)?,
            payload: String::try_from(payload)?,
            qos
        })
    }
    fn from_publish(
        msg: Publish,
        topics: &Topics,
    ) -> Result<Self, Error> {
        Ok(Self {
            topic_id: Some(msg.topic_id),
            msg_id: Some(msg.msg_id),
            qos: Some(msg.flags.qos()),
            topic: String::try_from(topics.get_by_id(msg.topic_id)?)?,
            payload: String::try_from(msg.data.as_str())?,
        })
    }
    pub fn get_ack(&self) -> Option<PubAck> {
        if let (Some(topic_id), Some(msg_id), Some(_)) = (self.topic_id, self.msg_id, self.qos) {
            return Some(PubAck {
                topic_id, msg_id,
                code: ReturnCode::Accepted
            })
        }
        None
    }
}

pub struct MsgId {
    last_id: u16
}

impl MsgId {
    fn next(&mut self) -> u16 {
        self.last_id = self.last_id.wrapping_add(1);
        self.last_id
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "no_std", derive(Format))]
pub enum MqttSnClientError {
    ModemError,
    SocketError,
    CodecError,
    AckError,
    UnknownError,
    ParseError,
    TopicNotRegistered,
    TopicFailedInsert,
    NoPingResponse,
}

impl From<SocketError> for MqttSnClientError {
    fn from(_e: SocketError) -> Self {
        MqttSnClientError::SocketError
    }
}

impl From<byte::Error> for MqttSnClientError {
    fn from(_e: byte::Error) -> Self {
        MqttSnClientError::CodecError
    }
}

impl From<TimeoutError> for MqttSnClientError {
    fn from(_e: TimeoutError) -> Self {
        MqttSnClientError::AckError
    }
}

impl From<()> for MqttSnClientError {
    fn from(_e: ()) -> Self {
        MqttSnClientError::UnknownError
    }
}
