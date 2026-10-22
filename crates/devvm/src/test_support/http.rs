//! A local HTTP server serving one artifact, with `ETag`, `Range` / `If-Range`, request records and gates the test
//! controls, so a test decides when a response stalls or breaks without any timing.
//!
//! It speaks just enough HTTP/1.1 for `reqwest`: one request per connection, `Content-Length` bodies, and a hard
//! connection close to model a dropped connection (which an HTTP library's server would paper over).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// How the server treats one request (the n-th is answered with the n-th behavior; later ones are `Normal`).
pub enum Behavior {
    Normal,
    /// Sends `after` body bytes, then closes the connection short of the declared length.
    AbortAfter(usize),
    /// Sends `after` body bytes, tells the test (`paused`), then waits for `release` before sending the rest.
    PauseAfter {
        after: usize,
        paused: Sender<()>,
        release: Receiver<()>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    pub range: Option<String>,
    pub if_range: Option<String>,
}

struct State {
    behaviors: Mutex<VecDeque<Behavior>>,
    records: Mutex<Vec<Recorded>>,
    stop: AtomicBool,
}

pub struct TestServer {
    state: Arc<State>,
    port: u16,
    accept: Option<std::thread::JoinHandle<()>>,
}

/// Body bytes go out in chunks of this size, flushed one by one, so a gate lands exactly where it says.
const CHUNK: usize = 8192;

fn serve(stream: TcpStream, data: &[u8], etag: &str, state: &State) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        head.push(line.trim_end().to_owned());
    }
    let method = head
        .first()
        .and_then(|line| line.split(' ').next())
        .unwrap_or("")
        .to_owned();
    let find = |name: &str| {
        head.iter().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim().to_owned())
        })
    };
    let (range, if_range) = (find("Range"), find("If-Range"));
    state.records.lock().unwrap().push(Recorded {
        method,
        range: range.clone(),
        if_range: if_range.clone(),
    });
    let behavior = state.behaviors.lock().unwrap().pop_front().unwrap_or(Behavior::Normal);

    let total = data.len();
    let from = range
        .as_deref()
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.strip_suffix('-'))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|_| if_range.as_deref().is_none_or(|validator| validator == etag));

    let mut out = stream;
    let (status, extra, body): (&str, String, &[u8]) = match from {
        Some(from) if from >= total => (
            "416 Range Not Satisfiable",
            format!("Content-Range: bytes */{total}\r\n"),
            &[],
        ),
        Some(from) => (
            "206 Partial Content",
            format!("ETag: {etag}\r\nContent-Range: bytes {from}-{}/{total}\r\n", total - 1),
            &data[from..],
        ),
        None => ("200 OK", format!("ETag: {etag}\r\n"), data),
    };
    write!(
        out,
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.flush()?;

    let gate = match &behavior {
        Behavior::AbortAfter(after) | Behavior::PauseAfter { after, .. } => Some(*after),
        Behavior::Normal => None,
    };
    let mut sent = 0;
    while sent < body.len() {
        if gate == Some(sent) {
            match &behavior {
                Behavior::AbortAfter(_) => {
                    // A reset-free hard close, short of the declared length.
                    return out.shutdown(Shutdown::Both);
                }
                Behavior::PauseAfter { paused, release, .. } => {
                    let _ = paused.send(());
                    // A dropped sender (the test is over) releases the gate too.
                    let _ = release.recv();
                }
                Behavior::Normal => {}
            }
        }
        let limit = gate.filter(|gate| *gate > sent).unwrap_or(body.len());
        let end = (sent + CHUNK).min(limit).min(body.len());
        out.write_all(&body[sent..end])?;
        out.flush()?;
        sent = end;
    }
    Ok(())
}

impl TestServer {
    pub fn start(data: Vec<u8>, etag: &str) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().expect("a local address").port();
        let state = Arc::new(State {
            behaviors: Mutex::new(VecDeque::new()),
            records: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let accept = {
            let state = state.clone();
            let (data, etag) = (Arc::new(data), etag.to_owned());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if state.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    let (data, etag, state) = (data.clone(), etag.clone(), state.clone());
                    // A client that went away mid-response is the point of some tests; its error is not ours.
                    std::thread::spawn(move || {
                        let _ = serve(stream, &data, &etag, &state);
                    });
                }
            })
        };
        TestServer {
            state,
            port,
            accept: Some(accept),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Sets the behaviors of the next requests, in order.
    pub fn script(&self, behaviors: Vec<Behavior>) {
        *self.state.behaviors.lock().unwrap() = behaviors.into();
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.records.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop, which is blocked in `accept`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}
