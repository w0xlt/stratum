use async_std::net::TcpStream;
use std::convert::TryInto;

use bitcoin::util::uint::Uint256;

use async_channel::{bounded, Receiver, Sender};

use async_std::{io::BufReader, prelude::*, task};
use roles_logic_sv2::utils::Mutex;
use std::{sync::Arc, time};

use v1::{
    client_to_server,
    error::Error,
    json_rpc, server_to_client,
    utils::{Extranonce, HexU32Be},
    ClientStatus, IsClient,
};

use crate::{job::Job, miner::Miner};
const ADDR: &str = "127.0.0.1:34255";

/// Represents the Mining Device client which is connected to a Upstream node (either a SV1 Pool
/// server or a SV1 <-> SV2 Translator Proxy server).
#[derive(Debug, Clone)]
pub(crate) struct Client {
    client_id: u32,
    extranonce1: Option<Extranonce<'static>>,
    extranonce2_size: Option<usize>,
    version_rolling_mask: Option<HexU32Be>,
    version_rolling_min_bit: Option<HexU32Be>,
    pub(crate) status: ClientStatus,
    sented_authorize_request: Vec<(u64, String)>, // (id, user_name)
    authorized: Vec<String>,
    /// Receives incoming messages from the SV1 Upstream node.
    receiver_incoming: Receiver<String>,
    /// Sends outgoing messages to the SV1 Upstream node.
    sender_outgoing: Sender<String>,
    /// Representation of the Mining Devices
    miner: Arc<Mutex<Miner>>,
}

impl Client {
    /// Outgoing channels are used to send messages to the Upstream
    /// Incoming channels are used to receive messages from the Upstream
    /// There are three separate channels, the first two are responsible for receiving and sending
    /// messages to the Upstream, and the third is responsible for pass valid job submissions to
    /// the first set of channels:
    /// 1. `(sender_incoming, receiver_incoming)`:
    ///     `sender_incoming` listens on the socket where messages are being sent from the Upstream
    ///     node. From the socket, it reads the incoming bytes from the Upstream into a
    ///     `BufReader`. The incoming bytes represent a message from the Upstream, and each new
    ///     line is a new message. When it gets this line (a message) from the Upstream, it sends
    ///     them to the `receiver_incoming` which is listening in a loop. The message line received
    ///     by the `receiver_incoming` are then parsed by the `Client` in the `parse_message`
    ///     method to be handled.
    /// 2. `(sender_outgoing, receiver_outgoing)`:
    ///    When the `parse_message` method on the `Client` is called, it handles the message and
    ///    formats the a new message to be sent to the Upstream in response. It sends the response
    ///    message via the `sender_outgoing` to the `receiver_outgoing` which is waiting to receive
    ///    a message in its own task. When the `receiver_outgoing` receives the response message
    ///    from the the `sender_outgoing`, it writes this message to the socket connected to the
    ///    Upstream via `write_all`.
    /// 3. `(sender_share, receiver_share)`:
    ///    A new thread is spawned to mock the act of a Miner hashing over a candidate block
    ///    without blocking the rest of the program. Since this in its own thread, we need a
    ///    channel to communicate with it, which is `(sender_share, receiver_share)`. In this thread, on
    ///    each new share, `sender_share` sends the pertinent information to create a `mining.submit`
    ///    message to the `receiver_share` that is waiting to receive this information in a separate
    ///    task. In this task, once `receiver_share` gets the information from `sender_share`, it is
    ///    formatted as a `v1::client_to_server::Submit` and then serialized into a json message
    ///    that is sent to the Upstream via `sender_outgoing`.
    pub(crate) async fn connect(client_id: u32) {
        let stream = std::sync::Arc::new(TcpStream::connect(ADDR).await.unwrap());
        let (reader, writer) = (stream.clone(), stream);

        // `sender_incoming` listens on socket for incoming messages from the Upstream and sends
        // messages to the `receiver_incoming` to be parsed and handled by the `Client`
        let (sender_incoming, receiver_incoming) = bounded(10);
        // `sender_outgoing` sends the message parsed by the `Client` to the `receiver_outgoing`
        // which writes the messages to the socket to the Upstream
        let (sender_outgoing, receiver_outgoing) = bounded(10);
        // `sender_share` sends job share results to the `receiver_share` where the job share results are
        // formated into a "mining.submit" messages that is then sent to the Upstream via
        // `sender_outgoing`
        let (sender_share, receiver_share) = bounded(10);

        // Instantiates a new `Miner` (a mock of an actual Mining Device) with a job id of 0.
        let miner = Arc::new(Mutex::new(Miner::new(0)));

        // Sets an initial target for the `Miner`.
        // TODO: This is hard coded for the purposes of a demo, should be set by the SV1
        // `mining.set_difficulty` message received from the Upstream role
        let target_vec: [u8; 32] = [
            0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ];
        let default_target = Uint256::from_be_bytes(target_vec);
        miner.safe_lock(|m| m.new_target(default_target)).unwrap();

        let miner_cloned = miner.clone();

        // Reads messages sent by the Upstream from the socket to be passed to the
        // `receiver_incoming`
        task::spawn(async move {
            let mut messages = BufReader::new(&*reader).lines();
            while let Some(message) = messages.next().await {
                let message = message.unwrap();
                sender_incoming.send(message).await.unwrap();
            }
        });

        // Waits to receive a message from `sender_outgoing` and writes it to the socket for the
        // Upstream to receive
        task::spawn(async move {
            loop {
                let message: String = receiver_outgoing.recv().await.unwrap();
                (&*writer).write_all(message.as_bytes()).await.unwrap();
            }
        });

        // Clone the sender to the Upstream node to use it in another task below as
        // `sender_outgoing` is consumed by the initialization of `Client`
        let sender_outgoing_clone = sender_outgoing.clone();

        // Initialize Client
        let client = Arc::new(Mutex::new(Client {
            client_id,
            extranonce1: None,
            extranonce2_size: None,
            version_rolling_mask: None,
            version_rolling_min_bit: None,
            status: ClientStatus::Init,
            sented_authorize_request: vec![],
            authorized: vec![],
            receiver_incoming,
            sender_outgoing,
            miner,
        }));

        // configure subscribe and authorize
        Self::send_configure(client.clone()).await;

        // Gets the latest candidate block header hash from the `Miner` by calling the `next_share`
        // method. Mocks the act of the `Miner` incrementing the nonce. Performs this in a loop,
        // incrementing the nonce each time, to mimic a Mining Device generating continuous hashes.
        // For each generated block header, sends to the `receiver_share` the relevant values that
        // generated the candidate block header needed to then format and send as a "mining.submit"
        // message to the Upstream node.
        // Is a separate thread as it can be CPU intensive and we do not want to block the reading
        // and writing of messages to the socket.
        std::thread::spawn(move || loop {
            if miner_cloned.safe_lock(|m| m.next_share()).unwrap().is_ok() {
                let nonce = miner_cloned.safe_lock(|m| m.header.unwrap().nonce).unwrap();
                let time = miner_cloned.safe_lock(|m| m.header.unwrap().time).unwrap();
                let job_id = miner_cloned.safe_lock(|m| m.job_id).unwrap();
                let version = miner_cloned.safe_lock(|m| m.version).unwrap();
                // Sends relevant candidate block header values needed to construct a
                // `mining.submit` message to the `receiver_share` in the task that is responsible for
                // sending messages to the Upstream node.
                sender_share
                    .try_send((nonce, job_id.unwrap(), version.unwrap(), time))
                    .unwrap();
            }
            miner_cloned
                .safe_lock(|m| m.header.as_mut().map(|h| h.nonce += 1))
                .unwrap();
        });

        // Task to receive relevant candidate block header values needed to construct a
        // `mining.submit` message. This message is contructed as a `client_to_server::Submit` and
        // then serialized into json to be sent to the Upstream via the `sender_outgoing` sender.
        let cloned = client.clone();
        task::spawn(async move {
            let recv = receiver_share.clone();
            loop {
                let (nonce, job_id, version, ntime) = recv.recv().await.unwrap();
                if cloned.clone().safe_lock(|c| c.status).unwrap() != ClientStatus::Subscribed {
                    continue;
                }
                let extra_nonce2: Extranonce =
                    vec![0; cloned.safe_lock(|c| c.extranonce2_size.unwrap()).unwrap()]
                        .try_into()
                        .unwrap();
                let version = Some(HexU32Be(version));
                let submit = client_to_server::Submit {
                    id: 0,
                    user_name: "user".into(), // TODO: user name should NOT be hardcoded
                    job_id: job_id.to_string(),
                    extra_nonce2,
                    time: HexU32Be(ntime),
                    nonce: HexU32Be(nonce),
                    version_bits: version,
                };
                let message: json_rpc::Message = submit.into();
                let message = format!("{}\n", serde_json::to_string(&message).unwrap());
                sender_outgoing_clone.send(message).await.unwrap();
            }
        });
        let recv_incoming = client.safe_lock(|c| c.receiver_incoming.clone()).unwrap();

        loop {
            match client.clone().safe_lock(|c| c.status).unwrap() {
                ClientStatus::Init => panic!("impossible state"),
                ClientStatus::Configured => {
                    let incoming = recv_incoming.clone().recv().await.unwrap();
                    Self::parse_message(client.clone(), Ok(incoming)).await;
                }
                ClientStatus::Subscribed => {
                    Self::send_authorize(client.clone()).await;
                    break;
                }
            }
        }
        // Waits for the `sender_incoming` to get message line from socket to be parsed by the
        // `Client`
        loop {
            let incoming = recv_incoming.recv().await.unwrap();
            Self::parse_message(client.clone(), Ok(incoming)).await;
        }
    }

    /// Parse SV1 messages received from the Upstream node.
    async fn parse_message(
        self_: Arc<Mutex<Self>>,
        incoming_message: Result<String, async_channel::TryRecvError>,
    ) {
        // If we have a line (1 line represents 1 sv1 incoming message), then handle that message
        if let Ok(line) = incoming_message {
            println!(
                "CLIENT {} - Received: {}",
                self_.safe_lock(|s| s.client_id).unwrap(),
                line
            );
            let message: json_rpc::Message = serde_json::from_str(&line).unwrap();
            // If has a message, it sends it back
            if let Some(m) = self_
                .safe_lock(|s| s.handle_message(message).unwrap())
                .unwrap()
            {
                let sender = self_.safe_lock(|s| s.sender_outgoing.clone()).unwrap();
                Self::send_message(sender, m).await;
            }
        };
    }

    /// Send SV1 messages to the receiver_outgoing which writes to the socket (aka Upstream node)
    async fn send_message(sender: Sender<String>, msg: json_rpc::Message) {
        let msg = format!("{}\n", serde_json::to_string(&msg).unwrap());
        println!(" - Send: {}", &msg);
        sender.send(msg).await.unwrap();
    }

    pub(crate) async fn send_configure(self_: Arc<Mutex<Self>>) {
        // This loop is probably unnecessary as the first state is `Init`
        loop {
            if let ClientStatus::Init = self_.safe_lock(|s| s.status).unwrap() {
                break;
            }
        }
        let id = time::SystemTime::now()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let configure = self_.safe_lock(|s| s.configure(id)).unwrap();
        let sender = self_.safe_lock(|s| s.sender_outgoing.clone()).unwrap();
        Self::send_message(sender, configure).await;
        // Update status as configured
        self_
            .safe_lock(|s| s.status = ClientStatus::Configured)
            .unwrap();
    }

    pub async fn send_authorize(self_: Arc<Mutex<Self>>) {
        let id = time::SystemTime::now()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let authorize = self_
            .safe_lock(|s| {
                s.authorize(id, "user".to_string(), "password".to_string())
                    .unwrap()
            })
            .unwrap();
        self_
            .safe_lock(|s| s.sented_authorize_request.push((id, "user".to_string())))
            .unwrap();
        let sender = self_.safe_lock(|s| s.sender_outgoing.clone()).unwrap();

        Self::send_message(sender, authorize).await;
    }
}

impl IsClient<'static> for Client {
    /// Updates miner with new job
    fn handle_notify(
        &mut self,
        notify: server_to_client::Notify<'static>,
    ) -> Result<(), Error<'static>> {
        let new_job: Job = notify.into();
        self.miner.safe_lock(|m| m.new_header(new_job)).unwrap();
        Ok(())
    }

    fn handle_configure(
        &self,
        _conf: &mut server_to_client::Configure,
    ) -> Result<(), Error<'static>> {
        Ok(())
    }

    fn handle_subscribe(
        &mut self,
        _subscribe: &server_to_client::Subscribe,
    ) -> Result<(), Error<'static>> {
        Ok(())
    }

    fn set_extranonce1(&mut self, extranonce1: Extranonce<'static>) {
        self.extranonce1 = Some(extranonce1);
    }

    fn extranonce1(&self) -> Extranonce<'static> {
        self.extranonce1.clone().unwrap()
    }

    fn set_extranonce2_size(&mut self, extra_nonce2_size: usize) {
        self.extranonce2_size = Some(extra_nonce2_size);
    }

    fn extranonce2_size(&self) -> usize {
        self.extranonce2_size.unwrap()
    }

    fn version_rolling_mask(&self) -> Option<HexU32Be> {
        self.version_rolling_mask.clone()
    }

    fn set_version_rolling_mask(&mut self, mask: Option<HexU32Be>) {
        self.version_rolling_mask = mask;
    }

    fn set_version_rolling_min_bit(&mut self, min: Option<HexU32Be>) {
        self.version_rolling_min_bit = min;
    }

    fn set_status(&mut self, status: ClientStatus) {
        self.status = status;
    }

    fn signature(&self) -> String {
        format!("{}", self.client_id)
    }

    fn status(&self) -> ClientStatus {
        self.status
    }

    fn version_rolling_min_bit(&mut self) -> Option<HexU32Be> {
        self.version_rolling_min_bit.clone()
    }

    fn id_is_authorize(&mut self, id: &u64) -> Option<String> {
        let req: Vec<&(u64, String)> = self
            .sented_authorize_request
            .iter()
            .filter(|x| x.0 == *id)
            .collect();
        match req.len() {
            0 => None,
            _ => Some(req[0].1.clone()),
        }
    }

    fn id_is_submit(&mut self, _: &u64) -> bool {
        false
    }

    fn authorize_user_name(&mut self, name: String) {
        self.authorized.push(name)
    }

    fn is_authorized(&self, name: &String) -> bool {
        self.authorized.contains(name)
    }

    fn authorize(
        &mut self,
        id: u64,
        name: String,
        password: String,
    ) -> Result<json_rpc::Message, Error> {
        match self.status() {
            ClientStatus::Init => Err(Error::IncorrectClientStatus("mining.authorize".to_string())),
            _ => {
                self.sented_authorize_request.push((id, "user".to_string()));
                Ok(client_to_server::Authorize { id, name, password }.into())
            }
        }
    }

    fn last_notify(&self) -> Option<server_to_client::Notify> {
        None
    }

    fn handle_error_message(
        &mut self,
        message: v1::Message,
    ) -> Result<Option<json_rpc::Message>, Error<'static>> {
        println!("{:?}", message);
        Ok(None)
    }
}

// /// Represents a new outgoing `mining.submit` solution submission to be sent to the Upstream
// /// server.
// struct Submit {
//     // worker_name: String,
//     /// ID of the job used while submitting share generated from this job.
//     /// TODO: Currently is `u32` and is hardcoded, but should be String and set by the incoming
//     /// `mining.notify` message.
//     job_id: u32,
//     // /// TODO: Hard coded for demo
//     // extranonce_2: u32,
//     /// Current time
//     ntime: u32,
//     /// Nonce
//     /// TODO: Hard coded for the demo
//     nonce: u32,
// }
