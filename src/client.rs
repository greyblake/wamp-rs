use websocket::{self, Sender, Receiver};
use websocket::client::request::Url;
use websocket::client;
use websocket::stream;
use websocket::message::{Message as WSMessage, Type};
use websocket::header;
use messages::{URI, Dict, List, ID, SubscribeOptions, PublishOptions, Message,  HelloDetails, Reason, ErrorDetails, ClientRoles};
use std::collections::HashMap;
use std::io::{Cursor, Write};
use serde_json;
use serde::{Deserialize, Serialize};

use std::str::from_utf8;
use std::fmt;
use ::{WampResult, Error, ErrorKind};
use std::thread::{self, JoinHandle};
use std::sync::{Mutex, Arc};
use rmp_serde::Deserializer as RMPDeserializer;
use rmp_serde::Serializer;
use rmp::Marker;
use rmp::encode::{ValueWriteError, write_map_len, write_str};
use rmp_serde::encode::VariantWriter;

macro_rules! try_websocket {
    ($e: expr) => (
        match $e {
            Ok(result) => result,
            Err(e) => return Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    );
}

pub struct Connection {
    // sender: client::Sender<stream::WebSocketStream>,
    // receiver: client::Receiver<stream::WebSocketStream>,
    realm: URI,
    url: String
}

struct Subscription {
    callback: Box<Fn(List, Dict)>
}


static WAMP_JSON:&'static str = "wamp.2.json";
static WAMP_MSGPACK:&'static str = "wamp.2.msgpack";

#[derive(PartialEq)]
enum ConnectionState {
    Connected,
    ShuttingDown,
    Disconnected
}

unsafe impl <'a> Send for Subscription {}

unsafe impl<'a> Sync for Subscription {}

pub struct Client {
    connection_info: Arc<ConnectionInfo>,
    max_session_id: ID,
    id: u64
}

struct ConnectionInfo {
    connection_state: Mutex<ConnectionState>,
    sender: Mutex<client::Sender<stream::WebSocketStream>>,
    subscription_requests: Mutex<HashMap<ID, Subscription>>,
    subscriptions: Mutex<HashMap<ID, Subscription>>,
    protocol: String,
}

fn send_message(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message, protocol: &str) -> WampResult<()> {
    info!("Sending message {:?}", message);
    if protocol == WAMP_MSGPACK {
        send_message_msgpack(sender, message)
    } else {
        send_message_json(sender, message)
    }
}

fn send_message_json(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message) -> WampResult<()> {
    let mut sender = sender.lock().unwrap();
    // Send the message
    match sender.send_message(&WSMessage::text(serde_json::to_string(&message).unwrap())) {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("Could not send messsage: {}", e.to_string());
            let _ = sender.send_message(&WSMessage::close());
            Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    }
}



struct StructMapWriter;

impl VariantWriter for StructMapWriter {
    fn write_struct_len<W>(&self, wr: &mut W, len: u32) -> Result<Marker, ValueWriteError>
        where W: Write
    {
        write_map_len(wr, len)
    }

    fn write_field_name<W>(&self, wr: &mut W, _key: &str) -> Result<(), ValueWriteError>
        where W: Write
    {
        write_str(wr, _key)
    }
}

fn send_message_msgpack(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message) -> WampResult<()> {
    let mut sender = sender.lock().unwrap();

    // Send the message
    let mut buf: Vec<u8> = Vec::new();
    message.serialize(&mut Serializer::with(&mut buf, StructMapWriter)).unwrap();
    match sender.send_message(&WSMessage::binary(buf)) {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("Could not send messsage: {}", e.to_string());
            let _ = sender.send_message(&WSMessage::close());
            Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    }
}

fn handle_welcome_message(receiver: &mut client::Receiver<stream::WebSocketStream>, sender: &Mutex<client::Sender<stream::WebSocketStream>>) -> WampResult<Message> {
    let mut sender = sender.lock().unwrap();
    for message in receiver.incoming_messages() {
        let message: WSMessage = try_websocket!(message);
        match message.opcode {
            Type::Close => {
                info!("Received close message, shutting down");
                return Err(Error::new(ErrorKind::ConnectionLost));
            },
            Type::Text => {
                match from_utf8(&message.payload) {
                    Ok(message_text) => {
                        match serde_json::from_str(message_text) {
                            Ok(message) => {
                                return Ok(message);
                            } Err(e) => {
                                return Err(Error::new(ErrorKind::JSONError(e)));
                            }
                        }
                    },
                    Err(_) => {
                        return Err(Error::new(ErrorKind::MalformedData));
                    }
                }
            },
            Type::Binary => {
                let mut de = RMPDeserializer::new(Cursor::new(&*message.payload));
                match Deserialize::deserialize(&mut de) {
                    Ok(message) => {
                        return Ok(message);
                    },
                    Err(e) => {
                        return Err(Error::new(ErrorKind::MsgPackError(e)));
                    }
                }
            },
            Type::Ping => {
                info!("Receieved ping.  Ponging");
                let _ = sender.send_message(&WSMessage::pong(message.payload));
            },
            Type::Pong => {
                info!("Receieved pong");
            }
        };
    }
    Err(Error::new(ErrorKind::ConnectionLost))
}

impl Connection {
    pub fn new(url: &str, realm: &str) -> Connection {
        Connection {
            realm: URI::new(realm),
            url: url.to_string()
        }
    }

    pub fn connect<'a>(&self) -> WampResult<Client> {
        let url = match Url::parse(&self.url) {
            Ok(url) => url,
            Err(e) => return Err(Error::new(ErrorKind::URLError(e)))
        };
        let mut request = try_websocket!(websocket::Client::connect(url)); // Connect to the server
        request.headers.set(header::WebSocketProtocol(vec![WAMP_MSGPACK.to_string(), WAMP_JSON.to_string()]));
        let response = try_websocket!(request.send()); // Send the request

        try_websocket!(response.validate()); // Ensure the response is valid
        let protocol = match response.protocol() {
            Some(protocols) => {
                if protocols.len() == 0 {
                    warn!("Router did not specify protocol. Defaulting to wamp.2.json");
                    WAMP_JSON.to_string()
                } else {
                    protocols[0].clone()
                }
            }
            None => {
                warn!("Router did not specify protocol. Defaulting to wamp.2.json");
                WAMP_JSON.to_string()
            }
        };
        let (sender, mut receiver)  = response.begin().split(); // Get a Client

        let info = Arc::new(ConnectionInfo {
            protocol: protocol,
            subscription_requests: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            sender: Mutex::new(sender),
            connection_state: Mutex::new(ConnectionState::Connected)
        });


        let hello_message = Message::Hello(self.realm.clone(), HelloDetails::new(ClientRoles::new()));
        info!("Sending Hello message");
        if info.protocol == WAMP_MSGPACK {
            try!(send_message_msgpack(&info.sender, hello_message))
        } else {
            try!(send_message_json(&info.sender, hello_message))
        }

        let welcome_message = try!(handle_welcome_message(&mut receiver, &info.sender));
        let session_id = match welcome_message {
            Message::Welcome(session_id, _) => session_id,
            Message::Abort(_, reason) => {
                error!("Recieved abort message.  Reason: {:?}", reason);
                return Err(Error::new(ErrorKind::ConnectionLost));
            },
            _ => return Err(Error::new(ErrorKind::UnexpectedMessage("Expected Welcome Message")))
        };


        self.start_recv_loop(receiver, info.clone());

        Ok(Client {
            connection_info: info,
            id: session_id,
            max_session_id: 0
        })
    }

    fn start_recv_loop(&self, mut receiver: client::Receiver<stream::WebSocketStream>, mut connection_info: Arc<ConnectionInfo>) -> JoinHandle<()> {
        thread::spawn(move || {
            // Receive loop
            for message in receiver.incoming_messages() {
                let message: WSMessage = match message {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Could not receieve message: {:?}", e);
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::close());
                        break;
                    }
                };
                match message.opcode {
                    Type::Close => {
                        info!("Received close message, shutting down");
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::close());
                        break;
                    },
                    Type::Text => {
                        match from_utf8(&message.payload) {
                            Ok(message_text) => {
                                match serde_json::from_str(message_text) {
                                    Ok(message) => {
                                        if !Connection::handle_message(message, &mut connection_info) {
                                            break;
                                        }
                                    } Err(_) => {
                                        error!("Received unknown message: {}", message_text)
                                    }
                                }
                            },
                            Err(_) => {
                                error!("Receieved non-utf-8 json message.  Ignoring");
                            }
                        }
                    },
                    Type::Binary => {
                        let mut de = RMPDeserializer::new(Cursor::new(&*message.payload));
                        match Deserialize::deserialize(&mut de) {
                            Ok(message) => {
                                if !Connection::handle_message(message, &mut connection_info) {
                                    break;
                                }
                            },
                            Err(_) => {
                                error!("Could not understand MsgPack message");
                            }
                        }
                    },
                    Type::Ping => {
                        info!("Receieved ping.  Ponging");
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::pong(message.payload));
                    },
                    Type::Pong => {
                        info!("Receieved pong");
                    }
                }
            }
            connection_info.sender.lock().unwrap().shutdown().ok();
            receiver.shutdown().ok();
            *connection_info.connection_state.lock().unwrap() = ConnectionState::Disconnected;
        })
    }

    fn handle_message(message: Message, connection_info: &mut Arc<ConnectionInfo>) -> bool {
        match message {
            Message::Subscribed(request_id, subscription_id) => {
                // TODO handle errors here
                match connection_info.subscription_requests.lock().unwrap().remove(&request_id) {
                    Some(subscription) => {
                        connection_info.subscriptions.lock().unwrap().insert(subscription_id, subscription);
                    },
                    None => {
                        warn!("Recieved a subscribed notification for a subscription we don't have.  ID: {}", subscription_id);
                    }
                }

            },
            Message::Event(subscription_id, _, _) => {
                match connection_info.subscriptions.lock().unwrap().get(&subscription_id) {
                    Some(subscription) => {
                        let ref callback = subscription.callback;
                        callback(Vec::new(), HashMap::new());
                    },
                    None => {
                        warn!("Recieved an event for a subscription we don't have.  ID: {}", subscription_id);
                    }
                }
            },
            Message::EventArgs(subscription_id, _, _, args) => {
                match connection_info.subscriptions.lock().unwrap().get(&subscription_id) {
                    Some(subscription) => {
                        let ref callback = subscription.callback;
                        callback(args, HashMap::new());
                    },
                    None => {
                        warn!("Recieved an event for a subscription we don't have.  ID: {}", subscription_id);
                    }
                }

            },
            Message::EventKwArgs(subscription_id, _, _, args, kwargs) => {
                match connection_info.subscriptions.lock().unwrap().get(&subscription_id) {
                    Some(subscription) => {
                        let ref callback = subscription.callback;
                        callback(args, kwargs);
                    },
                    None => {
                        warn!("Recieved an event for a subscription we don't have.  ID: {}", subscription_id);
                    }
                }
            },
            Message::Goodbye(_, reason) => {
                match *connection_info.connection_state.lock().unwrap() {
                    ConnectionState::Connected => {
                        info!("Router said goodbye.  Reason: {:?}", reason);
                        send_message(&connection_info.sender, Message::Goodbye(ErrorDetails::new(), Reason::GoodbyeAndOut), &connection_info.protocol).unwrap();
                        return false;
                    },
                    ConnectionState::ShuttingDown => {
                        // The router has seen our goodbye message and has responded in kind
                        return false;
                    },
                    ConnectionState::Disconnected => {
                        // Should never happen
                        return false;
                    }
                }
            }
            _ => {}
        }
        true
    }
}



impl Client {

    fn send_message(&self, message: Message) -> WampResult<()> {
        if self.connection_info.protocol == WAMP_MSGPACK {
            send_message_msgpack(&self.connection_info.sender, message)
        } else {
            send_message_json(&self.connection_info.sender, message)
        }
    }

    fn get_next_session_id(&mut self) -> ID {
        self.max_session_id += 1;
        self.max_session_id
    }

    pub fn subscribe(&mut self, topic: URI, callback: Box<Fn(List, Dict)>) -> WampResult<()> {
        // Send a subscribe messages
        let request_id = self.get_next_session_id();
        self.connection_info.subscription_requests.lock().unwrap().insert(request_id, Subscription{callback: callback});
        self.send_message(Message::Subscribe(request_id, SubscribeOptions::new(), topic))
    }

    pub fn publish(&mut self, topic: URI, args: List, kwargs: Dict) -> WampResult<()> {
        info!("Publishing to {:?} with {:?} | {:?}", topic, args, kwargs);
        let request_id = self.get_next_session_id();
        self.send_message(Message::PublishKwArgs(request_id, PublishOptions::new(false), topic, args, kwargs))
    }

    pub fn shutdown(&mut self) {
        let mut state = self.connection_info.connection_state.lock().unwrap();
        if *state == ConnectionState::Connected {
            self.send_message(Message::Goodbye(ErrorDetails::new(), Reason::SystemShutdown)).ok();
            *state = ConnectionState::ShuttingDown;
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{Connection id: {}}}", self.id)
    }
}