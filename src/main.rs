mod cam;
mod conf;
mod http;
mod recorder;
mod rotate;
mod settings;
mod snapshot;
mod status;

use std::sync::Arc;
use std::thread;

use cam::{CamControl, FrameBus};
use snapshot::Snapshots;
use tiny_http::Server;

/// Address/port to serve on. Matches the legacy Python server and the systemd
/// unit so existing Motion Eye config keeps working.
const BIND_ADDR: &str = "0.0.0.0:6877";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bus = FrameBus::new();
    // Orientation and focus start from the environment (record.conf) and can be
    // changed at runtime through the API, which also saves them back.
    let cam = CamControl::from_env();
    log::info!("initial orientation: {:?}", cam.get());
    cam::start(bus.clone(), cam.clone());
    // Saves footage to an attached USB drive whenever we're off the HQ network
    // (automatic, self-gating) or when Record is switched on in the web UI.
    let recorder = recorder::start(bus.clone());
    let app = http::App { bus, cam, recorder, snapshots: Snapshots::from_env() };

    let server = Arc::new(
        Server::http(BIND_ADDR).unwrap_or_else(|e| panic!("failed to bind {BIND_ADDR}: {e}")),
    );
    log::info!("serving on http://{BIND_ADDR}");

    // Thread-per-request, mirroring the Python ThreadingMixIn server: a stream
    // client blocks its own thread for as long as it stays connected.
    loop {
        match server.recv() {
            Ok(request) => {
                let app = app.clone();
                thread::spawn(move || http::handle(request, app));
            }
            Err(e) => log::error!("accept error: {e}"),
        }
    }
}
