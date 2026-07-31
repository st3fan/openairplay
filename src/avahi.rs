//! `_raop._tcp` advertisement via the system Avahi daemon.
//!
//! Milestone 1 keeps this deliberately simple: spawn `avahi-publish-service`
//! and keep it alive for the lifetime of the receiver. A later milestone can
//! switch to the Avahi D-Bus API for proper lifecycle handling.

use std::io;
use std::process::{Child, Command, Stdio};

use log::info;

use crate::Config;

/// TXT records matching what shairport-sync advertises in classic
/// (AirPlay 1) mode: ALAC or PCM (`cn`), optional RSA encryption (`et`),
/// 44100/16/2 audio, no password.
pub fn txt_records() -> Vec<String> {
    [
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "ek=1",
        "et=0,1",
        "sv=false",
        "da=true",
        "sr=44100",
        "ss=16",
        "pw=false",
        "vn=65537",
        "tp=UDP",
        "md=0,1,2",
        "am=OpenAirPlay",
        "vs=105.1",
        "sf=0x4",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// A running `avahi-publish-service` process; the registration disappears
/// when this is dropped.
pub struct Advertisement {
    child: Child,
}

pub fn publish(config: &Config) -> io::Result<Advertisement> {
    let service_name = config.service_name();
    let child = Command::new("avahi-publish-service")
        .arg(&service_name)
        .arg("_raop._tcp")
        .arg(config.port.to_string())
        .args(txt_records())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    info!(
        "advertising \"{service_name}\" on _raop._tcp port {}",
        config.port
    );
    Ok(Advertisement { child })
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
