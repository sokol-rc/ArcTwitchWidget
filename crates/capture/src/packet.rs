use std::net::{IpAddr, Ipv4Addr};

use pcapsql_core::stream::TcpFlags;

#[derive(Debug, Clone)]
pub struct TcpSegment {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: TcpFlags,
    pub payload: Vec<u8>,
    pub frame_number: u64,
    pub timestamp_us: i64,
}

pub fn parse_ipv4_tcp(frame_number: u64, timestamp_us: i64, data: &[u8]) -> Option<TcpSegment> {
    if data.len() < 40 || data[0] >> 4 != 4 || data[9] != 6 {
        return None;
    }
    let ip_header_len = usize::from(data[0] & 0x0f) * 4;
    if ip_header_len < 20 || data.len() < ip_header_len + 20 {
        return None;
    }
    let declared_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let ip_len = if declared_len >= ip_header_len {
        declared_len.min(data.len())
    } else {
        data.len()
    };
    let tcp = ip_header_len;
    let tcp_header_len = usize::from(data[tcp + 12] >> 4) * 4;
    if tcp_header_len < 20 || tcp + tcp_header_len > ip_len {
        return None;
    }
    let src_port = u16::from_be_bytes([data[tcp], data[tcp + 1]]);
    let dst_port = u16::from_be_bytes([data[tcp + 2], data[tcp + 3]]);
    if src_port != 443 && dst_port != 443 {
        return None;
    }
    let seq = u32::from_be_bytes(data[tcp + 4..tcp + 8].try_into().ok()?);
    let ack = u32::from_be_bytes(data[tcp + 8..tcp + 12].try_into().ok()?);
    let bits = data[tcp + 13];
    Some(TcpSegment {
        src_ip: IpAddr::V4(Ipv4Addr::new(data[12], data[13], data[14], data[15])),
        dst_ip: IpAddr::V4(Ipv4Addr::new(data[16], data[17], data[18], data[19])),
        src_port,
        dst_port,
        seq,
        ack,
        flags: TcpFlags {
            syn: bits & 0x02 != 0,
            ack: bits & 0x10 != 0,
            fin: bits & 0x01 != 0,
            rst: bits & 0x04 != 0,
        },
        payload: data[tcp + tcp_header_len..ip_len].to_vec(),
        frame_number,
        timestamp_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_tcp_and_non_https() {
        assert!(parse_ipv4_tcp(1, 0, &[0; 20]).is_none());
        let mut packet = vec![0u8; 40];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&(40u16).to_be_bytes());
        packet[9] = 6;
        packet[20..22].copy_from_slice(&(1234u16).to_be_bytes());
        packet[22..24].copy_from_slice(&(80u16).to_be_bytes());
        packet[32] = 0x50;
        assert!(parse_ipv4_tcp(1, 0, &packet).is_none());
    }
}
