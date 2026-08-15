pub(crate) mod etw;
pub(crate) mod packet;
mod raw;
mod rawsock;
mod scanner;
mod source;

pub use scanner::{CaptureEvent, CaptureHandle, CaptureStats, start_capture};

/// Opens the packet source and reports what it actually delivers, so the
/// driver-free backend can be verified on real hardware before it ships.
/// Both directions must be visible: the ClientHello that carries
/// `client_random` is outbound, and without it no key can be matched.
pub fn probe_packet_source(seconds: u64) -> anyhow::Result<String> {
    use std::time::{Duration, Instant};

    let addresses = rawsock::capture_addresses()
        .map(|list| {
            list.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|error| format!("не определены: {error}"));
    let (mut source, control, backend, fallback) = source::PacketSource::open()?;
    let local: Vec<std::net::IpAddr> = rawsock::capture_addresses()
        .unwrap_or_default()
        .into_iter()
        .map(std::net::IpAddr::V4)
        .collect();
    let mut from_me = 0u64;
    let mut to_me = 0u64;
    let mut inbound = 0u64;
    let mut outbound = 0u64;
    let mut client_hellos = 0u64;
    let mut server_hellos = 0u64;
    let mut packets = 0u64;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        match source.next_packet() {
            Ok(Some(packet)) => {
                packets += 1;
                if let Some(segment) = packet::parse_ipv4_tcp(packets, 0, packet) {
                    if local.contains(&segment.src_ip) {
                        from_me += 1;
                    }
                    if local.contains(&segment.dst_ip) {
                        to_me += 1;
                    }
                    if segment.dst_port == 443 {
                        outbound += 1;
                        if scanner::looks_like_client_hello(&segment.payload) {
                            client_hellos += 1;
                        }
                    }
                    if segment.src_port == 443 {
                        inbound += 1;
                        if scanner::looks_like_server_hello(&segment.payload) {
                            server_hellos += 1;
                        }
                    }
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                control.shutdown();
                return Err(error);
            }
        }
    }
    control.shutdown();

    let driver_free = matches!(backend, source::Backend::RawSocket | source::Backend::Etw);
    let verdict = match (outbound, inbound) {
        (0, 0) => "трафик на 443 не пойман вообще.",
        (_, 0) => "видны только ИСХОДЯЩИЕ — нет ServerHello, ключи не установить.",
        (0, _) => "видны только ВХОДЯЩИЕ — нет ClientHello, ключи не сопоставить.",
        _ if driver_free => "ОБЕ стороны видны — сторонний драйвер не нужен.",
        _ => "ОБЕ стороны видны, но через WinDivert: встроенные источники здесь не подошли.",
    };
    Ok(format!(
        "движок: {} (режим RCVALL: {})\nинтерфейсы: {addresses}\nпакетов всего: {packets}\nTCP 443 исходящих: {outbound} (из них ClientHello: {client_hellos})\nTCP 443 входящих: {inbound} (из них ServerHello: {server_hellos})\nпо адресам: от нас {from_me}, к нам {to_me}\nкрупных пакетов потеряно: {}\nзапасной движок: {}\nВЫВОД: {verdict}",
        backend.as_str(),
        std::env::var("ARC_LIVE_RCVALL_MODE").unwrap_or_else(|_| "3 (по умолчанию)".to_owned()),
        source.oversized_packets(),
        fallback.unwrap_or_else(|| "не понадобился".to_owned()),
    ))
}
