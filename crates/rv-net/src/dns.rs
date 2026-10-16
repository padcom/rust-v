//! The DNS server at the virtual DNS address: answers A queries through the host's resolver
//! (`getaddrinfo`), so it works with whatever DNS setup the host has, on every OS.

use std::net::{Ipv4Addr, ToSocketAddrs};

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;
const RCODE_FORMERR: u16 = 1;
const RCODE_NXDOMAIN: u16 = 3;
const TTL: u32 = 60;

pub struct Query {
    id: u16,
    recursion_desired: bool,
    /// The question section as received.
    question: Vec<u8>,
    pub name: String,
    pub qtype: u16,
    qclass: u16,
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// Parses a standard query with exactly one question.
pub fn parse(msg: &[u8]) -> Option<Query> {
    let flags = be16(msg, 2)?;
    if flags & 0x8000 != 0 || (flags >> 11) & 0xf != 0 || be16(msg, 4)? != 1 {
        return None;
    }
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let len = *msg.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None; // compression pointers do not occur in a lone question
        }
        labels.push(String::from_utf8_lossy(msg.get(i..i + len)?).into_owned());
        i += len;
    }
    let qtype = be16(msg, i)?;
    let qclass = be16(msg, i + 2)?;
    Some(Query {
        id: be16(msg, 0)?,
        recursion_desired: flags & 0x0100 != 0,
        question: msg[12..i + 4].to_vec(),
        name: labels.join("."),
        qtype,
        qclass,
    })
}

/// A FORMERR reply to a message that could not be parsed, if it has an ID to answer to.
pub fn format_error(msg: &[u8]) -> Option<Vec<u8>> {
    let id = be16(msg, 0)?;
    let mut r = id.to_be_bytes().to_vec();
    r.extend_from_slice(&(0x8180 | RCODE_FORMERR).to_be_bytes());
    r.extend_from_slice(&[0; 8]);
    Some(r)
}

/// Resolves the query on the host (blocking) and builds the reply.
pub fn resolve(q: &Query) -> Vec<u8> {
    if q.qclass != CLASS_IN {
        return reply(q, 0, &[]);
    }
    match (q.name.as_str(), 0).to_socket_addrs() {
        Ok(addrs) => {
            let v4: Vec<Ipv4Addr> = addrs
                .filter_map(|a| match a.ip() {
                    std::net::IpAddr::V4(ip) => Some(ip),
                    _ => None,
                })
                .collect();
            // Only IPv4 is routed, so other record types get an empty answer.
            reply(q, 0, if q.qtype == TYPE_A { &v4 } else { &[] })
        }
        Err(_) => reply(q, RCODE_NXDOMAIN, &[]),
    }
}

fn reply(q: &Query, rcode: u16, answers: &[Ipv4Addr]) -> Vec<u8> {
    let flags = 0x8080 | if q.recursion_desired { 0x0100 } else { 0 } | rcode;
    let mut r = Vec::with_capacity(64 + 16 * answers.len());
    r.extend_from_slice(&q.id.to_be_bytes());
    r.extend_from_slice(&flags.to_be_bytes());
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q.question);
    for ip in answers {
        r.extend_from_slice(&[0xc0, 12]); // pointer to the question's name
        r.extend_from_slice(&TYPE_A.to_be_bytes());
        r.extend_from_slice(&CLASS_IN.to_be_bytes());
        r.extend_from_slice(&TTL.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&ip.octets());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        m
    }

    #[test]
    fn answers_localhost() {
        let q = parse(&query("localhost", TYPE_A)).unwrap();
        assert_eq!((q.name.as_str(), q.qtype), ("localhost", TYPE_A));
        let r = resolve(&q);
        assert_eq!(&r[0..2], &[0xab, 0xcd]);
        assert_eq!(r[3] & 0xf, 0, "NOERROR");
        assert_eq!(be16(&r, 6), Some(1), "one answer");
        assert_eq!(&r[r.len() - 4..], &[127, 0, 0, 1]);
    }

    #[test]
    fn empty_answer_for_aaaa() {
        let q = parse(&query("localhost", 28)).unwrap();
        let r = resolve(&q);
        assert_eq!(be16(&r, 6), Some(0));
    }

    #[test]
    fn rejects_responses() {
        let mut m = query("example.com", TYPE_A);
        m[2] |= 0x80;
        assert!(parse(&m).is_none());
        assert!(format_error(&m).is_some());
    }
}
