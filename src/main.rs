mod cam;
mod recorder;

use std::sync::Arc;
use std::thread;

use cam::{FrameBus, MjpegStream};
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// Address/port to serve on. Matches the legacy Python server and the systemd
/// unit so existing Motion Eye config keeps working.
const BIND_ADDR: &str = "0.0.0.0:6877";

const PAGE: &str = r#"<html>
<head>
<title>Coastal Cam - Table Time</title>
</head>
<body>
<img src="stream.mjpg" />
</body>
</html>
"#;

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bus = FrameBus::new();
    cam::start(bus.clone());
    // Save footage to an attached USB drive whenever we're off the HQ network,
    // so nothing is lost while Motion Eye can't reach us. Self-gates; a no-op
    // on HQ WiFi or with no drive attached.
    recorder::start(bus.clone());

    let server = Arc::new(
        Server::http(BIND_ADDR).unwrap_or_else(|e| panic!("failed to bind {BIND_ADDR}: {e}")),
    );
    log::info!("serving MJPEG on http://{BIND_ADDR}");

    // Thread-per-request, mirroring the Python ThreadingMixIn server: a stream
    // client blocks its own thread for as long as it stays connected.
    loop {
        let request = match server.recv() {
            Ok(rq) => rq,
            Err(e) => {
                log::error!("accept error: {e}");
                continue;
            }
        };
        let bus = bus.clone();
        thread::spawn(move || handle(request, bus));
    }
}

fn handle(request: tiny_http::Request, bus: Arc<FrameBus>) {
    if *request.method() != Method::Get {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return;
    }

    let path = request.url().split('?').next().unwrap_or("/");

    let result = match path {
        "/" => {
            let response =
                Response::empty(StatusCode(301)).with_header(header("Location", "/index.html"));
            request.respond(response)
        }
        "/index.html" => {
            let response =
                Response::from_string(PAGE).with_header(header("Content-Type", "text/html"));
            request.respond(response)
        }
        "/stream.mjpg" => {
            let response = Response::new(
                StatusCode(200),
                vec![
                    header("Content-Type", "multipart/x-mixed-replace; boundary=FRAME"),
                    header("Cache-Control", "no-cache, private"),
                    header("Pragma", "no-cache"),
                    header("Age", "0"),
                ],
                MjpegStream::new(bus),
                None, // no Content-Length -> chunked; runs until the client disconnects
                None,
            );
            request.respond(response)
        }
        _ => request.respond(Response::empty(StatusCode(404))),
    };

    if let Err(e) = result {
        // A client dropping mid-stream is the normal way an MJPEG view ends.
        log::debug!("response ended: {e}");
    }
}
