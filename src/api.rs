//! Local control API: JSON over HTTP on the loopback interface, for hardware
//! controllers (a Stream Deck plugin) and scripts. The protocol is documented
//! in docs/control-api.md.
//!
//! The UI thread owns all state, so this module only speaks the protocol: each
//! request becomes a [`Command`] sent to the UI along with a channel to answer on.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Map, Value, json};

pub const DEFAULT_PORT: u16 = 47800;

/// Saving a long clip can take a while; everything else answers at once.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY: usize = 64 * 1024;

/// What the UI answers with: extra fields for the response, or an error.
pub type Reply = Result<Value, ApiError>;

pub struct Request {
    pub command: Command,
    pub reply: mpsc::Sender<Reply>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }

    /// The request itself is malformed.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    /// It names something that doesn't exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    /// It's valid, but not in the app's current state (e.g. while reviewing).
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }

    /// There's nothing to act on yet (no device, no footage).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, message)
    }

    /// It was fine, but it didn't work.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    fn reason(&self) -> &'static str {
        match self.status {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Internal Server Error",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ImportFrom {
    File(PathBuf),
    Url(String),
}

/// `Option<bool>` fields are switches: Some(on), or None to flip.
#[derive(Debug, PartialEq)]
pub enum Command {
    Status,
    Options,
    Record(Option<bool>),
    Mute(Option<bool>),
    Stats(Option<bool>),
    HideOverlays(Option<bool>),
    Fullscreen(Option<bool>),
    Play(Option<bool>),
    Live,
    Screenshot,
    Step(i64),
    StepSize(i64),
    Jump(f64),
    Seek(f64),
    Clip(Option<bool>),
    MarkStart,
    MarkEnd,
    ClearMarks,
    SaveClip,
    CopyClip,
    CopyFrame,
    Import(ImportFrom),
    VideoDevice(String),
    Format(String),
    /// None: audio off.
    AudioDevice(Option<String>),
    Codec(String),
    Buffer(u64),
    Aspect(String),
}

impl Command {
    /// Reads state without changing anything.
    fn is_query(&self) -> bool {
        matches!(self, Command::Status | Command::Options)
    }
}

type Build = fn(&Map<String, Value>) -> Result<Command, ApiError>;

/// Every endpoint under `/v1/`: method, path, and how to read its body.
static ROUTES: &[(&str, &str, Build)] = &[
    ("GET", "status", |_| Ok(Command::Status)),
    ("GET", "options", |_| Ok(Command::Options)),
    ("POST", "record", |b| Ok(Command::Record(switch(b)?))),
    ("POST", "mute", |b| Ok(Command::Mute(switch(b)?))),
    ("POST", "stats", |b| Ok(Command::Stats(switch(b)?))),
    ("POST", "hide-overlays", |b| Ok(Command::HideOverlays(switch(b)?))),
    ("POST", "fullscreen", |b| Ok(Command::Fullscreen(switch(b)?))),
    ("POST", "play", |b| Ok(Command::Play(switch(b)?))),
    ("POST", "live", |_| Ok(Command::Live)),
    ("POST", "screenshot", |_| Ok(Command::Screenshot)),
    ("POST", "step", |b| Ok(Command::Step(integer(b, "frames")?))),
    ("POST", "step-size", |b| Ok(Command::StepSize(integer(b, "frames")?))),
    ("POST", "jump", |b| Ok(Command::Jump(number(b, "seconds")?))),
    ("POST", "seek", |b| Ok(Command::Seek(number(b, "seconds")?))),
    ("POST", "clip", |b| Ok(Command::Clip(switch(b)?))),
    ("POST", "clip/start", |_| Ok(Command::MarkStart)),
    ("POST", "clip/end", |_| Ok(Command::MarkEnd)),
    ("POST", "clip/clear", |_| Ok(Command::ClearMarks)),
    ("POST", "clip/save", |_| Ok(Command::SaveClip)),
    ("POST", "clip/copy", |_| Ok(Command::CopyClip)),
    ("POST", "clip/copy-frame", |_| Ok(Command::CopyFrame)),
    ("POST", "import", import),
    ("POST", "video-device", |b| Ok(Command::VideoDevice(string(b, "id")?))),
    ("POST", "format", |b| Ok(Command::Format(string(b, "label")?))),
    ("POST", "audio-device", |b| match b.get("name") {
        Some(Value::Null) => Ok(Command::AudioDevice(None)),
        _ => Ok(Command::AudioDevice(Some(string(b, "name")?))),
    }),
    ("POST", "codec", |b| Ok(Command::Codec(string(b, "id")?))),
    ("POST", "buffer", |b| {
        let minutes = integer(b, "minutes")?;
        u64::try_from(minutes)
            .map(Command::Buffer)
            .map_err(|_| ApiError::bad_request("\"minutes\" must be positive"))
    }),
    ("POST", "aspect", |b| Ok(Command::Aspect(string(b, "id")?))),
];

fn switch(body: &Map<String, Value>) -> Result<Option<bool>, ApiError> {
    match body.get("on") {
        None => Ok(None),
        Some(Value::Bool(on)) => Ok(Some(*on)),
        Some(_) => Err(ApiError::bad_request("\"on\" must be true or false")),
    }
}

fn string(body: &Map<String, Value>, key: &str) -> Result<String, ApiError> {
    match body.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        _ => Err(ApiError::bad_request(format!("\"{key}\" must be a string"))),
    }
}

fn integer(body: &Map<String, Value>, key: &str) -> Result<i64, ApiError> {
    body.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::bad_request(format!("\"{key}\" must be a whole number")))
}

fn number(body: &Map<String, Value>, key: &str) -> Result<f64, ApiError> {
    body.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ApiError::bad_request(format!("\"{key}\" must be a number")))
}

fn import(body: &Map<String, Value>) -> Result<Command, ApiError> {
    match (body.get("path"), body.get("url")) {
        (Some(_), None) => Ok(Command::Import(ImportFrom::File(string(body, "path")?.into()))),
        (None, Some(_)) => Ok(Command::Import(ImportFrom::Url(string(body, "url")?))),
        _ => Err(ApiError::bad_request("give either \"path\" or \"url\"")),
    }
}

fn route(method: &str, path: &str, body: &Map<String, Value>) -> Result<Command, ApiError> {
    let name = path.strip_prefix("/v1/").map(|n| n.trim_end_matches('/'));
    let mut wrong_method = false;
    for (m, p, build) in ROUTES {
        if Some(*p) == name {
            if *m == method {
                return build(body);
            }
            wrong_method = true;
        }
    }
    if wrong_method {
        Err(ApiError::new(405, format!("{method} isn't allowed on {path}")))
    } else {
        Err(ApiError::not_found(format!("no such endpoint: {path}")))
    }
}

/// The port to listen on: `FRAMEBACK_API_PORT` (a port number, or `off`), else the default.
pub fn configured_port() -> Result<Option<u16>, String> {
    parse_port(std::env::var("FRAMEBACK_API_PORT").ok().as_deref())
}

fn parse_port(setting: Option<&str>) -> Result<Option<u16>, String> {
    match setting.map(str::trim) {
        None => Ok(Some(DEFAULT_PORT)),
        Some(off) if off.eq_ignore_ascii_case("off") => Ok(None),
        Some(port) => port
            .parse()
            .map(Some)
            .map_err(|_| format!("FRAMEBACK_API_PORT must be a port number or \"off\", not {port:?}")),
    }
}

/// Listens on the loopback interface and returns the port it got (useful when
/// asked for 0) and the stream of requests for the UI to answer.
pub fn serve(port: u16) -> io::Result<(u16, UnboundedReceiver<Request>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let port = listener.local_addr()?.port();
    let (requests, rx) = unbounded();
    thread::Builder::new().name("api".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = requests.clone();
            thread::spawn(move || {
                let _ = handle(stream, &requests);
            });
        }
    })?;
    Ok((port, rx))
}

fn handle(mut stream: TcpStream, requests: &UnboundedSender<Request>) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let result = read_request(&stream).and_then(|(method, path, body)| {
        let command = route(&method, &path, &body)?;
        respond(command, requests)
    });
    let (status, reason, body) = match result {
        Ok(body) => (200, "OK", body),
        Err(err) => (err.status, err.reason(), json!({ "ok": false, "error": err.message })),
    };
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    // Closing with unread request data makes the OS reset the connection, which
    // can discard the response before the client reads it (an oversized request).
    stream.shutdown(Shutdown::Write)?;
    io::copy(&mut (&stream).take(2 * MAX_BODY as u64), &mut io::sink())?;
    Ok(())
}

/// Runs the command; anything that changes state answers with the new status too,
/// so a controller can redraw its buttons without a second request.
fn respond(command: Command, requests: &UnboundedSender<Request>) -> Result<Value, ApiError> {
    let query = command.is_query();
    let extra = call(requests, command)?;
    if query {
        return Ok(extra);
    }
    let mut body = match extra {
        Value::Object(fields) => fields,
        _ => Map::new(),
    };
    body.insert("ok".into(), true.into());
    body.insert("status".into(), call(requests, Command::Status)?);
    Ok(Value::Object(body))
}

fn call(requests: &UnboundedSender<Request>, command: Command) -> Reply {
    let (reply, answer) = mpsc::channel();
    requests
        .unbounded_send(Request { command, reply })
        .map_err(|_| ApiError::unavailable("Frameback is shutting down"))?;
    answer
        .recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| ApiError::new(504, "Frameback didn't answer in time"))?
}

/// Reads one request: method, path (without query) and JSON object body.
/// Browsers are refused: any web page can reach a loopback port, and this API
/// can start recordings and download things.
fn read_request(stream: &TcpStream) -> Result<(String, String, Map<String, Value>), ApiError> {
    let io_err = |_: io::Error| ApiError::bad_request("couldn't read the request");
    let mut reader = BufReader::new(stream);

    let line = read_line(&mut reader)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ApiError::bad_request("malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(ApiError::bad_request("only HTTP/1.x is supported"));
    }
    let path = target.split('?').next().unwrap_or_default().to_string();
    let method = method.to_string();

    let mut length = 0;
    for _ in 0..=MAX_HEADERS {
        let header = read_line(&mut reader)?;
        if header.is_empty() {
            let mut body = vec![0; length];
            reader.read_exact(&mut body).map_err(io_err)?;
            return match body.is_empty() {
                true => Ok((method, path, Map::new())),
                false => match serde_json::from_slice(&body) {
                    Ok(Value::Object(fields)) => Ok((method, path, fields)),
                    _ => Err(ApiError::bad_request("the body must be a JSON object")),
                },
            };
        }
        let (name, value) = header.split_once(':').ok_or_else(|| ApiError::bad_request("malformed header"))?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "origin" => return Err(ApiError::new(403, "requests from web pages aren't allowed")),
            "host" if !is_loopback_host(value) => {
                return Err(ApiError::new(403, "this API only answers to localhost"));
            }
            "content-length" => {
                length = value.parse().map_err(|_| ApiError::bad_request("bad Content-Length"))?;
                if length > MAX_BODY {
                    return Err(ApiError::new(413, "body too large"));
                }
            }
            "transfer-encoding" => return Err(ApiError::bad_request("chunked bodies aren't supported")),
            _ => {}
        }
    }
    Err(ApiError::bad_request("too many headers"))
}

/// The name in a Host header. A rebinding attack arrives under its own domain.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    matches!(name.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
}

/// One line without its line ending; empty at the end of the headers.
fn read_line(reader: &mut BufReader<&TcpStream>) -> Result<String, ApiError> {
    let mut line = String::new();
    let read = reader.by_ref().take(MAX_LINE).read_line(&mut line);
    match read {
        Ok(_) if line.ends_with('\n') => Ok(line.trim_end_matches(['\r', '\n']).to_string()),
        Ok(_) => Err(ApiError::bad_request("incomplete or oversized request line")),
        Err(_) => Err(ApiError::bad_request("couldn't read the request")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn body(json: Value) -> Map<String, Value> {
        json.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn switches_are_on_off_or_flip() {
        let route = |b| route("POST", "/v1/record", &body(b));
        assert_eq!(route(json!({})).unwrap(), Command::Record(None));
        assert_eq!(route(json!({ "on": true })).unwrap(), Command::Record(Some(true)));
        assert_eq!(route(json!({ "on": false })).unwrap(), Command::Record(Some(false)));
        assert_eq!(route(json!({ "on": "yes" })).unwrap_err().status, 400);
    }

    #[test]
    fn bodies_are_checked() {
        let post = |path, b| route("POST", path, &body(b));
        assert_eq!(post("/v1/step", json!({ "frames": -5 })).unwrap(), Command::Step(-5));
        assert_eq!(post("/v1/step", json!({ "frames": 1.5 })).unwrap_err().status, 400);
        assert_eq!(post("/v1/step", json!({})).unwrap_err().status, 400);
        assert_eq!(post("/v1/jump", json!({ "seconds": 2.5 })).unwrap(), Command::Jump(2.5));
        assert_eq!(post("/v1/buffer", json!({ "minutes": -1 })).unwrap_err().status, 400);
        assert_eq!(
            post("/v1/audio-device", json!({ "name": null })).unwrap(),
            Command::AudioDevice(None)
        );
        assert_eq!(
            post("/v1/audio-device", json!({ "name": "HD60" })).unwrap(),
            Command::AudioDevice(Some("HD60".into()))
        );
        // Audio off must be asked for, not implied by a missing field.
        assert_eq!(post("/v1/audio-device", json!({})).unwrap_err().status, 400);
        assert_eq!(
            post("/v1/import", json!({ "url": "https://example.com/v" })).unwrap(),
            Command::Import(ImportFrom::Url("https://example.com/v".into()))
        );
        assert_eq!(post("/v1/import", json!({ "url": "x", "path": "y" })).unwrap_err().status, 400);
        assert_eq!(post("/v1/import", json!({})).unwrap_err().status, 400);
    }

    #[test]
    fn routes_check_method_and_path() {
        let none = Map::new();
        assert_eq!(route("GET", "/v1/status", &none).unwrap(), Command::Status);
        assert_eq!(route("POST", "/v1/clip/save", &none).unwrap(), Command::SaveClip);
        assert_eq!(route("POST", "/v1/clip/save/", &none).unwrap(), Command::SaveClip);
        assert_eq!(route("POST", "/v1/status", &none).unwrap_err().status, 405);
        assert_eq!(route("GET", "/v1/record", &none).unwrap_err().status, 405);
        assert_eq!(route("GET", "/v1/nope", &none).unwrap_err().status, 404);
        assert_eq!(route("GET", "/status", &none).unwrap_err().status, 404);
    }

    #[test]
    fn port_comes_from_the_environment_or_defaults() {
        assert_eq!(parse_port(None), Ok(Some(DEFAULT_PORT)));
        assert_eq!(parse_port(Some("5000")), Ok(Some(5000)));
        assert_eq!(parse_port(Some("OFF")), Ok(None));
        assert!(parse_port(Some("http")).is_err());
        assert!(parse_port(Some("70000")).is_err());
    }

    #[test]
    fn only_loopback_hosts_are_accepted() {
        for host in ["localhost", "localhost:47800", "LOCALHOST", "127.0.0.1:1", "[::1]:47800"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["evil.example", "evil.example:47800", "localhost.evil.example", "127.0.0.1.evil.example"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    /// A stand-in for the UI: answers Status, refuses Record, accepts the rest.
    fn start() -> u16 {
        let (port, mut requests) = serve(0).unwrap();
        thread::spawn(move || {
            while let Some(request) = futures::executor::block_on(requests.next()) {
                let reply = match request.command {
                    Command::Status => Ok(json!({ "recording": false })),
                    Command::Record(_) => Err(ApiError::conflict("Go LIVE first")),
                    Command::Screenshot => Ok(json!({ "path": "/tmp/shot.png" })),
                    _ => Ok(Value::Null),
                };
                let _ = request.reply.send(reply);
            }
        });
        port
    }

    /// Sends `raw` and returns the status code and JSON body of the response.
    fn send(port: u16, raw: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let (_, json) = response.split_once("\r\n\r\n").unwrap();
        (status, serde_json::from_str(json).unwrap())
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
        let host = format!("localhost:{port}");
        send(port, &format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }

    #[test]
    fn queries_answer_with_the_state_itself() {
        let port = start();
        let (status, json) = send(port, &format!("GET /v1/status?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"));
        assert_eq!((status, json), (200, json!({ "recording": false })));
    }

    #[test]
    fn commands_answer_with_the_new_state_and_their_results() {
        let port = start();
        let (status, json) = post(port, "/v1/screenshot", "");
        assert_eq!(status, 200);
        assert_eq!(
            json,
            json!({ "ok": true, "path": "/tmp/shot.png", "status": { "recording": false } })
        );
    }

    #[test]
    fn refusals_carry_their_status_and_reason() {
        let port = start();
        let (status, json) = post(port, "/v1/record", "{\"on\":true}");
        assert_eq!((status, json), (409, json!({ "ok": false, "error": "Go LIVE first" })));
        assert_eq!(post(port, "/v1/record", "[1]").0, 400);
        assert_eq!(post(port, "/v1/record", "{oops").0, 400);
        assert_eq!(post(port, "/v1/nope", "").0, 404);
    }

    #[test]
    fn web_pages_and_foreign_hosts_are_refused() {
        let port = start();
        let browser = format!("POST /v1/record HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: https://evil.example\r\n\r\n");
        assert_eq!(send(port, &browser).0, 403);
        let rebinding = "POST /v1/record HTTP/1.1\r\nHost: evil.example:47800\r\n\r\n";
        assert_eq!(send(port, rebinding).0, 403);
    }

    #[test]
    fn oversized_and_malformed_requests_are_refused() {
        let port = start();
        let big = format!("POST /v1/record HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(send(port, &big).0, 413);
        let long_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE as usize));
        assert_eq!(send(port, &long_line).0, 400);
        assert_eq!(send(port, "nonsense\r\n\r\n").0, 400);
        let chunked = "POST /v1/record HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(send(port, chunked).0, 400);
    }
}
