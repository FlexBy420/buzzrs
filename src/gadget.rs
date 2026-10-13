//! A real Buzz! dongle made with the Linux USB gadget framework (configfs + the HID function).
//!
//! On a PC the gadget is plugged into the PC itself through dummy_hcd (a virtual USB host + device
//! controller): RPCS3 then finds a Buzz! controller and passes it through like real hardware.
//! On a board with a device-mode USB port (Raspberry Pi Zero / 4 with dwc2, ...) the same gadget
//! plugged into a PS3 is a Buzz! dongle for the console.
//!
//! `up` / `down` need root; the running server only needs read/write access to /dev/hidgN.

use crate::report::REPORT_DESCRIPTOR;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use tracing::{info, warn};

const CONFIGFS: &str = "/sys/kernel/config/usb_gadget";
/// Shortest "off" of a light that is passed on (see Dongle::start).
const LIGHT_GAP: std::time::Duration = std::time::Duration::from_millis(40);

fn write(path: impl AsRef<Path>, value: impl AsRef<[u8]>) -> Result<()> {
    let p = path.as_ref();
    fs::write(p, value).with_context(|| format!("writing {}", p.display()))
}

fn modprobe(module: &str) {
    match Command::new("modprobe").arg(module).status() {
        Ok(s) if s.success() => {}
        _ => warn!("modprobe {module} failed (built in, or missing)"),
    }
}

fn gadget_name(set: usize) -> String {
    if set == 0 {
        "buzzrs".to_string()
    } else {
        format!("buzzrs{}", set + 1)
    }
}

pub fn hidg_nodes() -> Vec<PathBuf> {
    (0..8).map_while(|i| hidg_node(&gadget_name(i)).ok()).collect()
}

/// Each dongle needs its own device controller: dummy_hcd gets `num=sets` virtual ones.
pub fn up_sets(sets: usize, owner: Option<&str>) -> Result<Vec<PathBuf>> {
    modprobe("libcomposite");
    modprobe("usb_f_hid");
    if !Path::new(CONFIGFS).exists() {
        let _ = Command::new("mount").args(["-t", "configfs", "none", "/sys/kernel/config"]).status();
    }
    for i in (0..8).rev() {
        let _ = down(&gadget_name(i));
    }
    let real: Vec<String> = udcs().into_iter().filter(|u| !u.starts_with("dummy")).collect();
    let list = if !real.is_empty() {
        if real.len() < sets {
            warn!("{} USB device port(s): {} set(s) of buzzers only", real.len(), real.len());
        }
        real
    } else {
        let dummies = udcs().into_iter().filter(|u| u.starts_with("dummy")).count();
        if dummies < sets {
            let _ = Command::new("modprobe").args(["-r", "dummy_hcd"]).status();
            let _ = Command::new("modprobe").args(["dummy_hcd", &format!("num={sets}")]).status();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        udcs().into_iter().filter(|u| u.starts_with("dummy")).collect()
    };
    let mut nodes = Vec::new();
    for (i, udc) in list.iter().take(sets).enumerate() {
        nodes.push(up(&gadget_name(i), udc, owner)?);
    }
    if nodes.is_empty() {
        bail!("no USB device controller (UDC) found; dummy_hcd not available?");
    }
    // RPCS3 tells USB devices apart by port + address only (not the bus): every dummy_hcd
    // bus puts its device on port 1 address 2, so the second dongle looked like the first one
    // and was skipped. Plugging a dongle in again gives it the next address.
    for _ in 0..10 {
        let Some(i) = clashing_set(nodes.len()) else { break };
        info!("dongle {} has the same USB port/address as another one, plugging it in again", i + 1);
        nodes[i] = up(&gadget_name(i), &list[i], owner)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    if clashing_set(nodes.len()).is_some() {
        warn!("two dongles still share a USB port/address: RPCS3 will only see one of them");
    }
    Ok(nodes)
}

fn buzz_ids() -> Vec<(u32, (String, String))> {
    let mut out = Vec::new();
    for e in fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
        let p = e.path();
        let rd = |f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        if rd("idVendor") == "054c" && rd("idProduct") == "0002" {
            let name = e.file_name().to_string_lossy().into_owned();
            let port = name.rsplit(['-', '.']).next().unwrap_or("").to_string();
            if let Ok(bus) = rd("busnum").parse() {
                out.push((bus, (port, rd("devnum"))));
            }
        }
    }
    out.sort();
    out
}

/// A set (index > 0) whose dongle clashes with an earlier one, if any. Dongles are matched to
/// sets by bus order (dummy_hcd.N makes bus N in order).
fn clashing_set(sets: usize) -> Option<usize> {
    let ids = buzz_ids();
    (1..ids.len().min(sets)).find(|&i| ids[..i].iter().any(|(_, id)| *id == ids[i].1))
}

pub fn down_all() -> Result<()> {
    for i in (0..8).rev() {
        down(&gadget_name(i))?;
    }
    Ok(())
}

fn udcs() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir("/sys/class/udc")
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// `owner`: user that gets the /dev/hidgN node (and, through dummy_hcd, the USB device).
fn up(name: &str, udc: &str, owner: Option<&str>) -> Result<PathBuf> {
    let g = Path::new(CONFIGFS).join(name);
    if g.exists() {
        down(name)?;
    }
    fs::create_dir_all(&g).with_context(|| format!("creating {} (run as root)", g.display()))?;
    write(g.join("idVendor"), "0x054c")?;
    write(g.join("idProduct"), "0x0002")?;
    // the real controller says 0x05a1, but configfs only takes BCD values
    if write(g.join("bcdDevice"), "0x05a1").is_err() {
        let _ = write(g.join("bcdDevice"), "0x0501");
    }
    write(g.join("bcdUSB"), "0x0200")?;
    write(g.join("bDeviceClass"), "0")?;
    write(g.join("bDeviceSubClass"), "0")?;
    write(g.join("bDeviceProtocol"), "0")?;
    let _ = write(g.join("bMaxPacketSize0"), "8");
    let s = g.join("strings/0x409");
    fs::create_dir_all(&s)?;
    write(s.join("manufacturer"), "Logitech")?;
    write(s.join("product"), "Logitech Buzz(tm) Controller V1")?;
    let c = g.join("configs/c.1");
    fs::create_dir_all(&c)?;
    write(c.join("MaxPower"), "100")?;
    write(c.join("bmAttributes"), "0x80")?;
    let f = g.join("functions/hid.buzz");
    fs::create_dir_all(&f)?;
    write(f.join("protocol"), "0")?;
    write(f.join("subclass"), "0")?;
    write(f.join("report_length"), "8")?;
    write(f.join("report_desc"), REPORT_DESCRIPTOR)?;
    // the real controller has no OUT endpoint: the lights come by SET_REPORT (kernel 5.19+)
    if f.join("no_out_endpoint").exists() {
        write(f.join("no_out_endpoint"), "1")?;
    }
    std::os::unix::fs::symlink(&f, c.join("hid.buzz")).context("linking the function")?;
    write(g.join("UDC"), udc)?;
    let node = hidg_node(name)?;
    info!("gadget {name} bound to {udc}: {}", node.display());
    std::thread::sleep(std::time::Duration::from_millis(500));
    if let Some(user) = owner {
        let _ = Command::new("chown").arg(user).arg(&node).status();
        // the host side of the virtual cable: let the user (RPCS3) open the "real" controller
        if udc.starts_with("dummy") {
            for dev in usb_nodes(0x054c, 0x0002) {
                let _ = Command::new("chown").arg(user).arg(&dev).status();
                info!("{} -> {user}", dev.display());
            }
        }
    }
    if udc.starts_with("dummy") {
        unbind_host_driver(0x054c, 0x0002);
    }
    Ok(node)
}

pub fn down(name: &str) -> Result<()> {
    let g = Path::new(CONFIGFS).join(name);
    if !g.exists() {
        return Ok(());
    }
    let _ = fs::write(g.join("UDC"), "\n");
    let _ = fs::remove_file(g.join("configs/c.1/hid.buzz"));
    let _ = fs::remove_dir(g.join("configs/c.1/strings/0x409"));
    let _ = fs::remove_dir(g.join("configs/c.1"));
    let _ = fs::remove_dir(g.join("functions/hid.buzz"));
    let _ = fs::remove_dir(g.join("strings/0x409"));
    fs::remove_dir(&g).with_context(|| format!("removing {} (run as root)", g.display()))?;
    info!("gadget {name} removed");
    Ok(())
}

fn hidg_node(name: &str) -> Result<PathBuf> {
    let dev = fs::read_to_string(Path::new(CONFIGFS).join(name).join("functions/hid.buzz/dev"))
        .context("the gadget is not set up (run `sudo buzzrs gadget-up`)")?;
    let minor = dev.trim().split(':').nth(1).unwrap_or("0");
    Ok(PathBuf::from(format!("/dev/hidg{minor}")))
}

fn usb_nodes(vid: u16, pid: u16) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
        let p = e.path();
        let rd = |f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        if rd("idVendor") == format!("{vid:04x}") && rd("idProduct") == format!("{pid:04x}") {
            if let (Ok(b), Ok(d)) = (rd("busnum").parse::<u32>(), rd("devnum").parse::<u32>()) {
                out.push(PathBuf::from(format!("/dev/bus/usb/{b:03}/{d:03}")));
            }
        }
    }
    out
}

/// On this machine's side of dummy_hcd the kernel's HID driver takes the "controller" (and
/// shows it as a joystick); RPCS3 needs the interface free to pass it through.
fn unbind_host_driver(vid: u16, pid: u16) {
    for e in fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten() {
        let p = e.path();
        let rd = |f: &str| fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        if rd("idVendor") != format!("{vid:04x}") || rd("idProduct") != format!("{pid:04x}") {
            continue;
        }
        let dev = e.file_name().to_string_lossy().into_owned();
        for i in fs::read_dir(&p).into_iter().flatten().flatten() {
            let name = i.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&format!("{dev}:")) {
                continue;
            }
            if let Ok(drv) = fs::canonicalize(i.path().join("driver")) {
                match fs::write(drv.join("unbind"), &name) {
                    Ok(_) => info!("{name}: host driver {} released", drv.file_name().unwrap_or_default().to_string_lossy()),
                    Err(e) => warn!("{name}: cannot unbind the host driver: {e}"),
                }
            }
        }
    }
}

/// The running dongle: input reports go out on their own thread (a write waits until the host
/// polls), output reports (lights) come back on another.
pub struct Dongle {
    tx: mpsc::SyncSender<[u8; 5]>,
}

impl Dongle {
    pub fn open(node: &Path, on_lights: impl Fn(Vec<u8>) + Send + 'static) -> Result<Dongle> {
        let w = fs::OpenOptions::new().write(true).open(node).with_context(|| format!("opening {}", node.display()))?;
        let mut r = fs::OpenOptions::new().read(true).open(node)?;
        let (ltx, lrx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => std::thread::sleep(std::time::Duration::from_millis(50)),
                    Ok(n) => {
                        if ltx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("hidg read: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        });
        Ok(Dongle::start(w, lrx, on_lights))
    }

    /// Button reports go to `w` (a write blocks until the host takes it), light reports come
    /// from `lights`.
    pub fn start(
        mut w: impl Write + Send + 'static,
        lrx: mpsc::Receiver<Vec<u8>>,
        on_lights: impl Fn(Vec<u8>) + Send + 'static,
    ) -> Dongle {
        let (tx, rx) = mpsc::sync_channel::<[u8; 5]>(64);
        std::thread::spawn(move || {
            while let Ok(mut rep) = rx.recv() {
                loop {
                    while let Ok(n) = rx.try_recv() {
                        rep = n;
                    }
                    match w.write_all(&rep) {
                        Ok(()) => break,
                        // not plugged in / not configured yet: keep the state for later
                        Err(e) => {
                            tracing::debug!("report write: {e}");
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            // The games refresh the lights by sending "all off" and a few ms later the real state
            // (Quiz TV: every ~0.55 s). A real buzzer cannot show such a gap: a report that turns a
            // light off only counts when nothing else follows within LIGHT_GAP.
            let mut shown: Vec<u8> = Vec::new();
            while let Ok(mut rep) = lrx.recv() {
                loop {
                    let dims = crate::report::lights(&rep)
                        .zip(crate::report::lights(&shown))
                        .is_some_and(|(n, o)| n.iter().zip(o.iter()).any(|(a, b)| !a && *b));
                    if !dims {
                        break;
                    }
                    match lrx.recv_timeout(LIGHT_GAP) {
                        Ok(next) => rep = next,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if rep != shown {
                    shown = rep.clone();
                    on_lights(rep);
                }
            }
        });
        Dongle { tx }
    }

    pub fn send(&self, report: [u8; 5]) {
        if self.tx.try_send(report).is_err() {
            warn!("report queue full");
        }
    }
}

pub fn check_root() -> Result<()> {
    if !Command::new("id").arg("-u").output().map(|o| o.stdout.starts_with(b"0")).unwrap_or(false) {
        bail!("this needs root (sudo)");
    }
    Ok(())
}
