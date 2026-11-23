//! Minimal SDP client: find the RFCOMM channel of one 128-bit service UUID on a remote device.
//!
//! Our own lookup instead of BlueZ ConnectProfile: that one browses a device at most once per
//! bluetoothd run, so a phone that got our service later stayed "NotAvailable" until a restart.
//! One ServiceSearchAttributeRequest for attribute 0x0004 (ProtocolDescriptorList) over L2CAP
//! PSM 1; this is also where the phone gets paged.

use anyhow::{Context, Result, anyhow, bail, ensure};
use bluer::l2cap::{SeqPacket, SocketAddr};
use bluer::{Address, AddressType};
use std::os::fd::AsRawFd;
use uuid::Uuid;

const PSM_SDP: u16 = 1;
const PDU_ERROR: u8 = 0x01;
const PDU_SEARCH_ATTR_REQ: u8 = 0x06;
const PDU_SEARCH_ATTR_RSP: u8 = 0x07;
const ATTR_PROTOCOL_DESCRIPTORS: u16 = 0x0004;
const UUID_RFCOMM: u16 = 0x0003;
/// Bluetooth base UUID; 16/32-bit UUIDs are short forms of it.
const BASE_UUID: u128 = 0x0000_0000_0000_1000_8000_0080_5f9b_34fb;
/// Bytes per response we allow the server to send (it caps further to its MTU).
const MAX_ATTR_BYTES: u16 = 0x0400;
/// Continuation rounds before we call the server broken.
const MAX_ROUNDS: usize = 32;

/// The device answered SDP but has no record for the UUID: the app isn't listening.
#[derive(Debug)]
pub struct ServiceNotFound;

impl std::fmt::Display for ServiceNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("service not found")
    }
}

impl std::error::Error for ServiceNotFound {}

/// RFCOMM channel of `service` on `addr`. Connecting pages the device. Errors: [`ServiceNotFound`]
/// when it has no such record, the socket's io error when unreachable. Not bounded: wrap in a
/// timeout.
pub async fn rfcomm_channel(addr: Address, service: Uuid) -> Result<u8> {
    let sock = SeqPacket::connect(SocketAddr::new(addr, AddressType::BrEdr, PSM_SDP))
        .await
        .context("SDP connect")?;
    crate::btsock::settled(sock.as_raw_fd())
        .await
        .context("SDP connect")?;
    let mut buf = vec![0u8; sock.recv_mtu().unwrap_or(0).max(672)];
    let mut lists = Vec::new();
    let mut cont = Vec::new();
    for round in 0..MAX_ROUNDS {
        let tid = round as u16 + 1;
        sock.send(&request(service, tid, &cont))
            .await
            .context("SDP send")?;
        let n = sock.recv(&mut buf).await.context("SDP receive")?;
        let (part, next) = parse_response(&buf[..n], tid)?;
        lists.extend_from_slice(part);
        if next.is_empty() {
            return channel(&lists)?.ok_or_else(|| ServiceNotFound.into());
        }
        cont = next.to_vec();
    }
    bail!("SDP: no end after {MAX_ROUNDS} continuation rounds")
}

/// ServiceSearchAttributeRequest: records with `service`, attribute 0x0004 only.
fn request(service: Uuid, tid: u16, cont: &[u8]) -> Vec<u8> {
    let mut p = vec![0x35, 17, 0x1c]; // DES(17) { UUID128 }
    p.extend_from_slice(service.as_bytes());
    p.extend_from_slice(&MAX_ATTR_BYTES.to_be_bytes());
    p.extend_from_slice(&[0x35, 3, 0x09]); // DES(3) { uint16 attribute id }
    p.extend_from_slice(&ATTR_PROTOCOL_DESCRIPTORS.to_be_bytes());
    p.push(cont.len() as u8);
    p.extend_from_slice(cont);
    let mut pdu = vec![PDU_SEARCH_ATTR_REQ];
    pdu.extend_from_slice(&tid.to_be_bytes());
    pdu.extend_from_slice(&(p.len() as u16).to_be_bytes());
    pdu.extend(p);
    pdu
}

/// One response PDU → (attribute list bytes, continuation state; empty = done).
fn parse_response(pdu: &[u8], tid: u16) -> Result<(&[u8], &[u8])> {
    ensure!(pdu.len() >= 5, "SDP: short PDU ({} bytes)", pdu.len());
    let (id, got_tid) = (pdu[0], u16::from_be_bytes([pdu[1], pdu[2]]));
    let len = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
    let params = pdu.get(5..5 + len).context("SDP: truncated PDU")?;
    ensure!(got_tid == tid, "SDP: transaction id {got_tid}, sent {tid}");
    match id {
        PDU_SEARCH_ATTR_RSP => {}
        PDU_ERROR if params.len() >= 2 => {
            bail!(
                "SDP error 0x{:04x}",
                u16::from_be_bytes([params[0], params[1]])
            )
        }
        _ => bail!("SDP: unexpected PDU 0x{id:02x}"),
    }
    ensure!(params.len() >= 2, "SDP: short response");
    let count = u16::from_be_bytes([params[0], params[1]]) as usize;
    let lists = params
        .get(2..2 + count)
        .context("SDP: truncated attribute lists")?;
    let rest = &params[2 + count..];
    let cont_len = *rest.first().context("SDP: missing continuation state")? as usize;
    ensure!(
        cont_len <= 16,
        "SDP: continuation state of {cont_len} bytes"
    );
    let cont = rest
        .get(1..1 + cont_len)
        .context("SDP: truncated continuation state")?;
    Ok((lists, cont))
}

/// RFCOMM channel from the complete attribute lists; None if no record matched.
fn channel(lists: &[u8]) -> Result<Option<u8>> {
    let (records, rest) = Elem::parse(lists)?;
    ensure!(rest.is_empty(), "SDP: trailing bytes after attribute lists");
    let Elem::Seq(records) = records else {
        bail!("SDP: attribute lists not a sequence")
    };
    if records.is_empty() {
        return Ok(None);
    }
    for rec in &records {
        let Elem::Seq(attrs) = rec else { continue };
        for [id, val] in attrs.as_chunks::<2>().0 {
            if let (Elem::Uint(id), Elem::Seq(protos)) = (id, val)
                && *id == ATTR_PROTOCOL_DESCRIPTORS as u64
                && let Some(ch) = protos.iter().find_map(rfcomm_of)
            {
                return Ok(Some(ch));
            }
        }
    }
    Err(anyhow!("SDP: service record without an RFCOMM channel"))
}

/// `{ UUID RFCOMM, uint channel }` → channel.
fn rfcomm_of(proto: &Elem) -> Option<u8> {
    let Elem::Seq(p) = proto else { return None };
    match p.as_slice() {
        [Elem::Uuid(u), Elem::Uint(ch), ..]
            if *u == short_uuid(UUID_RFCOMM.into()) && (1..=30).contains(ch) =>
        {
            Some(*ch as u8)
        }
        _ => None,
    }
}

fn short_uuid(v: u32) -> u128 {
    BASE_UUID | ((v as u128) << 96)
}

/// SDP data element, only what we need to look at.
#[derive(Debug, PartialEq)]
enum Elem {
    Uint(u64),
    /// Always as 128 bit.
    Uuid(u128),
    Seq(Vec<Elem>),
    Other,
}

impl Elem {
    fn parse(b: &[u8]) -> Result<(Elem, &[u8])> {
        let (&head, b) = b.split_first().context("SDP: empty data element")?;
        let (kind, size) = (head >> 3, head & 7);
        let (len, b) = match size {
            0..=4 if kind == 0 => (0, b), // nil
            0..=4 => (1usize << size, b),
            5..=7 => {
                let n = 1usize << (size - 5);
                let raw = b.get(..n).context("SDP: truncated length")?;
                let len = raw.iter().fold(0usize, |a, &x| (a << 8) | x as usize);
                (len, &b[n..])
            }
            _ => unreachable!(),
        };
        let data = b.get(..len).context("SDP: truncated data element")?;
        let rest = &b[len..];
        let e = match kind {
            1 if len <= 8 => Elem::Uint(data.iter().fold(0u64, |a, &x| (a << 8) | x as u64)),
            3 => Elem::Uuid(match len {
                2 => short_uuid(u16::from_be_bytes([data[0], data[1]]).into()),
                4 => short_uuid(u32::from_be_bytes([data[0], data[1], data[2], data[3]])),
                16 => u128::from_be_bytes(data.try_into().unwrap()),
                _ => bail!("SDP: UUID of {len} bytes"),
            }),
            6 | 7 => {
                let mut items = Vec::new();
                let mut d = data;
                while !d.is_empty() {
                    let (e, r) = Elem::parse(d)?;
                    items.push(e);
                    d = r;
                }
                Elem::Seq(items)
            }
            _ => Elem::Other,
        };
        Ok((e, rest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::SERVICE;

    /// Attribute lists of one record: { 0x0004: { {L2CAP}, {RFCOMM, ch} } }, as Android sends it.
    fn record(ch: u8) -> Vec<u8> {
        vec![
            0x35, 0x13, // DES: all records
            0x35, 0x11, // DES: record
            0x09, 0x00, 0x04, // attr id 0x0004
            0x35, 0x0c, // DES: protocol descriptor list
            0x35, 0x03, 0x19, 0x01, 0x00, // { UUID16 L2CAP }
            0x35, 0x05, 0x19, 0x00, 0x03, 0x08, ch, // { UUID16 RFCOMM, uint8 ch }
        ]
    }

    fn response(tid: u16, lists: &[u8], cont: &[u8]) -> Vec<u8> {
        let mut p = (lists.len() as u16).to_be_bytes().to_vec();
        p.extend_from_slice(lists);
        p.push(cont.len() as u8);
        p.extend_from_slice(cont);
        let mut pdu = vec![PDU_SEARCH_ATTR_RSP];
        pdu.extend_from_slice(&tid.to_be_bytes());
        pdu.extend_from_slice(&(p.len() as u16).to_be_bytes());
        pdu.extend(p);
        pdu
    }

    #[test]
    fn request_bytes() {
        let r = request(SERVICE, 7, &[0xaa, 0xbb]);
        let mut want = vec![0x06, 0x00, 0x07, 0x00, 0x1d, 0x35, 0x11, 0x1c];
        want.extend_from_slice(SERVICE.as_bytes());
        want.extend_from_slice(&[0x04, 0x00, 0x35, 0x03, 0x09, 0x00, 0x04, 0x02, 0xaa, 0xbb]);
        assert_eq!(r, want);
        assert_eq!(*request(SERVICE, 1, &[]).last().unwrap(), 0);
    }

    #[test]
    fn single_response() {
        let pdu = response(1, &record(12), &[]);
        let (lists, cont) = parse_response(&pdu, 1).unwrap();
        assert!(cont.is_empty());
        assert_eq!(channel(lists).unwrap(), Some(12));
    }

    #[test]
    fn continuation() {
        let all = record(5);
        let (a, b) = all.split_at(9);
        let first = response(1, a, &[0x01, 0x02, 0x03]);
        let (part, cont) = parse_response(&first, 1).unwrap();
        assert_eq!(cont, &[1, 2, 3]);
        assert!(channel(part).is_err()); // half a record doesn't parse
        let mut lists = part.to_vec();
        let second = response(2, b, &[]);
        let (part, cont) = parse_response(&second, 2).unwrap();
        assert!(cont.is_empty());
        lists.extend_from_slice(part);
        assert_eq!(channel(&lists).unwrap(), Some(5));
    }

    #[test]
    fn no_record() {
        let pdu = response(1, &[0x35, 0x00], &[]);
        let (lists, _) = parse_response(&pdu, 1).unwrap();
        assert_eq!(channel(lists).unwrap(), None);
    }

    #[test]
    fn long_form_lengths_and_uuid128() {
        // DES with 1-byte length field (0x35) vs 2-byte (0x36); RFCOMM as full 128-bit UUID.
        let mut rfcomm = vec![0x1c];
        rfcomm.extend_from_slice(&short_uuid(3).to_be_bytes());
        let mut proto = vec![0x35, (rfcomm.len() + 2) as u8];
        proto.extend(rfcomm);
        proto.extend_from_slice(&[0x08, 0x1e]);
        let mut list = vec![0x36, 0x00, proto.len() as u8];
        list.extend(proto);
        let mut rec = vec![0x09, 0x00, 0x04];
        rec.extend(list);
        let mut lists = vec![0x35, (rec.len() + 2) as u8, 0x35, rec.len() as u8];
        lists.extend(rec);
        assert_eq!(channel(&lists).unwrap(), Some(30));
    }

    #[test]
    fn record_without_rfcomm() {
        let lists = [
            0x35, 0x0c, 0x35, 0x0a, 0x09, 0x00, 0x04, 0x35, 0x05, 0x35, 0x03, 0x19, 0x01, 0x00,
        ];
        assert!(
            channel(&lists)
                .unwrap_err()
                .to_string()
                .contains("without an RFCOMM")
        );
    }

    #[test]
    fn errors() {
        let err = [0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03];
        assert!(
            parse_response(&err, 1)
                .unwrap_err()
                .to_string()
                .contains("0x0003")
        );
        let pdu = response(2, &[0x35, 0x00], &[]);
        assert!(parse_response(&pdu, 1).is_err()); // wrong transaction
        assert!(parse_response(&pdu[..pdu.len() - 1], 2).is_err()); // truncated
        assert!(channel(&[0x35, 0x05, 0x35]).is_err());
    }
}
