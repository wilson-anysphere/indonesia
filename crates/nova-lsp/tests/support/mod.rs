#![allow(dead_code)]

use serde_json::Value;
use std::io::{BufRead, Write};

pub fn write_jsonrpc_message(writer: &mut impl Write, message: &Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

pub fn read_jsonrpc_message(reader: &mut impl BufRead) -> Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

pub fn read_response_with_id(reader: &mut impl BufRead, id: i64) -> Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("method").is_some() {
            continue;
        }
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}

pub fn drain_notifications_until_id(reader: &mut impl BufRead, id: i64) -> (Vec<Value>, Value) {
    let mut notifications = Vec::new();
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return (notifications, msg);
        }

        // JSON-RPC notifications have `method` without `id`. We keep everything else
        // (including server->client requests) for debugging/optional assertions.
        notifications.push(msg);
    }
}
