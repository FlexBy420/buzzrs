//! buzzrs: phones as Buzz! buzzers.
//!
//!   buzzrs [--set N]                           the app: sets up the dongle(s), serves, opens the
//!                                              host page (Linux: asks for the password once;
//!                                              rooted Android: uses FunctionFS)
//!   sudo buzzrs gadget-up [--sets 2] [--owner USER]   Linux USB gadget dongles (needs root)
//!   sudo buzzrs gadget-down
//!   buzzrs serve [--port 8000] [--backend gadget|ffs|none] [--set N] [--open]
//!
//! `serve` hosts the controller page: a phone that opens http://<this machine>:<port>/ (same
//! network) takes the first free buzzer and its presses go to the dongles. Buzzers 1-4 are the
//! first set, 5-8 the second (the 8-player Buzz! games ask for two sets). A server whose dongle
//! is a second set of its own (another phone-dongle) numbers its buzzers 5-8 with `--set 2`.

mod ffs;
mod gadget;
mod report;

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use report::{Button, Buttons, PLAYERS};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Sets of buzzers (dongles) made by default: two, for the 8-player Buzz! games.
const DEFAULT_SETS: usize = 2;

const PAGE: &str = include_str!("../web/index.html");
const HOST_PAGE: &str = include_str!("../web/host.html");
const TEST_PAGE: &str = include_str!("../web/test.html");

enum Output {
    Dongles(Vec<gadget::Dongle>),
    Log,
}

struct Hub {
    slots: Vec<Option<u64>>,
    buttons: Vec<Buttons>,
    lights: Vec<bool>,
    next_id: u64,
    /// browser token -> phone number, so a refreshed page is still the same phone
    phones: HashMap<String, u64>,
    conns: HashMap<u64, u64>,
}

impl Hub {
    fn new(sets: usize) -> Hub {
        Hub { slots: vec![None; sets * PLAYERS], buttons: vec![Buttons::default(); sets], lights: vec![false; sets * PLAYERS], next_id: 0, phones: HashMap::new(), conns: HashMap::new() }
    }

    fn pressed(&self, slot: usize) -> u8 {
        self.buttons[slot / PLAYERS].0[slot % PLAYERS]
    }
}

struct App {
    hub: Mutex<Hub>,
    out: Output,
    url: String,
    /// buzzers are numbered from first + 1 (`--set 2`: 4)
    first: usize,
    device: String,
    quit: tokio::sync::Notify,
    events: broadcast::Sender<String>,
}

impl App {
    fn push(&self, h: &Hub, set: usize) {
        let r = h.buttons[set].report();
        match &self.out {
            Output::Dongles(d) => {
                if let Some(d) = d.get(set) {
                    d.send(r)
                }
            }
            Output::Log => info!("set {} report {:02x?}", set + 1 + self.first / PLAYERS, r),
        }
    }

    fn status(h: &Hub) -> String {
        json!({
            "t": "status",
            "taken": h.slots.iter().map(Option::is_some).collect::<Vec<_>>(),
            "lights": h.lights,
            "buttons": (0..h.slots.len()).map(|i| h.pressed(i)).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn release_all(&self) {
        let mut h = self.hub.lock().unwrap();
        for set in 0..h.buttons.len() {
            h.buttons[set] = Buttons::default();
            self.push(&h, set);
        }
    }

    fn set_lights(&self, set: usize, lights: [bool; PLAYERS]) {
        let mut h = self.hub.lock().unwrap();
        let range = set * PLAYERS..(set + 1) * PLAYERS;
        if range.end <= h.lights.len() && h.lights[range.clone()] != lights {
            h.lights[range].copy_from_slice(&lights);
            let _ = self.events.send(Self::status(&h));
        }
    }
}

#[derive(Deserialize)]
struct Join {
    p: Option<usize>,
    host: Option<u8>,
    d: Option<String>,
}

async fn ws(ws: WebSocketUpgrade, Query(q): Query<Join>, State(app): State<Arc<App>>) -> impl IntoResponse {
    if q.host.is_some() {
        return ws.on_upgrade(move |s| host_client(s, app));
    }
    ws.on_upgrade(move |s| client(s, q.p, q.d, app))
}

async fn host_client(mut socket: WebSocket, app: Arc<App>) {
    let players = app.hub.lock().unwrap().slots.len();
    let hello = json!({"t": "host", "url": app.url, "device": app.device, "players": players, "first": app.first + 1}).to_string();
    let first = App::status(&app.hub.lock().unwrap());
    if socket.send(Message::Text(hello)).await.is_err() || socket.send(Message::Text(first)).await.is_err() {
        return;
    }
    let mut events = app.events.subscribe();
    loop {
        tokio::select! {
            m = socket.recv() => { if !matches!(m, Some(Ok(_))) { break } }
            e = events.recv() => match e {
                Ok(s) => { if socket.send(Message::Text(s)).await.is_err() { break } }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }
}

fn qr_svg(url: &str) -> String {
    qrcode::QrCode::new(url.as_bytes())
        .map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
        .unwrap_or_default()
}

async fn client(socket: WebSocket, want: Option<usize>, token: Option<String>, app: Arc<App>) {
    let (id, phone, back, slot) = {
        let mut h = app.hub.lock().unwrap();
        h.next_id += 1;
        let id = h.next_id;
        let known = token.as_ref().and_then(|t| h.phones.get(t).copied());
        let phone = known.unwrap_or_else(|| h.phones.len() as u64 + 1);
        if let (Some(t), None) = (&token, known) {
            h.phones.insert(t.clone(), phone);
        }
        h.conns.insert(id, phone);
        let n = h.slots.len();
        // free, or still held by this phone's previous page (refresh before the old one closed)
        let free = |h: &Hub, i: usize| {
            i < n && h.slots[i].is_none_or(|c| known.is_some() && h.conns.get(&c) == Some(&phone) && c != id)
        };
        let slot = want.and_then(|p| p.checked_sub(1 + app.first)).filter(|&i| free(&h, i)).or_else(|| (0..n).find(|&i| free(&h, i)));
        if let Some(s) = slot {
            // taken over from the old page: drop what it was holding
            if h.slots[s].is_some() && h.pressed(s) != 0 {
                h.buttons[s / PLAYERS].clear(s % PLAYERS);
                app.push(&h, s / PLAYERS);
            }
            h.slots[s] = Some(id);
            let _ = app.events.send(App::status(&h));
        }
        (id, phone, known.is_some(), slot)
    };
    let (mut tx, mut rx) = socket.split();
    let players = app.hub.lock().unwrap().slots.len();
    let hello = json!({"t": "you", "player": slot.map(|s| s + 1 + app.first), "players": players, "first": app.first + 1}).to_string();
    if tx.send(Message::Text(hello)).await.is_err() {
        release(&app, id, phone, slot);
        return;
    }
    let first = App::status(&app.hub.lock().unwrap());
    let _ = tx.send(Message::Text(first)).await;
    info!("phone {phone} {}: buzzer {}", if back { "back" } else { "joined" }, slot.map(|s| (s + 1 + app.first).to_string()).unwrap_or_else(|| "none (all taken)".into()));
    let mut events = app.events.subscribe();
    loop {
        tokio::select! {
            m = rx.next() => {
                let Some(Ok(m)) = m else { break };
                let Message::Text(t) = m else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                match v["t"].as_str() {
                    // {"t":"b","b":"red","d":true}
                    Some("b") => {
                        let (Some(s), Some(b)) = (slot, v["b"].as_str().and_then(Button::from_name)) else { continue };
                        let mut h = app.hub.lock().unwrap();
                        let set = s / PLAYERS;
                        let before = h.buttons[set];
                        h.buttons[set].set(s % PLAYERS, b, v["d"].as_bool().unwrap_or(false));
                        if h.buttons[set] != before {
                            app.push(&h, set);
                            let _ = app.events.send(App::status(&h));
                        }
                    }
                    Some("ping") => {
                        let _ = tx.send(Message::Text(json!({"t": "pong", "c": v["c"]}).to_string())).await;
                    }
                    _ => {}
                }
            }
            e = events.recv() => {
                match e {
                    Ok(s) => { if tx.send(Message::Text(s)).await.is_err() { break } }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        }
    }
    release(&app, id, phone, slot);
}

fn release(app: &App, id: u64, phone: u64, slot: Option<usize>) {
    let mut h = app.hub.lock().unwrap();
    h.conns.remove(&id);
    if let Some(s) = slot {
        if h.slots[s] == Some(id) {
            h.slots[s] = None;
            // a phone that went away must not leave a button held down
            if h.pressed(s) != 0 {
                h.buttons[s / PLAYERS].clear(s % PLAYERS);
                app.push(&h, s / PLAYERS);
            }
            let _ = app.events.send(App::status(&h));
        }
    }
    info!("phone {phone} left");
}

fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.255.255:9").or_else(|_| s.connect("8.8.8.8:80")).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn print_qr(url: &str) {
    if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
        let s = code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
        println!("{s}");
    }
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gadget-up") => {
            gadget::check_root()?;
            let owner = arg(&args, "--owner").map(str::to_string).or_else(|| std::env::var("SUDO_USER").ok());
            let sets = arg(&args, "--sets").and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SETS).clamp(1, 8);
            let nodes = gadget::up_sets(sets, owner.as_deref())?;
            for (i, n) in nodes.iter().enumerate() {
                println!("Buzz! dongle {} (buzzers {}-{}): {}", i + 1, i * PLAYERS + 1, i * PLAYERS + PLAYERS, n.display());
            }
            Ok(())
        }
        Some("gadget-down") => {
            gadget::check_root()?;
            gadget::down_all()
        }
        Some("serve") => serve(&args).await,
        None => app_mode(&[]).await,
        Some(a) if a.starts_with("--") => app_mode(&args).await,
        Some(other) => bail!("unknown command {other}; use serve, gadget-up or gadget-down"),
    }
}

/// Without a dongle it still runs, in test mode.
async fn app_mode(extra: &[String]) -> Result<()> {
    if health(8000).await {
        open_browser("http://127.0.0.1:8000/host");
        return Ok(());
    }
    let mut args = vec!["serve".to_string(), "--open".to_string(), "--find-port".to_string()];
    args.extend_from_slice(extra);
    if ffs::available() {
        args.extend(["--backend".to_string(), "ffs".to_string()]);
        return serve(&args).await;
    }
    if cfg!(target_os = "linux") && gadget::hidg_nodes().len() < DEFAULT_SETS {
        let exe = std::env::current_exe()?;
        let user = std::env::var("USER").unwrap_or_default();
        info!("setting up the USB dongle (the system asks for your password)");
        let ok = std::process::Command::new("pkexec")
            .arg(&exe)
            .args(["gadget-up", "--owner", &user, "--sets", &DEFAULT_SETS.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            warn!("no USB dongle: running in test mode");
        }
    }
    if gadget::hidg_nodes().is_empty() {
        args.extend(["--backend".to_string(), "none".to_string()]);
    }
    let r = serve(&args).await;
    if let Err(e) = &r {
        // no terminal when double-clicked: show the error on the desktop
        let _ = std::process::Command::new("notify-send").args(["buzzrs", &format!("{e:#}")]).status();
    }
    r
}

/// SIGTERM / SIGHUP (closed terminal, `kill`): stop cleanly, the phone gets adb back.
#[cfg(unix)]
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match (signal(SignalKind::terminate()), signal(SignalKind::hangup())) {
        (Ok(mut t), Ok(mut h)) => {
            tokio::select! {
                _ = t.recv() => {}
                _ = h.recv() => {}
            }
        }
        _ => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn stop_signal() {
    std::future::pending().await
}

async fn health(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(Ok(mut s)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), tokio::net::TcpStream::connect(("127.0.0.1", port))).await
    else {
        return false;
    };
    let _ = s.write_all(b"GET /health HTTP/1.0\r\n\r\n").await;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), s.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).ends_with("buzzrs")
}

fn open_browser(url: &str) {
    if Path::new("/system/bin/am").exists() {
        let _ = std::process::Command::new("/system/bin/am")
            .args(["start", "--user", "0", "-a", "android.intent.action.VIEW", "-d", url])
            .stdout(std::process::Stdio::null())
            .spawn();
        return;
    }
    let cmd = if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(cmd.0).args(&cmd.1).spawn();
}

async fn serve(args: &[String]) -> Result<()> {
    let mut port: u16 = arg(args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8000);
    // app mode: if the port is taken by something else, take the next free one
    let listener = loop {
        match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(l) => break l,
            Err(e) if args.iter().any(|a| a == "--find-port") && port < 8010 => {
                warn!("port {port}: {e}, trying {}", port + 1);
                port += 1;
            }
            Err(e) => return Err(e).with_context(|| format!("port {port} is in use (is buzzrs already running?)")),
        }
    };
    let backend = arg(args, "--backend").unwrap_or("gadget");
    let (events, _) = broadcast::channel(64);
    let lights_cb: Arc<Mutex<Option<Arc<App>>>> = Arc::new(Mutex::new(None));
    let out = match backend {
        "none" => Output::Log,
        "ffs" => {
            let cb = lights_cb.clone();
            let d = ffs::up(move |rep| {
                tracing::debug!("lights report {:02x?}", rep);
                if let (Some(l), Some(app)) = (report::lights(&rep), cb.lock().unwrap().clone()) {
                    app.set_lights(0, l);
                }
            })?;
            Output::Dongles(vec![d])
        }
        "gadget" => {
            let nodes = gadget::hidg_nodes();
            if nodes.is_empty() {
                bail!("no Buzz! dongle is set up (run `sudo buzzrs gadget-up`)");
            }
            let mut dongles = Vec::new();
            for (set, node) in nodes.iter().enumerate() {
                let cb = lights_cb.clone();
                let d = gadget::Dongle::open(node, move |rep| {
                    tracing::debug!("set {} lights report {:02x?}", set + 1, rep);
                    if let (Some(l), Some(app)) = (report::lights(&rep), cb.lock().unwrap().clone()) {
                        app.set_lights(set, l);
                    }
                })?;
                info!("dongle {} (buzzers {}-{}): {}", set + 1, set * PLAYERS + 1, set * PLAYERS + PLAYERS, node.display());
                dongles.push(d);
            }
            Output::Dongles(dongles)
        }
        b => bail!("unknown backend {b} (gadget, ffs or none)"),
    };
    let set_arg = arg(args, "--set").and_then(|s| s.parse::<usize>().ok());
    let sets = match &out {
        Output::Dongles(d) => d.len(),
        Output::Log if set_arg.is_some() => 1,
        Output::Log => DEFAULT_SETS,
    };
    let url = format!("http://{}:{port}/", lan_ip().map(|i| i.to_string()).unwrap_or_else(|| "localhost".into()));
    let first = (set_arg.unwrap_or(1).max(1) - 1) * PLAYERS;
    let range = format!("buzzers {}-{}", first + 1, first + sets * PLAYERS);
    let device = match &out {
        Output::Dongles(d) if d.len() == 1 => format!("USB Buzz! dongle ready ({range})"),
        Output::Dongles(d) => format!("{} USB Buzz! dongles ready ({range})", d.len()),
        Output::Log => "Test mode: no USB dongle (the presses are only shown here)".to_string(),
    };
    let app = Arc::new(App {
        hub: Mutex::new(Hub::new(sets)),
        out,
        url: url.clone(),
        first,
        device,
        quit: tokio::sync::Notify::new(),
        events,
    });
    *lights_cb.lock().unwrap() = Some(app.clone());
    app.release_all();
    let router = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/ws", get(ws))
        .route("/health", get(|| async { "buzzrs" }))
        .route("/host", get(|| async { Html(HOST_PAGE) }))
        .route("/test", get(|| async { Html(TEST_PAGE) }))
        .route(
            "/qr.svg",
            get(|State(app): State<Arc<App>>| async move { ([("content-type", "image/svg+xml")], qr_svg(&app.url)) }),
        )
        .route(
            "/quit",
            axum::routing::post(|State(app): State<Arc<App>>, req: axum::http::Request<axum::body::Body>| async move {
                // only from this machine
                let local = req
                    .extensions()
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                    .is_some_and(|c| c.0.ip().is_loopback());
                if local {
                    app.quit.notify_one();
                    "bye"
                } else {
                    "no"
                }
            }),
        )
        .with_state(app.clone());
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("\nOpen on the phones (same Wi-Fi): {url}\n");
    print_qr(&url);
    info!("serving on {addr} (backend {backend})");
    if args.iter().any(|a| a == "--open") {
        open_browser(&format!("http://127.0.0.1:{port}/host"));
    }
    let app2 = app.clone();
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .tcp_nodelay(true)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = app2.quit.notified() => {}
                _ = stop_signal() => {}
            }
            app2.release_all();
            // open WebSockets (phones, host page) would keep the server alive: give up on
            // them after a moment
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                ffs::restore();
                std::process::exit(0);
            });
        })
        .await?;
    app.release_all();
    ffs::restore();
    warn!("stopped");
    Ok(())
}
