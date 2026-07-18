//! JSON-RPC 2.0 codec for the ACP southbound transport.
//!
//! Framing is newline-delimited JSON (one message per line), the stdio
//! convention ACP uses. The codec is transport-agnostic; [`super::client`]
//! lays the stdio transport on top.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
#[error("json-rpc codec error: {0}")]
pub struct CodecError(pub String);

/// JSON-RPC request/response id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Num(i64),
    Str(String),
    Null,
}

impl Id {
    fn to_value(&self) -> Value {
        match self {
            Id::Num(n) => json!(n),
            Id::Str(s) => json!(s),
            Id::Null => Value::Null,
        }
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(match v {
            Value::Number(n) => Id::Num(n.as_i64().unwrap_or(0)),
            Value::String(s) => Id::Str(s),
            Value::Null => Id::Null,
            _ => Id::Null,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A single JSON-RPC 2.0 message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request {
        id: Id,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Id,
        result: Result<Value, RpcError>,
    },
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = Map::new();
        m.insert("jsonrpc".into(), json!("2.0"));
        match self {
            Message::Request { id, method, params } => {
                m.insert("method".into(), json!(method));
                m.insert("params".into(), params.clone());
                m.insert("id".into(), id.to_value());
            }
            Message::Notification { method, params } => {
                m.insert("method".into(), json!(method));
                m.insert("params".into(), params.clone());
            }
            Message::Response { id, result } => {
                m.insert("id".into(), id.to_value());
                match result {
                    Ok(v) => {
                        m.insert("result".into(), v.clone());
                    }
                    Err(e) => {
                        m.insert("error".into(), json!(e));
                    }
                }
            }
        }
        m.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        from_value(&v).map_err(serde::de::Error::custom)
    }
}

fn from_value(v: &Value) -> Result<Message, String> {
    let obj = v.as_object().ok_or("message must be a JSON object")?;
    if obj.get("jsonrpc").and_then(|x| x.as_str()) != Some("2.0") {
        return Err("missing or wrong jsonrpc version".into());
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if let Some(method) = obj.get("method").and_then(|x| x.as_str()) {
        let method = method.to_string();
        if obj.contains_key("id") {
            let id = obj.get("id").map(id_from_value).unwrap_or(Id::Null);
            Ok(Message::Request { id, method, params })
        } else {
            Ok(Message::Notification { method, params })
        }
    } else if obj.contains_key("result") || obj.contains_key("error") {
        let id = obj.get("id").map(id_from_value).unwrap_or(Id::Null);
        let result = if let Some(e) = obj.get("error") {
            Err(serde_json::from_value(e.clone()).map_err(|x| x.to_string())?)
        } else {
            Ok(obj.get("result").cloned().unwrap_or(Value::Null))
        };
        Ok(Message::Response { id, result })
    } else {
        Err("message has neither method nor result/error".into())
    }
}

fn id_from_value(v: &Value) -> Id {
    match v {
        Value::Number(n) => Id::Num(n.as_i64().unwrap_or(0)),
        Value::String(s) => Id::Str(s.clone()),
        Value::Null => Id::Null,
        _ => Id::Null,
    }
}

/// Encode a message as a newline-terminated frame.
pub fn encode_line(msg: &Message) -> String {
    format!(
        "{}\n",
        serde_json::to_string(msg).expect("message serializes")
    )
}

/// Decode a single newline-delimited frame.
pub fn decode_line(line: &str) -> Result<Message, CodecError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(CodecError("empty frame".into()));
    }
    serde_json::from_str(line).map_err(|e| CodecError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let m = Message::Request {
            id: Id::Num(1),
            method: "session/new".into(),
            params: json!({"x": 1}),
        };
        let s = encode_line(&m);
        assert_eq!(decode_line(&s).unwrap(), m);
    }

    #[test]
    fn notification_no_id() {
        let m = Message::Notification {
            method: "session/update".into(),
            params: json!({"a": 1}),
        };
        let s = encode_line(&m);
        assert_eq!(decode_line(&s).unwrap(), m);
        // id must be absent
        assert!(!s.contains("\"id\""));
    }

    #[test]
    fn response_ok_and_error() {
        let ok = Message::Response {
            id: Id::Num(2),
            result: Ok(json!({"ok": true})),
        };
        assert_eq!(decode_line(&encode_line(&ok)).unwrap(), ok);
        let err = Message::Response {
            id: Id::Str("abc".into()),
            result: Err(RpcError {
                code: -32000,
                message: "boom".into(),
                data: None,
            }),
        };
        assert_eq!(decode_line(&encode_line(&err)).unwrap(), err);
    }

    #[test]
    fn rejects_wrong_version() {
        let bad = "{\"jsonrpc\":\"1.0\",\"method\":\"x\"}";
        assert!(decode_line(bad).is_err());
    }

    #[test]
    fn rejects_unknown_shape() {
        assert!(decode_line("{\"jsonrpc\":\"2.0\"}").is_err());
    }
}
