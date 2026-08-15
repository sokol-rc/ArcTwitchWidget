//! Chooses where packets come from.
//!
//! ARC Live prefers the driver-free raw socket, but only after it proves it
//! delivers inbound packets; otherwise the WinDivert driver takes over. On a
//! host with Hyper-V and WSL adapters the raw socket was measured delivering
//! outbound traffic only, which silently makes decryption impossible, hence
//! the check. `ARC_LIVE_CAPTURE_BACKEND` overrides the choice: `rawsocket`,
//! `windivert` or `auto` (the default).

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::raw::{RawCapture as DivertCapture, RawCaptureControl as DivertControl};
use crate::rawsock::{RawCapture as SocketCapture, RawCaptureControl as SocketControl};

/// How long a fresh raw socket gets to show an inbound packet before it is
/// declared useless. Any busy network answers in well under a second.
const VALIDATION_WINDOW: Duration = Duration::from_secs(6);

/// True as soon as one packet arrives *towards* this host, which is what the
/// decryption needs and what a broken raw socket never produces.
fn delivers_inbound(capture: &mut SocketCapture, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        match capture.next_packet() {
            Ok(Some(packet)) => {
                if let Some(segment) = crate::packet::parse_ipv4_tcp(0, 0, packet)
                    && segment.src_port == 443
                {
                    return true;
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    RawSocket,
    WinDivert,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawSocket => "raw socket",
            Self::WinDivert => "WinDivert",
        }
    }
}

/// Parses the backend preference. Anything unrecognised means "decide for me".
pub fn preference(raw: Option<&str>) -> Option<Backend> {
    match raw
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("rawsocket" | "raw" | "socket") => Some(Backend::RawSocket),
        Some("windivert" | "divert" | "driver") => Some(Backend::WinDivert),
        _ => None,
    }
}

pub enum PacketSource {
    RawSocket(SocketCapture),
    WinDivert(DivertCapture),
}

#[derive(Clone)]
pub enum SourceControl {
    RawSocket(SocketControl),
    WinDivert(DivertControl),
}

impl SourceControl {
    pub fn shutdown(&self) {
        match self {
            Self::RawSocket(control) => control.shutdown(),
            Self::WinDivert(control) => control.shutdown(),
        }
    }
}

impl PacketSource {
    /// Opens the preferred source. Without a preference the raw socket is tried
    /// first and WinDivert is used only if it fails, so the driver becomes
    /// optional instead of required.
    pub fn open() -> Result<(Self, SourceControl, Backend, Option<String>)> {
        let requested = preference(std::env::var("ARC_LIVE_CAPTURE_BACKEND").ok().as_deref());
        match requested {
            Some(Backend::WinDivert) => {
                let (capture, control) = DivertCapture::open()?;
                Ok((
                    Self::WinDivert(capture),
                    SourceControl::WinDivert(control),
                    Backend::WinDivert,
                    None,
                ))
            }
            Some(Backend::RawSocket) => {
                let (capture, control) = SocketCapture::open()?;
                Ok((
                    Self::RawSocket(capture),
                    SourceControl::RawSocket(control),
                    Backend::RawSocket,
                    None,
                ))
            }
            None => {
                // The raw socket opens fine on machines where it then delivers
                // outbound traffic only - measured on a host with Hyper-V and
                // WSL virtual adapters. Without inbound packets there is no
                // ServerHello and nothing can be decrypted, so the source has
                // to prove itself before it is trusted.
                let reason = match SocketCapture::open() {
                    Ok((mut capture, control)) => {
                        if delivers_inbound(&mut capture, VALIDATION_WINDOW) {
                            return Ok((
                                Self::RawSocket(capture),
                                SourceControl::RawSocket(control),
                                Backend::RawSocket,
                                None,
                            ));
                        }
                        control.shutdown();
                        "the raw socket delivered no inbound packets, so TLS could never be \
                         decrypted"
                            .to_owned()
                    }
                    Err(error) => format!("{error:#}"),
                };
                let (capture, control) = DivertCapture::open()?;
                Ok((
                    Self::WinDivert(capture),
                    SourceControl::WinDivert(control),
                    Backend::WinDivert,
                    Some(reason),
                ))
            }
        }
    }

    pub fn next_packet(&mut self) -> Result<Option<&[u8]>> {
        match self {
            Self::RawSocket(capture) => capture.next_packet(),
            Self::WinDivert(capture) => capture.next_packet(),
        }
    }

    /// Packets Winsock had to truncate; always zero for WinDivert.
    pub fn oversized_packets(&self) -> u64 {
        match self {
            #[cfg(windows)]
            Self::RawSocket(capture) => capture.oversized_packets,
            #[cfg(not(windows))]
            Self::RawSocket(_) => 0,
            Self::WinDivert(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_preference_is_read_from_the_environment() {
        assert_eq!(preference(Some("rawsocket")), Some(Backend::RawSocket));
        assert_eq!(preference(Some(" WinDivert ")), Some(Backend::WinDivert));
        assert_eq!(preference(Some("auto")), None);
        assert_eq!(preference(None), None);
        assert_eq!(preference(Some("nonsense")), None);
    }
}
