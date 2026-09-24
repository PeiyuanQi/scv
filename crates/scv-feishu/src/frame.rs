//! Frames of the Feishu event long connection (`proto/pbbp2.proto`) and
//! reassembly of events the server splits across several frames.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Control frames carry ping and pong.
pub const METHOD_CONTROL: i32 = 0;
/// Data frames carry events and card callbacks.
pub const METHOD_DATA: i32 = 1;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Header {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
    #[prost(uint64, required, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub log_id: u64,
    #[prost(int32, required, tag = "3")]
    pub service: i32,
    #[prost(int32, required, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<Header>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}

impl Frame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.key == key)
            .map(|header| header.value.as_str())
    }

    fn header_number(&self, key: &str) -> Option<usize> {
        self.header(key)?.parse().ok()
    }

    /// A ping for the connection's service.
    pub fn ping(service: i32) -> Self {
        Self {
            service,
            method: METHOD_CONTROL,
            headers: vec![Header {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Default::default()
        }
    }

    /// The acknowledgement of this event frame: the same frame, with the
    /// handling time and a success response as its payload.
    pub fn acknowledgement(mut self, handled_in: Duration) -> Self {
        self.headers.push(Header {
            key: "biz_rt".into(),
            value: handled_in.as_millis().to_string(),
        });
        self.payload = Some(br#"{"code":200,"headers":null,"data":null}"#.to_vec());
        self
    }
}

/// Parts of one split event may arrive this far apart.
const FRAGMENT_TTL: Duration = Duration::from_secs(30);
/// Split events being reassembled at once.
const MAX_PARTIAL: usize = 64;
/// Parts of one event.
const MAX_PARTS: usize = 64;
/// Bytes of one reassembled event.
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

struct Partial {
    started: Instant,
    parts: Vec<Option<Vec<u8>>>,
    bytes: usize,
}

/// Reassembles events split by the `sum` and `seq` headers, within bounds.
#[derive(Default)]
pub struct Fragments {
    partial: HashMap<String, Partial>,
}

impl Fragments {
    /// The event's whole payload once every part has arrived, or `None`
    /// while parts are missing or when the frame cannot be used.
    pub fn accept(&mut self, frame: &Frame) -> Option<Vec<u8>> {
        let payload = frame.payload.clone().unwrap_or_default();
        let sum = frame.header_number("sum").unwrap_or(1);
        if sum <= 1 {
            return (payload.len() <= MAX_EVENT_BYTES).then_some(payload);
        }
        let seq = frame.header_number("seq")?;
        let id = frame.header("message_id")?.to_owned();
        if sum > MAX_PARTS || seq >= sum || id.is_empty() {
            return None;
        }
        let now = Instant::now();
        self.partial
            .retain(|_, partial| now.duration_since(partial.started) < FRAGMENT_TTL);
        if !self.partial.contains_key(&id) && self.partial.len() >= MAX_PARTIAL {
            return None;
        }
        let partial = self.partial.entry(id.clone()).or_insert_with(|| Partial {
            started: now,
            parts: vec![None; sum],
            bytes: 0,
        });
        if partial.parts.len() != sum {
            self.partial.remove(&id);
            return None;
        }
        if partial.parts[seq].is_none() {
            partial.bytes += payload.len();
            partial.parts[seq] = Some(payload);
        }
        if partial.bytes > MAX_EVENT_BYTES {
            self.partial.remove(&id);
            return None;
        }
        if partial.parts.iter().any(Option::is_none) {
            return None;
        }
        let partial = self.partial.remove(&id)?;
        Some(partial.parts.into_iter().flatten().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn part(id: &str, sum: usize, seq: usize, payload: &[u8]) -> Frame {
        Frame {
            method: METHOD_DATA,
            headers: vec![
                Header {
                    key: "message_id".into(),
                    value: id.into(),
                },
                Header {
                    key: "sum".into(),
                    value: sum.to_string(),
                },
                Header {
                    key: "seq".into(),
                    value: seq.to_string(),
                },
            ],
            payload: Some(payload.to_vec()),
            ..Default::default()
        }
    }

    #[test]
    fn frames_round_trip_with_required_fields() {
        let ping = Frame::ping(7);
        let bytes = ping.encode_to_vec();
        // Required fields are present even when zero.
        assert_eq!(&bytes[..4], &[0x08, 0x00, 0x10, 0x00]);
        let decoded = Frame::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, ping);
        assert_eq!(decoded.header("type"), Some("ping"));
    }

    #[test]
    fn acknowledgement_keeps_the_frame_and_reports_success() {
        let ack = part("m", 1, 0, b"{}").acknowledgement(Duration::from_millis(12));
        assert_eq!(ack.header("message_id"), Some("m"));
        assert_eq!(ack.header("biz_rt"), Some("12"));
        let body: serde_json::Value = serde_json::from_slice(&ack.payload.unwrap()).unwrap();
        assert_eq!(body["code"], 200);
    }

    #[test]
    fn split_events_reassemble_in_any_order() {
        let mut fragments = Fragments::default();
        assert_eq!(fragments.accept(&part("m", 3, 2, b"c")), None);
        assert_eq!(fragments.accept(&part("m", 3, 0, b"a")), None);
        assert_eq!(fragments.accept(&part("m", 3, 1, b"b")).unwrap(), b"abc");
        // A whole event passes straight through.
        assert_eq!(fragments.accept(&part("n", 1, 0, b"x")).unwrap(), b"x");
    }

    #[test]
    fn malformed_or_oversized_parts_are_dropped() {
        let mut fragments = Fragments::default();
        assert_eq!(fragments.accept(&part("m", 2, 2, b"a")), None);
        assert_eq!(fragments.accept(&part("m", MAX_PARTS + 1, 0, b"a")), None);
        assert_eq!(fragments.accept(&part("", 2, 0, b"a")), None);
        let big = vec![b'x'; MAX_EVENT_BYTES];
        assert_eq!(fragments.accept(&part("m", 2, 0, &big)), None);
        assert_eq!(fragments.accept(&part("m", 2, 1, b"y")), None);
        assert!(fragments.partial.is_empty());
    }
}
