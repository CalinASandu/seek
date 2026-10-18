//! One logical sync message: a small JSON header plus an optional binary payload (index copies, deltas).
//!
//! Bytes: `u32 LE json length | json | payload`.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::Error;

/// Largest frame accepted from a peer (a full copy of a very large device).
pub const MAX_FRAME: usize = 1 << 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub json: Vec<u8>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new<T: Serialize>(msg: &T, payload: Vec<u8>) -> Frame {
        Frame { json: serde_json::to_vec(msg).expect("message serializes"), payload }
    }

    pub fn msg<T: DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_slice(&self.json).map_err(|e| Error::Protocol(format!("bad message: {e}")))
    }

    pub fn len(&self) -> usize {
        4 + self.json.len() + self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        out.extend_from_slice(&(self.json.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.json);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn from_bytes(mut b: Vec<u8>) -> Result<Frame, Error> {
        if b.len() < 4 {
            return Err(Error::Protocol("frame too short".into()));
        }
        let n = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
        if 4 + n > b.len() {
            return Err(Error::Protocol("frame header length out of range".into()));
        }
        let payload = b.split_off(4 + n);
        b.drain(..4);
        Ok(Frame { json: b, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_bad_input() {
        let f = Frame::new(&serde_json::json!({"t": "hello"}), vec![1, 2, 3]);
        let back = Frame::from_bytes(f.to_bytes()).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.msg::<serde_json::Value>().unwrap()["t"], "hello");
        assert!(Frame::from_bytes(vec![1, 0]).is_err());
        assert!(Frame::from_bytes(vec![9, 0, 0, 0, 1]).is_err());
    }
}
