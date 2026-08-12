//! JSON envelopes (`docs/core-protocol.md` "JSON envelopes"). KTD1: `cmd`
//! and `event` values are exactly the Tauri seam's names — this file defines
//! only the *carrier*, never per-command shapes (those stay with their
//! commands, as today).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Client → server. `id` is client-chosen and opaque; echoed on the response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Server → client, unsolicited; fan-out to every client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub event: String,
    pub payload: Value,
}

/// Serialize a success response. Built by hand (not a struct) so the `ok` key
/// is present even for `null` results — `{"id":n,"ok":null}` is a completed
/// command, distinct from an error.
pub fn ok_response(id: u64, result: Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "id": id, "ok": result }))
        .expect("static shape serializes")
}

/// Serialize an error response.
pub fn err_response(id: u64, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "id": id, "err": message }))
        .expect("static shape serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parses_with_and_without_args() {
        let r: Request = serde_json::from_str(r#"{"id":7,"cmd":"core/ping"}"#).unwrap();
        assert_eq!(r.id, 7);
        assert_eq!(r.cmd, "core/ping");
        assert!(r.args.is_null());
        let r: Request =
            serde_json::from_str(r#"{"id":8,"cmd":"x","args":{"paneId":3}}"#).unwrap();
        assert_eq!(r.args["paneId"], 3);
    }

    #[test]
    fn responses_carry_id_and_exactly_one_of_ok_err() {
        let v: Value = serde_json::from_slice(&ok_response(9, Value::Null)).unwrap();
        assert_eq!(v["id"], 9);
        assert!(v.get("ok").is_some() && v.get("err").is_none());
        let v: Value = serde_json::from_slice(&err_response(9, "nope")).unwrap();
        assert_eq!(v["err"], "nope");
        assert!(v.get("ok").is_none());
    }

    #[test]
    fn event_roundtrips() {
        let e = Event {
            event: "pane://attention".into(),
            payload: serde_json::json!({"paneId": 1}),
        };
        let back: Event = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }
}
