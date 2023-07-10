#![no_main]
#![no_std]
#![feature(type_alias_impl_trait)]
#![feature(alloc_error_handler)]

extern crate alloc;
extern crate tinyrlibc;

pub mod config;
pub mod errors;
pub mod heap;
pub mod keys;
pub mod utils;

use crate::config::{GOLIOTH_SERVER_PORT, GOLIOTH_SERVER_URL, SECURITY_TAG};
use crate::errors::Error;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use at_commands::parser::CommandParser;
use coap_lite::MessageType::NonConfirmable;
use coap_lite::{CoapRequest, ContentFormat, ObserveOption, Packet, RequestType};
use core::cell::RefCell;
use core::option::Option;
use core::str;
use core::sync::atomic::{AtomicU16, Ordering};
use core::task::Poll;
use defmt::{debug, unwrap, Debug2Format};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::waitqueue::WakerRegistration;
use embassy_time::{with_timeout, Duration};
use futures::future::poll_fn;
use nanorand::{Rng, WyRand};
use nrf_modem::{DtlsSocket, OwnedDtlsReceiveSocket, OwnedDtlsSendSocket, PeerVerification};
use panic_probe as _;
use serde::de::DeserializeOwned;
use serde::Serialize;

// Once flashed, comment this out along with the SPM entry in memory.x to eliminate flashing the SPM
// more than once, and will speed up subsequent builds.  Or leave it and flash it every time
#[link_section = ".spm"]
#[used]
static SPM: [u8; 24052] = *include_bytes!("zephyr.bin");

/// use for CoAP mesaage header ID to avoid requests being flagged as duplicate messages
static MESSAGE_ID_COUNTER: AtomicU16 = AtomicU16::new(0);

// A static vector to hold tokens for pending CoAP requests
// Mutex and RefCell are to ensure safe mutability
// Use ThreadModeRawMutex when data is shared between tasks running on the same executor but you want a singleton.
static REQUESTS: Mutex<ThreadModeRawMutex, RefCell<Vec<PendingRequest>>> =
    Mutex::new(RefCell::new(Vec::new()));

/// Enum for light_db write types
#[derive(Debug)]
pub enum LightDBType {
    State,
    Stream,
}

#[derive(Clone)]
enum RequestState {
    Pending(),
    Done { packet: Packet },
}

#[allow(dead_code)]
pub struct PendingRequest {
    state: RequestState,
    token: [u8; 8],
    is_observer: bool,
    is_stale: bool,
    waker: WakerRegistration,
}

impl PendingRequest {
    pub fn new(token: [u8; 8], is_observer: bool) -> Self {
        Self {
            state: RequestState::Pending(),
            token,
            is_observer,
            is_stale: false,
            waker: WakerRegistration::new(),
        }
    }
}

/// Struct to hold our DTLS Socket to Golioth, should live the length of the program
pub struct Golioth {
    tx: OwnedDtlsSendSocket,
    // requests: SharedRequests,
}

impl Golioth {
    pub async fn new(spawner: &Spawner) -> Result<Self, Error> {
        let socket = DtlsSocket::connect(
            GOLIOTH_SERVER_URL,
            GOLIOTH_SERVER_PORT,
            PeerVerification::Enabled,
            &[SECURITY_TAG],
        )
            .await?;

        // // Split the socket so the responses can run in their own task
        // let static_socket= Box::new(socket);
        // let leaked_socket = Box::leak(static_socket);
        // let (rx,tx) = leaked_socket.split();
        let (rx, tx) = socket.split_owned().await?;
        debug!("DTLS Socket created");

        // create a SharedRequests instance for sharing requests between rx and main (tx) tasks
        // let requests = SharedRequests::new();
        unwrap!(spawner.spawn(socket_rx_task(rx)));

        Ok(Self { tx })
    }

    // the DeserializeOwned trait is equivalent to the higher-rank trait bound
    // for<'de> Deserialize<'de>. The only difference is DeserializeOwned is more
    // intuitive to read. It means T owns all the data that gets deserialized.
    /// Get a state value from LightDB.  Provides a unique token to prevent packet spoofing.
    pub async fn lightdb_read<T: DeserializeOwned>(
        &mut self,
        read_type: LightDBType,
        path: &str,
    ) -> Result<T, Error> {
        let mut request: CoapRequest<OwnedDtlsSendSocket> = CoapRequest::new();
        let formatted_path = get_formatted_path(read_type, path);
        let token = create_token();

        request.message.header.message_id = MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        request.set_method(RequestType::Get);
        request.set_path(&formatted_path);
        request
            .message
            .set_content_format(ContentFormat::ApplicationJSON);

        request.message.set_token(token.to_vec());

        debug!("set lighdb write path: {}", &formatted_path.as_str());
        // register a new request
        register_request(token.clone(), false);

        debug!("sending read bytes");
        self.tx.send(&request.message.to_bytes()?).await?;
        let response = request_wait_complete(token).await;
        // let response = with_timeout(
        //     Duration::from_secs(15),
        //     request_wait_complete(token)
        // ).await?;

        Ok(serde_json::from_slice(&response.payload)?)
    }

    /// Post a new value to LightDB state or stream.  Currently, only writes as Non-confirmable.
    pub async fn lightdb_write<T: Serialize>(
        &mut self,
        write_type: LightDBType,
        path: &str,
        v: T,
    ) -> Result<(), Error> {
        let mut request: CoapRequest<OwnedDtlsSendSocket> = CoapRequest::new();
        let token = create_token();
        // message header id distinguishes duplicate messages
        request.message.header.message_id = MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        request.set_method(RequestType::Post);
        // Do not ask for a confirmed response
        request.message.header.set_type(NonConfirmable);
        request.message.set_token(token.to_vec());

        let formatted_path = get_formatted_path(write_type, path);

        request.set_path(&formatted_path);

        request
            .message
            .set_content_format(ContentFormat::ApplicationJSON);
        request.message.payload = serde_json::to_vec(&v)?;

        debug!("set lighdb write path: {}", &formatted_path.as_str());
        debug!("sending write bytes");
        self.tx.send(&request.message.to_bytes()?).await?;

        Ok(())
    }

    /// Register an observer in LightDB, which is an extended GET.  When the data of the observed
    /// path changes, the client will be notified with the updated state.
    pub async fn register_observer(
        &mut self,
        path: &str,
    ) -> Result<CoapRequest<OwnedDtlsSendSocket>, Error> {
        let mut request: CoapRequest<OwnedDtlsSendSocket> = CoapRequest::new();
        let token = create_token();
        let formatted_path = get_formatted_path(LightDBType::State, path);
        debug!(
            "set lighdb path for observing: {}",
            &formatted_path.as_str()
        );

        request.set_method(RequestType::Get);
        request.set_observe_flag(ObserveOption::Register);
        request.set_path(&formatted_path);

        request.message.header.message_id = MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        request.message.set_token(token.to_vec());

        register_request(token, true);

        self.tx.send(&request.message.to_bytes()?).await?;

        Ok(request)
    }

    // pub async fn deregister_observer{};
}

#[inline]
fn get_formatted_path(db_type: LightDBType, path: &str) -> String {
    match db_type {
        LightDBType::State => {
            format!(".d/{}", path)
        }
        LightDBType::Stream => {
            format!(".s/{}", path)
        }
    }
}

/// The nRF9160 does not have a RNG peripheral.  To bypass using the Cryptocell C-lib
/// we can just use the current uptime ticks u64 value as a way to create a unique "enough"
/// token as to not be easily spoofed.
fn create_token() -> [u8; 8] {
    let seed = embassy_time::Instant::now().as_ticks();
    let mut rng = WyRand::new_seed(seed);
    let token: [u8; 8] = rng.generate::<u64>().to_ne_bytes();

    debug!("WyRand Request Token: {}", Debug2Format(&token));
    token
}

/// register a PendingRequest in our Mutex so it can be used to match with in the rx task
pub fn register_request(token: [u8; 8], is_observer: bool) {
    debug!("registering request for response matching");

    let new_request = PendingRequest::new(token, is_observer);
    REQUESTS.lock(|this| {
        this.borrow_mut().push(new_request);
    });
}

pub async fn request_wait_complete(token: [u8; 8]) -> Packet {
    poll_fn(|cx| {
        let mut remove_ndx: Option<usize> = None;
        let mut result = Poll::Pending;

        REQUESTS.lock(|this| {
            for (i, shared) in this.borrow_mut().iter_mut().enumerate() {
                if shared.token == token {
                    if let RequestState::Done { packet } = &shared.state {
                        result = Poll::Ready(packet.clone());
                        if shared.is_observer {
                            shared.state = RequestState::Pending();
                        } else {
                            remove_ndx = Some(i);
                        };
                        break;
                    }
                }
                shared.waker.register(cx.waker());
            }
            // Remove finished requests if they are not observers
            if let Some(i) = remove_ndx {
                this.borrow_mut().remove(i);
            };
            debug!("wait_complete result: {}", Debug2Format(&result));
            // Return received packet
            result
        })
    })
        .await
}

/// Get the current signal strength from the modem using AT+CESQ
pub async fn get_signal_strength() -> Result<i32, Error> {
    let command = nrf_modem::send_at::<32>("AT+CESQ").await?;

    let (_, _, _, _, _, mut signal) = CommandParser::parse(command.as_bytes())
        .expect_identifier(b"+CESQ:")
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_int_parameter()
        .expect_identifier(b"\r\n")
        .finish()
        .unwrap();
    if signal != 255 {
        signal += -140;
    }
    Ok(signal)
}

// if at some point, more than one socket is allowed then set
// pool_size = nrfxlib_sys::NRF_MODEM_MAX_SOCKET_COUNT)
#[embassy_executor::task(pool_size = 1)]
async fn socket_rx_task(rx: OwnedDtlsReceiveSocket) -> ! {
    debug!("RX Task: spawned");
    // buffer for holding response bytes
    let mut buf = [0; 1024];
    loop {
        // wait for rsponses
        debug!("RX Task: waiting for a response");
        let (response, _src_addr) = unwrap!(rx.receive_from(&mut buf[..]).await);
        // parse response bytes into CoAP packets and get the token
        debug!("RX Task: {} Bytes received", response.len());
        if response.len() > 0 {
            let packet = Packet::from_bytes(&response).unwrap();
            let response_token = packet.get_token();

            debug!("RX Task: response bytes {:X}", &response);
            debug!("RX Task: response token {}", &response_token);

            REQUESTS.lock(|this| {
                for request in this.borrow_mut().iter_mut() {
                    if let RequestState::Pending() = request.state {
                        if request.token == response_token {
                            debug!("RX Task: marking pending request as `Done`");
                            request.state = RequestState::Done {
                                packet: packet.clone(),
                            }
                        }
                    }
                    request.waker.wake();
                }
            });
        };
    };
}
