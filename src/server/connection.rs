mod connection_hdl;
mod internal_hdl;
mod sender;
mod receiver;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use futures_util::StreamExt;
use futures_util::stream::{SplitSink, SplitStream};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{ mpsc, oneshot };
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, warn};

use crate::scheme;
use crate::scheme::internal;

pub use connection_hdl::ConnectionHdl;
use sender::SenderHdl;
use receiver::ReceiverHdl;
use crate::server::connection::internal_hdl::{InternalHdl, InternalMessage};

enum ConnectionMessage<OurReq, OurRep, OurEvent, TheirRep> {
    Close,
    Request{ data: OurReq, tx: oneshot::Sender<TheirRep> },
    Reply(OurRep),
    Event(OurEvent),
    Send(internal::Message<OurReq, OurRep, OurEvent>),
}

struct Connection<
    OurReq: Serialize,
    OurRep: Serialize,
    OurEvent: Serialize,
    TheirReq: for<'a> Deserialize<'a>,
    TheirRep: for<'a> Deserialize<'a>,
    TheirEvent: for<'a> Deserialize<'a>
> {
    next_id: usize,

    receiver_hdl: ReceiverHdl,
    sender_hdl: SenderHdl<OurReq, OurRep, OurEvent>,

    internal_rx: mpsc::Receiver<InternalMessage<TheirReq, TheirRep, TheirEvent>>,
    rx: mpsc::Receiver<ConnectionMessage<OurReq, OurRep, OurEvent, TheirRep>>,

    reply_map: HashMap<usize, oneshot::Sender<TheirRep>>,
    request_listeners: Vec<mpsc::Sender<TheirReq>>,
    event_listeners: Vec<mpsc::Sender<TheirEvent>>,
}

impl<
    OurReq: Serialize + Send + 'static,
    OurRep: Serialize + Send + 'static,
    OurEvent: Serialize + Send + 'static,
    TheirReq: for<'a> Deserialize<'a> + Clone + Send + 'static,
    TheirRep: for<'a> Deserialize<'a> + Clone + Send + 'static,
    TheirEvent: for<'a> Deserialize<'a> + Clone + Send + 'static
> Connection<OurReq, OurRep, OurEvent, TheirReq, TheirRep, TheirEvent> {
    pub fn new(rx: mpsc::Receiver<ConnectionMessage<OurReq, OurRep, OurEvent, TheirRep>>, stream: WebSocketStream<TcpStream>) -> Self {
        let (internal_tx, internal_rx) = mpsc::channel(1);
        let internal_hdl: InternalHdl<TheirReq, TheirRep, TheirEvent> = InternalHdl::new(internal_tx);
        let (sender, receiver) = stream.split();

        let sender_hdl = SenderHdl::new(internal_hdl.clone(), sender);
        let receiver_hdl = ReceiverHdl::new(internal_hdl.clone(), receiver);

        Connection {
            next_id: 0,

            receiver_hdl,
            sender_hdl,

            internal_rx,
            rx,

            reply_map: HashMap::new(),
            request_listeners: vec![],
            event_listeners: vec![],
        }
    }

    pub async fn run(mut self) {
        loop {
            let control = tokio::select! {
                Some(message) = self.rx.recv() => {
                    self.handle_external(message).await
                },
                Some(message) = self.internal_rx.recv() => {
                    self.handle_internal(message).await
                },
            };

            // If we got a close message from either an internal or external message then close.
            if let ControlFlow::Break(_) = control {
                break;
            }
        }

        debug!("Connection closing.");
    }

    async fn handle_external(&mut self, message: ConnectionMessage<OurReq, OurRep, OurEvent, TheirRep>)
        -> ControlFlow<()>
    {
        match message {
            ConnectionMessage::Event(event) => {
                self.sender_hdl.send(internal::Message::Event(event)).await;
            },
            ConnectionMessage::Request{data, tx} => {
                self.send_request(data, tx).await;
            },
            ConnectionMessage::Reply(reply) => {
                todo!()
            }
            ConnectionMessage::Close => {
                self.close().await;
                return ControlFlow::Break(());
            },
            ConnectionMessage::Send(m) => {
                self.sender_hdl.send(m).await;
            }
        }

        ControlFlow::Continue(())
    }

    async fn handle_internal(&mut self, message: InternalMessage<TheirReq, TheirRep, TheirEvent>)
        -> ControlFlow<()>
    {
        match message {
            InternalMessage::Close => {
                self.close();
                return ControlFlow::Break(());
            }
            InternalMessage::NewMessage(message) => {
                self.handle_internal_new_message(message).await;
            }
        }

        ControlFlow::Continue(())
    }

    async fn handle_internal_new_message(&mut self, message: internal::Message<TheirReq, TheirRep, TheirEvent>) {
        match message {
            internal::Message::Request(request) => { todo!() }
            internal::Message::Reply(reply) => { self.handle_internal_reply(reply).await }
            internal::Message::Event(event) => {}
        }
    }

    async fn handle_internal_reply(&mut self, reply: internal::Reply<TheirRep>) {
        if let Some(tx) = self.reply_map.remove(&reply.id) {
            if let Err(_) = tx.send(reply.data) {
                warn!("Problem sending reply back to requester");
            };
        } else {
            warn!("No request id matches reply id.");
        };
    }

    async fn send_request(&mut self, data: OurReq, tx: oneshot::Sender<TheirRep>) {
        let id = self.next_id;
        self.next_id += 1;
        if self.reply_map.contains_key(&id) {
            warn!("Failed to send request. Request id has already been used.");
            return;
        }
        self.reply_map.insert(id, tx);

        let request = internal::Message::Request(internal::Request::<OurReq>{ id, data });
        self.sender_hdl.send(request).await;
    }

    async fn close(&mut self) {
        self.receiver_hdl.close();
        self.sender_hdl.close();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::BufRead;
    use std::time::Duration;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tokio_tungstenite::{connect_async, MaybeTlsStream, tungstenite, WebSocketStream};
    use crate::scheme::internal;
    use crate::server::connection::connection_hdl::RequestError;
    use crate::server::connection::ConnectionHdl;

    type ConHdlType = ConnectionHdl<String, String, String, String>;

    async fn client(addr: String) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let url = url::Url::parse(format!("ws://{addr}").as_str())
            .expect("Error parsing url.");

        let (ws_stream, _) = connect_async(url).await
            .expect("Error connecting to the server.");

        ws_stream
    }

    async fn send_client(addr: String, tx: mpsc::Sender<String>) {
        let ws_stream = client(addr).await;

        let (_, mut read) = ws_stream.split();
        while let Some(message) = read.next().await {
            match message {
                Ok(m) => {
                    tx.send(m.to_string()).await;
                },
                Err(_) => break,
            }
        }
    }

    async fn responsive_client(addr: String, responses: HashMap<String, String>) {
        let ws_stream = client(addr).await;
        let (mut send, mut read) = ws_stream.split();

        while let Some(message) = read.next().await {
            match message {
                Ok(m) => {
                    assert!(m.is_text());
                    let m_str = m.to_text().unwrap();
                    assert!(responses.contains_key(m_str));
                    send.send(tungstenite::Message::Text(responses.get(m_str).unwrap().clone())).await.unwrap();
                },
                Err(e) => {},
            }
        }

    }

    async fn socket() -> (TcpListener, String) {
        let socket = TcpListener::bind("127.0.0.1:0").await
            .expect("Error binding on socket address");
        let addr = socket.local_addr()
            .expect("Error getting socket listen address");

        (socket, addr.to_string())
    }

    async fn server(socket: TcpListener) -> ConHdlType {
        let (stream, _) = socket.accept().await
            .expect("Error accepting connection.");

        let ws_stream =
            tokio_tungstenite::accept_async(stream).await
                .expect("Error accepting websocket stream.");

        let connection_hdl = ConHdlType::new::<String, String>(ws_stream).await;

        connection_hdl
    }

    #[tokio::test]
    async fn check_timeout() {
        let message = "test";
        let request = internal::Message::<String, String, String>::Request(
            internal::Request { id: 0, data: message.to_string() }
        );

        let (client_tx, mut client_rx) = mpsc::channel(1);
        let (socket, addr) = socket().await;
        tokio::spawn(send_client(addr, client_tx));
        let connection_hdl = server(socket).await;

        let timeout = connection_hdl.request_timeout(
            message.to_string(),
            Duration::from_millis(10),
        ).await;

        // Assert that the request timed out.
        assert_eq!(Err(RequestError::Timeout), timeout);

        // Assert the the Connection did send the message.
        assert_eq!(
            client_rx.recv().await.expect("Problem getting message from client."),
            serde_json::to_string(&request).expect("Problem serializing request.")
        );
    }

    #[tokio::test]
    async fn check_request() {
        let message = "test";
        let request = internal::Message::<String, String, String>::Request(
            internal::Request { id: 0, data: message.to_string() }
        );
        let reply = internal::Message::Reply::<String, String, String>(
            internal::Reply { id: 0, data: message.to_string() }
        );

        let mut requests = HashMap::new();
        requests.insert(
            serde_json::to_string(&request).unwrap(),
            serde_json::to_string(&reply).unwrap()
        );

        let (socket, addr) = socket().await;
        tokio::spawn(responsive_client(addr, requests));
        let connection_hdl = server(socket).await;

        let result = connection_hdl.request_timeout(message.to_string(), Duration::from_millis(10000)).await
            .expect("Timeout getting request result");

        assert_eq!(message, result);

    }

    #[tokio::test]
    async fn check_send_event() {
        let message = "test";
        let event = internal::Message::<String, String, String>::Event(message.to_string());
        let (client_tx, mut client_rx) = mpsc::channel(1);

        let (socket, addr) = socket().await;
        tokio::spawn(send_client(addr, client_tx));
        let connection_hdl = server(socket).await;

        connection_hdl.event(message.to_string()).await;

        let client_message =
            tokio::time::timeout(Duration::from_millis(100), client_rx.recv())
                .await.expect("Timeout getting message.")
                .expect("Empty message");

        assert_eq!(client_message.as_str(), serde_json::to_string(&event).unwrap());
    }
}
