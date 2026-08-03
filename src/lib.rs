pub mod alsa_sink;
pub mod avahi;
pub mod clock;
pub mod crypto;
pub mod decode;
pub mod events;
pub mod jitter;
pub mod mac;
pub mod player;
pub mod rtp;
pub mod rtsp;
pub mod sdp;
pub mod server;
pub mod session;
pub mod sink;

/// Receiver-wide configuration, shared by the RTSP server and the mDNS
/// advertisement. The MAC address must be the same in both places: clients
/// verify the Apple-Response signature against the MAC embedded in the
/// service name.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub port: u16,
    pub mac: [u8; 6],
}

impl Config {
    /// MAC as 12 uppercase hex digits, e.g. "AABBCCDDEEFF", as used in the
    /// `_raop._tcp` service name.
    pub fn mac_hex(&self) -> String {
        self.mac.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// The mDNS service name: `<MAC>@<FriendlyName>`.
    pub fn service_name(&self) -> String {
        format!("{}@{}", self.mac_hex(), self.name)
    }
}
