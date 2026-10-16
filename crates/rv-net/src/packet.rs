//! Minimal Ethernet / ARP / IPv4 / UDP parsing and construction.

use std::net::Ipv4Addr;

pub type Mac = [u8; 6];

pub const ETH_HEADER: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const IP_HEADER: usize = 20;
pub const UDP_HEADER: usize = 8;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

pub struct Ethernet<'a> {
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse_ethernet(frame: &[u8]) -> Option<Ethernet<'_>> {
    (frame.len() >= ETH_HEADER).then(|| Ethernet {
        src: frame[6..12].try_into().unwrap(),
        ethertype: be16(frame, 12),
        payload: &frame[ETH_HEADER..],
    })
}

pub struct Arp {
    pub sender_ip: Ipv4Addr,
    pub target_ip: Ipv4Addr,
}

pub fn parse_arp(p: &[u8]) -> Option<Arp> {
    // Ethernet + IPv4 only.
    if p.len() < 28 || be16(p, 0) != 1 || be16(p, 2) != ETHERTYPE_IPV4 || p[4] != 6 || p[5] != 4 {
        return None;
    }
    Some(Arp {
        sender_ip: Ipv4Addr::new(p[14], p[15], p[16], p[17]),
        target_ip: Ipv4Addr::new(p[24], p[25], p[26], p[27]),
    })
}

pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub protocol: u8,
    pub payload: &'a [u8],
}

pub fn parse_ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    if p.len() < IP_HEADER || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0xf) as usize * 4;
    let total = be16(p, 2) as usize;
    let fragmented = be16(p, 6) & 0x3fff != 0;
    if ihl < IP_HEADER || total < ihl || total > p.len() || fragmented {
        return None;
    }
    Some(Ipv4 {
        src: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        dst: Ipv4Addr::new(p[16], p[17], p[18], p[19]),
        protocol: p[9],
        payload: &p[ihl..total],
    })
}

pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

pub fn parse_udp(p: &[u8]) -> Option<Udp<'_>> {
    if p.len() < UDP_HEADER {
        return None;
    }
    let len = be16(p, 4) as usize;
    if len < UDP_HEADER || len > p.len() {
        return None;
    }
    Some(Udp { src_port: be16(p, 0), dst_port: be16(p, 2), payload: &p[UDP_HEADER..len] })
}

/// TCP ports and flags, enough to spot connection attempts.
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub syn: bool,
    pub ack: bool,
    pub rst: bool,
}

pub fn parse_tcp_header(p: &[u8]) -> Option<TcpHeader> {
    (p.len() >= 20).then(|| TcpHeader {
        src_port: be16(p, 0),
        dst_port: be16(p, 2),
        syn: p[13] & 0x02 != 0,
        ack: p[13] & 0x10 != 0,
        rst: p[13] & 0x04 != 0,
    })
}

fn ethernet_header(dst: Mac, src: Mac, ethertype: u16) -> Vec<u8> {
    let mut f = Vec::with_capacity(1514);
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f
}

/// One's-complement sum used by IP checksums.
fn checksum_add(mut sum: u32, data: &[u8]) -> u32 {
    let (pairs, rest) = data.as_chunks::<2>();
    for c in pairs {
        sum += u16::from_be_bytes(*c) as u32;
    }
    if let [last] = rest {
        sum += (*last as u32) << 8;
    }
    sum
}

fn checksum_finish(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn ipv4_checksum(header: &[u8]) -> u16 {
    checksum_finish(checksum_add(0, header))
}

/// An Ethernet frame carrying a UDP datagram.
pub fn udp_frame(
    src_mac: Mac,
    dst_mac: Mac,
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = UDP_HEADER + payload.len();
    let total = IP_HEADER + udp_len;
    let mut f = ethernet_header(dst_mac, src_mac, ETHERTYPE_IPV4);
    let ip_start = f.len();
    f.extend_from_slice(&[0x45, 0]);
    f.extend_from_slice(&(total as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0x40, 0, 64, PROTO_UDP, 0, 0]);
    f.extend_from_slice(&src.0.octets());
    f.extend_from_slice(&dst.0.octets());
    let csum = ipv4_checksum(&f[ip_start..ip_start + IP_HEADER]);
    f[ip_start + 10..ip_start + 12].copy_from_slice(&csum.to_be_bytes());

    let udp_start = f.len();
    f.extend_from_slice(&src.1.to_be_bytes());
    f.extend_from_slice(&dst.1.to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(payload);
    let mut sum = checksum_add(0, &src.0.octets());
    sum = checksum_add(sum, &dst.0.octets());
    sum += PROTO_UDP as u32 + udp_len as u32;
    sum = checksum_add(sum, &f[udp_start..]);
    let csum = match checksum_finish(sum) {
        0 => 0xffff,
        c => c,
    };
    f[udp_start + 6..udp_start + 8].copy_from_slice(&csum.to_be_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_frame_round_trip() {
        let f = udp_frame(
            [2; 6],
            [4; 6],
            (Ipv4Addr::new(10, 0, 2, 3), 53),
            (Ipv4Addr::new(10, 0, 2, 15), 4000),
            b"hello",
        );
        let eth = parse_ethernet(&f).unwrap();
        assert_eq!((&f[0..6], eth.src, eth.ethertype), (&[4u8; 6][..], [2; 6], ETHERTYPE_IPV4));
        let ip = parse_ipv4(eth.payload).unwrap();
        assert_eq!(ipv4_checksum(&eth.payload[..IP_HEADER]), 0, "header checksum verifies");
        assert_eq!(
            (ip.src, ip.dst, ip.protocol),
            (Ipv4Addr::new(10, 0, 2, 3), Ipv4Addr::new(10, 0, 2, 15), PROTO_UDP)
        );
        let udp = parse_udp(ip.payload).unwrap();
        assert_eq!((udp.src_port, udp.dst_port, udp.payload), (53, 4000, &b"hello"[..]));
        // The UDP checksum over the pseudo-header verifies too.
        let mut sum = checksum_add(0, &ip.src.octets());
        sum = checksum_add(sum, &ip.dst.octets());
        sum += PROTO_UDP as u32 + ip.payload.len() as u32;
        assert_eq!(checksum_finish(checksum_add(sum, ip.payload)), 0);
    }
}
