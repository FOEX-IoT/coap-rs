use super::message::packet::{ObserveOption, Packet};
use super::message::request::CoAPRequest;
use super::message::response::{CoAPResponse, Status};
use super::message::IsMessage;
use crate::ssl_utils::get_ssl_connector;
use crate::udp::UDPWrapper;
use log::*;
use openssl::ssl::SslStream;
use regex::Regex;
use std::io::{Error, ErrorKind, Result};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use url::Url;

const DEFAULT_RECEIVE_TIMEOUT: u64 = 1; // 1s

enum ObserveMessage {
  Terminate,
}

pub struct DTLSCoAPClient {
  socket: SslStream<UDPWrapper>,
  peer_addr: SocketAddr,
  observe_sender: Option<mpsc::Sender<ObserveMessage>>,
  observe_thread: Option<thread::JoinHandle<()>>,
}

impl DTLSCoAPClient {
  /// Create a CoAP client with the specific source and peer address.
  pub fn new_with_specific_source<A: ToSocketAddrs, B: ToSocketAddrs>(
    bind_addr: A,
    peer_addr: B,
  ) -> Result<DTLSCoAPClient> {
    let addr = peer_addr
      .to_socket_addrs()?
      .next()
      .ok_or(Error::new(ErrorKind::Other, "no address"))?;

    let bind_addr = bind_addr
      .to_socket_addrs()?
      .next()
      .ok_or(Error::new(ErrorKind::Other, "no address"))?;

    let socket: UDPWrapper = UDPWrapper::connect(&addr, &bind_addr)?;

    socket.set_read_timeout(Some(Duration::new(DEFAULT_RECEIVE_TIMEOUT, 0)))?;

    let connector = get_ssl_connector()?;

    let stream = connector.connect("localhost", socket).unwrap();

    Ok(DTLSCoAPClient {
      socket: stream,
      peer_addr: addr,
      observe_sender: None,
      observe_thread: None,
    })
  }

  /// Create a CoAP client with the peer address.
  pub fn new<A: ToSocketAddrs>(addr: A) -> Result<DTLSCoAPClient> {
    addr
      .to_socket_addrs()
      .and_then(|mut iter| match iter.next() {
        Some(SocketAddr::V4(_)) => Self::new_with_specific_source("0.0.0.0:0", addr),
        Some(SocketAddr::V6(_)) => Self::new_with_specific_source(":::0", addr),
        None => Err(Error::new(ErrorKind::Other, "no address")),
      })
  }

  /// Execute a get request
  pub fn get(url: &str) -> Result<CoAPResponse> {
    Self::get_with_timeout(url, Duration::new(DEFAULT_RECEIVE_TIMEOUT, 0))
  }

  /// Execute a get request with the coap url and a specific timeout.
  pub fn get_with_timeout(url: &str, timeout: Duration) -> Result<CoAPResponse> {
    let (domain, port, path) = Self::parse_coap_url(url)?;

    let mut packet = CoAPRequest::new();
    packet.set_path(path.as_str());

    let mut client = Self::new((domain.as_str(), port))?;
    client.send(&packet)?;

    client.set_receive_timeout(Some(timeout))?;
    match client.receive() {
      Ok(receive_packet) => Ok(receive_packet),
      Err(e) => Err(e),
    }
  }

  /// Observe a resource with the handler
  pub fn observe<H: FnMut(Packet) + Send + 'static>(
    &mut self,
    resource_path: &str,
    mut handler: H,
  ) -> Result<()> {
    // TODO: support observe multi resources at the same time
    let mut message_id: u16 = 0;
    let mut register_packet = CoAPRequest::new();
    register_packet.set_observe(vec![ObserveOption::Register as u8]);
    register_packet.set_message_id(Self::gen_message_id(&mut message_id));
    register_packet.set_path(resource_path);

    self.send(&register_packet)?;

    self.set_receive_timeout(Some(Duration::new(DEFAULT_RECEIVE_TIMEOUT, 0)))?;
    let response = self.receive()?;
    if *response.get_status() != Status::Content {
      return Err(Error::new(ErrorKind::NotFound, "the resource not found"));
    }

    handler(response.message);

    let socket;
    match self.socket.get_ref().try_clone() {
      Ok(good_socket) => socket = good_socket,
      Err(_) => return Err(Error::new(ErrorKind::Other, "network error")),
    }

    let connector = get_ssl_connector()?;

    let mut stream = connector.connect("localhost", socket).unwrap();
    let peer_addr = self.peer_addr.clone();
    let (observe_sender, observe_receiver) = mpsc::channel();
    let observe_path = String::from(resource_path);

    let observe_thread = thread::spawn(move || loop {
      match Self::receive_from_socket(&mut stream) {
        Ok(packet) => {
          let receive_packet = CoAPRequest::from_packet(packet, &peer_addr);

          handler(receive_packet.message);

          if let Some(response) = receive_packet.response {
            let mut packet = Packet::new();
            packet.header.set_type(response.message.header.get_type());
            packet
              .header
              .set_message_id(response.message.header.get_message_id());
            packet.set_token(response.message.get_token().clone());

            match Self::send_with_socket(&mut stream, &peer_addr, &packet) {
              Ok(_) => (),
              Err(e) => warn!("reply ack failed {}", e),
            }
          }
        }
        Err(e) => {
          match e.kind() {
            ErrorKind::WouldBlock => (), // timeout
            _ => warn!("observe failed {:?}", e),
          }
        }
      };

      match observe_receiver.try_recv() {
        Ok(ObserveMessage::Terminate) => {
          let mut deregister_packet = CoAPRequest::new();
          deregister_packet.set_message_id(Self::gen_message_id(&mut message_id));
          deregister_packet.set_observe(vec![ObserveOption::Deregister as u8]);
          deregister_packet.set_path(observe_path.as_str());

          Self::send_with_socket(&mut stream, &peer_addr, &deregister_packet.message).unwrap();
          Self::receive_from_socket(&mut stream).unwrap();
          break;
        }
        _ => continue,
      }
    });
    self.observe_sender = Some(observe_sender);
    self.observe_thread = Some(observe_thread);

    return Ok(());
  }

  /// Stop observing
  pub fn unobserve(&mut self) {
    match self.observe_sender.take() {
      Some(ref sender) => {
        sender.send(ObserveMessage::Terminate).unwrap();

        self.observe_thread.take().map(|g| g.join().unwrap());
      }
      _ => {}
    }
  }

  /// Execute a request.
  pub fn send(&mut self, request: &CoAPRequest) -> Result<()> {
    Self::send_with_socket(&mut self.socket, &self.peer_addr, &request.message)
  }

  /// Receive a response.
  pub fn receive(&mut self) -> Result<CoAPResponse> {
    let packet = Self::receive_from_socket(&mut self.socket)?;
    Ok(CoAPResponse { message: packet })
  }

  /// Set the receive timeout.
  pub fn set_receive_timeout(&self, dur: Option<Duration>) -> Result<()> {
    self.socket.get_ref().set_read_timeout(dur)
  }

  fn send_with_socket(
    socket: &mut SslStream<UDPWrapper>,
    peer_addr: &SocketAddr,
    message: &Packet,
  ) -> Result<()> {
    match message.to_bytes() {
      Ok(bytes) => {
        let size = socket.ssl_write(&bytes[..]).unwrap();
        if size == bytes.len() {
          Ok(())
        } else {
          Err(Error::new(ErrorKind::Other, "send length error"))
        }
      }
      Err(_) => Err(Error::new(ErrorKind::InvalidInput, "packet error")),
    }
  }

  fn receive_from_socket(socket: &mut SslStream<UDPWrapper>) -> Result<Packet> {
    let mut buf = [0; 1500];

    let nread = socket.ssl_read(&mut buf);
    if nread.is_err() {
      return Err(Error::new(ErrorKind::InvalidInput, "packet error"));
    }
    let nread = nread.unwrap();
    match Packet::from_bytes(&buf[..nread]) {
      Ok(packet) => Ok(packet),
      Err(_) => Err(Error::new(ErrorKind::InvalidInput, "packet error")),
    }
  }

  fn parse_coap_url(url: &str) -> Result<(String, u16, String)> {
    let url_params = match Url::parse(url) {
      Ok(url_params) => url_params,
      Err(_) => return Err(Error::new(ErrorKind::InvalidInput, "url error")),
    };

    let host = match url_params.host_str() {
      Some("") => return Err(Error::new(ErrorKind::InvalidInput, "host error")),
      Some(h) => h,
      None => return Err(Error::new(ErrorKind::InvalidInput, "host error")),
    };
    let host = Regex::new(r"^\[(.*?)]$")
      .unwrap()
      .replace(&host, "$1")
      .to_string();

    let port = match url_params.port() {
      Some(p) => p,
      None => 5683,
    };

    let path = url_params.path().to_string();

    return Ok((host.to_string(), port, path));
  }

  fn gen_message_id(message_id: &mut u16) -> u16 {
    (*message_id) += 1;
    return *message_id;
  }
}

impl Drop for DTLSCoAPClient {
  fn drop(&mut self) {
    self.unobserve();
  }
}

#[cfg(test)]
mod test {
  use super::super::*;
  use super::*;
  use std::io::ErrorKind;
  use std::time::Duration;

  #[test]
  fn test_parse_coap_url_good_url() {
    assert!(DTLSCoAPClient::parse_coap_url("coap://127.0.0.1").is_ok());
    assert!(DTLSCoAPClient::parse_coap_url("coap://127.0.0.1:5683").is_ok());
    assert!(DTLSCoAPClient::parse_coap_url("coap://[::1]").is_ok());
    assert!(DTLSCoAPClient::parse_coap_url("coap://[::1]:5683").is_ok());
    assert!(DTLSCoAPClient::parse_coap_url("coap://[bbbb::9329:f033:f558:7418]").is_ok());
    assert!(DTLSCoAPClient::parse_coap_url("coap://[bbbb::9329:f033:f558:7418]:5683").is_ok());
  }

  #[test]
  fn test_parse_coap_url_bad_url() {
    assert!(DTLSCoAPClient::parse_coap_url("coap://127.0.0.1:65536").is_err());
    assert!(DTLSCoAPClient::parse_coap_url("coap://").is_err());
    assert!(DTLSCoAPClient::parse_coap_url("coap://:5683").is_err());
    assert!(DTLSCoAPClient::parse_coap_url("127.0.0.1").is_err());
  }

  async fn request_handler(_: CoAPRequest) -> Option<CoAPResponse> {
    None
  }

  #[test]
  fn test_get() {
    let resp = DTLSCoAPClient::get("coap://coap.me:5683/hello").unwrap();
    assert_eq!(resp.message.payload, b"world".to_vec());
  }

  #[test]
  fn test_get_timeout() {
    let server_port = server::test::spawn_server(request_handler).recv().unwrap();

    let error = DTLSCoAPClient::get_with_timeout(
      &format!("coap://127.0.0.1:{}/Rust", server_port),
      Duration::new(1, 0),
    )
    .unwrap_err();
    if cfg!(windows) {
      assert_eq!(error.kind(), ErrorKind::TimedOut);
    } else {
      assert_eq!(error.kind(), ErrorKind::WouldBlock);
    }
  }
}
