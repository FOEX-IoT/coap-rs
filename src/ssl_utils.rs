use openssl::ssl::{SslConnector, SslMethod};
use std::io::Result;
use std::io::Write;

const KEY: &'static str = "<placeholder>";
const ID: &'static str = "Client_identity";

pub fn get_ssl_connector() -> Result<SslConnector> {
  let mut builder = SslConnector::builder(SslMethod::dtls())?;

  builder.set_psk_client_callback(move |_ssl, _hint, mut identity_buffer, mut psk_buffer| {
    identity_buffer.write_all(ID.as_bytes()).unwrap();
    psk_buffer.write_all(KEY.as_bytes()).unwrap();
    Ok(KEY.len())
  });
  builder.set_cipher_list("ECDHE-PSK-AES128-CBC-SHA256:PSK-AES128-CCM8:ECDHE-ECDSA-AES128-CCM8")?;

  let connector = builder.build();
  return Ok(connector);
}
