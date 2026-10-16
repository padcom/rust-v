//! A DHCP server that leases the single guest address.

use std::net::Ipv4Addr;

use crate::Config;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const MAGIC: [u8; 4] = [99, 130, 83, 99];
const OPTIONS: usize = 240;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const INFORM: u8 = 8;

const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_BROADCAST: u8 = 28;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_END: u8 = 255;
const OPT_PAD: u8 = 0;

const LEASE_SECS: u32 = 24 * 3600;

fn message_type(options: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            OPT_PAD => i += 1,
            OPT_END => return None,
            code => {
                let len = *options.get(i + 1)? as usize;
                let value = options.get(i + 2..i + 2 + len)?;
                if code == OPT_MESSAGE_TYPE && len == 1 {
                    return Some(value[0]);
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// Answers a client message (the UDP payload sent to port 67), if it needs an answer.
pub fn handle(config: &Config, request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < OPTIONS || request[0] != BOOTREQUEST || request[236..240] != MAGIC {
        return None;
    }
    let reply_type = match message_type(&request[OPTIONS..])? {
        DISCOVER => OFFER,
        REQUEST | INFORM => ACK,
        _ => return None,
    };
    let mut r = vec![0u8; OPTIONS];
    r[0] = BOOTREPLY;
    r[1] = 1; // Ethernet
    r[2] = 6;
    r[4..8].copy_from_slice(&request[4..8]); // transaction ID
    r[10..12].copy_from_slice(&request[10..12]); // flags
    r[12..16].copy_from_slice(&request[12..16]); // ciaddr
    r[16..20].copy_from_slice(&config.guest_ip.octets());
    r[20..24].copy_from_slice(&config.gateway_ip.octets());
    r[28..44].copy_from_slice(&request[28..44]); // client hardware address
    r[236..240].copy_from_slice(&MAGIC);
    let mut opt = |code: u8, value: &[u8]| {
        r.push(code);
        r.push(value.len() as u8);
        r.extend_from_slice(value);
    };
    let broadcast = Ipv4Addr::from(u32::from(config.gateway_ip) | !u32::from(config.netmask));
    opt(OPT_MESSAGE_TYPE, &[reply_type]);
    opt(OPT_SERVER_ID, &config.gateway_ip.octets());
    opt(OPT_LEASE_TIME, &LEASE_SECS.to_be_bytes());
    opt(OPT_SUBNET_MASK, &config.netmask.octets());
    opt(OPT_ROUTER, &config.gateway_ip.octets());
    opt(OPT_DNS, &config.dns_ip.octets());
    opt(OPT_BROADCAST, &broadcast.octets());
    r.push(OPT_END);
    // Pad to the minimum BOOTP message size some clients insist on.
    r.resize(r.len().max(300), 0);
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: u8) -> Vec<u8> {
        let mut m = vec![0u8; OPTIONS];
        m[0] = BOOTREQUEST;
        m[4..8].copy_from_slice(&[1, 2, 3, 4]);
        m[28..34].copy_from_slice(&[0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        m[236..240].copy_from_slice(&MAGIC);
        m.extend_from_slice(&[OPT_PAD, OPT_MESSAGE_TYPE, 1, kind, OPT_END]);
        m
    }

    #[test]
    fn discover_then_request() {
        let config = Config::default();
        let offer = handle(&config, &request(DISCOVER)).unwrap();
        assert_eq!(offer[0], BOOTREPLY);
        assert_eq!(&offer[4..8], &[1, 2, 3, 4]);
        assert_eq!(&offer[16..20], &[10, 0, 2, 15]);
        assert_eq!(message_type(&offer[OPTIONS..]), Some(OFFER));
        let ack = handle(&config, &request(REQUEST)).unwrap();
        assert_eq!(message_type(&ack[OPTIONS..]), Some(ACK));
        assert!(handle(&config, &request(7)).is_none(), "RELEASE needs no answer");
    }
}
