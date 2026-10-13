//! A Buzz! dongle on an Android phone (root), without a custom kernel.
//!
//! Older Android kernels (e.g. Samsung's 3.18 on the Galaxy J7 2017) have no configfs HID
//! function, but their `android_usb` gadget has FunctionFS (`ffs`), which adbd normally uses at
//! /dev/usb-ffs/adb. FunctionFS lets a program describe its own USB function: we stop adbd, give
//! the function a HID interface with the Buzz! report descriptor, set the device to 054c:0002
//! and answer the host's requests ourselves. `restore()` gives USB back to adbd (a reboot does
//! too).

use crate::report::REPORT_DESCRIPTOR;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

const FFS: &str = "/dev/usb-ffs/adb";
const ANDROID: &str = "/sys/class/android_usb/android0";
const SAVED_ATTRS: [&str; 6] = ["idVendor", "idProduct", "bcdDevice", "iManufacturer", "iProduct", "functions"];

static SAVED: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

pub fn available() -> bool {
    Path::new(ANDROID).join("f_ffs").exists() && Path::new(FFS).join("ep0").exists()
}

fn attr(name: &str) -> String {
    fs::read_to_string(Path::new(ANDROID).join(name)).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn set_attr(name: &str, value: &str) {
    if let Err(e) = fs::write(Path::new(ANDROID).join(name), value) {
        warn!("{ANDROID}/{name} = {value}: {e}");
    }
}

/// Phone Wi-Fi power saving holds packets for up to ~2 s (J7: 1000 ms average ping), which
/// turns quick presses into late bursts. `iw` (Termux: `pkg install root-repo iw`) switches it
/// off while buzzrs runs.
const TERMUX: &str = "/data/data/com.termux/files/usr";

fn iw_path() -> Option<String> {
    [format!("{TERMUX}/bin/iw"), "/system/bin/iw".to_string(), "/system/xbin/iw".to_string()]
        .into_iter()
        .find(|p| Path::new(p).exists())
}

fn run_iw(iw: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(iw).args(args).env("LD_LIBRARY_PATH", format!("{TERMUX}/lib")).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn wifi_power_save(on: bool) {
    let Some(iw) = iw_path() else {
        if !on {
            warn!("Wi-Fi power saving stays on (presses may arrive late): in Termux run `pkg install root-repo iw`");
        }
        return;
    };
    if run_iw(&iw, &["dev", "wlan0", "set", "power_save", if on { "on" } else { "off" }]).is_some() {
        info!("Wi-Fi power saving {}", if on { "back on" } else { "off" });
    } else {
        warn!("could not change Wi-Fi power saving with {iw}");
    }
}

/// Fixes what Android undoes while buzzrs runs:
/// - it turns Wi-Fi power saving back on by itself (screen, USB or Wi-Fi events);
/// - Samsung's kernel takes a USB bus suspend for an unplugged cable and switches the gadget off
///   for good, so the host loses the dongle although the cable is still in.
fn watchdog() {
    let iw = iw_path();
    let mut lost = 0u32;
    while !SAVED.lock().unwrap().is_empty() {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if let Some(iw) = &iw {
            if run_iw(iw, &["dev", "wlan0", "get", "power_save"]).is_some_and(|s| s.contains(": on")) {
                debug!("Wi-Fi power saving came back on, switching it off");
                let _ = run_iw(iw, &["dev", "wlan0", "set", "power_save", "off"]);
            }
        }
        let cable = fs::read_to_string("/sys/class/power_supply/usb/online").is_ok_and(|s| s.trim() == "1");
        if cable && (attr("state") == "DISCONNECTED" || attr("enable") == "0") {
            lost += 1;
            // a charger without a host also looks like this: retry slowly
            if lost == 2 || lost.is_multiple_of(10) {
                if lost == 2 {
                    info!("USB host gone while the cable is in: switching the dongle on again");
                }
                set_attr("enable", "0");
                std::thread::sleep(std::time::Duration::from_millis(300));
                set_attr("enable", "1");
            }
        } else {
            lost = 0;
        }
    }
}

fn setprop(name: &str, value: &str) {
    let _ = Command::new("/system/bin/setprop").args([name, value]).status();
}

/// FunctionFS descriptors (v2 format): one HID interface with an interrupt IN endpoint, for full
/// and high speed. No OUT endpoint: the lights come as SET_REPORT on ep0, like the real dongle.
fn descriptors() -> Vec<u8> {
    let len = REPORT_DESCRIPTOR.len() as u8;
    let intf = [9, 4, 0, 0, 1, 3, 0, 0, 0];
    let hid = [9, 0x21, 0x11, 0x01, 0, 1, 0x22, len, 0];
    let mut fs_desc = Vec::new();
    fs_desc.extend(intf);
    fs_desc.extend(hid);
    fs_desc.extend([7, 5, 0x81, 3, 8, 0, 10]); // 10 ms
    let mut hs_desc = Vec::new();
    hs_desc.extend(intf);
    hs_desc.extend(hid);
    hs_desc.extend([7, 5, 0x81, 3, 8, 0, 7]); // 2^6 microframes = 8 ms
    let mut out = Vec::new();
    // magic v2, length, flags (has FS + HS), counts
    for v in [3u32, (20 + fs_desc.len() + hs_desc.len()) as u32, 3, 3, 3] {
        out.extend(v.to_le_bytes());
    }
    out.extend(fs_desc);
    out.extend(hs_desc);
    out
}

fn strings() -> Vec<u8> {
    // magic, length, no strings, no languages
    [2u32, 16, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn up(on_lights: impl Fn(Vec<u8>) + Send + 'static) -> Result<crate::gadget::Dongle> {
    if !available() {
        bail!("no FunctionFS USB gadget here ({FFS}, {ANDROID})");
    }
    {
        let mut saved = SAVED.lock().unwrap();
        if saved.is_empty() {
            *saved = SAVED_ATTRS.iter().map(|a| (*a, attr(a))).collect();
        }
    }
    wifi_power_save(false);
    info!("taking USB over from adb (it comes back when buzzrs stops, or after a reboot)");
    setprop("ctl.stop", "adbd");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    set_attr("enable", "0");
    let mut ep0 = fs::OpenOptions::new().read(true).write(true).open(Path::new(FFS).join("ep0")).context("opening FunctionFS ep0")?;
    ep0.write_all(&descriptors()).context("FunctionFS refused the HID descriptors")?;
    ep0.write_all(&strings()).context("FunctionFS strings")?;
    set_attr("idVendor", "054c");
    set_attr("idProduct", "0002");
    set_attr("bcdDevice", "05a1");
    set_attr("iManufacturer", "Logitech");
    set_attr("iProduct", "Logitech Buzz(tm) Controller V1");
    set_attr("functions", "ffs");
    set_attr("enable", "1");
    std::thread::spawn(watchdog);
    let ep1 = fs::OpenOptions::new().read(true).write(true).open(Path::new(FFS).join("ep1")).context("opening FunctionFS ep1")?;
    info!("USB: Buzz! dongle 054c:0002 (functions {})", attr("functions"));

    let (ltx, lrx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || control(ep0, ltx));
    Ok(crate::gadget::Dongle::start(ep1, lrx, on_lights))
}

fn set_blocking(f: &File, blocking: bool) {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: plain fcntl on a file descriptor we own
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        let flags = if blocking { flags & !libc::O_NONBLOCK } else { flags | libc::O_NONBLOCK };
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}

fn control(mut ep0: File, lights: mpsc::Sender<Vec<u8>>) {
    // A blocking read of ep0 sometimes misses the wake-up on Samsung's 3.18 kernel: the host's
    // requests then wait until the next disconnect and enumeration times out. Polling never
    // misses them; the answers are written in blocking mode.
    set_blocking(&ep0, false);
    let mut buf = [0u8; 12 * 8];
    let mut last_err = String::new();
    loop {
        let n = match ep0.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            Err(e) => {
                if e.to_string() != last_err {
                    debug!("FunctionFS ep0: {e}");
                    last_err = e.to_string();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        last_err.clear();
        set_blocking(&ep0, true);
        for ev in buf[..n].as_chunks::<12>().0 {
            let kind = ev[8];
            if kind != 4 {
                let name = ["bind", "unbind", "enable", "disable", "setup", "suspend", "resume"];
                debug!("USB event: {}", name.get(kind as usize).unwrap_or(&"?"));
                continue;
            }
            let (rt, rq) = (ev[0], ev[1]);
            let value = u16::from_le_bytes([ev[2], ev[3]]);
            let len = u16::from_le_bytes([ev[6], ev[7]]) as usize;
            let r = match (rt, rq) {
                // GET_DESCRIPTOR (HID report descriptor)
                (0x81, 6) if value >> 8 == 0x22 => ep0.write_all(&REPORT_DESCRIPTOR[..len.min(REPORT_DESCRIPTOR.len())]),
                // GET_REPORT: nothing pressed (the interrupt endpoint carries the real state)
                (0xa1, 1) => ep0.write_all(&[0x7f, 0x7f, 0, 0, 0xf0][..len.min(5)]),
                // SET_REPORT: the lights
                (0x21, 9) => {
                    let mut d = vec![0u8; len];
                    ep0.read_exact(&mut d).map(|_| {
                        let _ = lights.send(d);
                    })
                }
                // SET_IDLE, SET_PROTOCOL...: accept
                (0x21, _) => ep0.read(&mut []).map(|_| ()),
                // anything else: stall (a transfer in the wrong direction)
                (rt, _) if rt & 0x80 != 0 => ep0.read(&mut []).map(|_| ()),
                _ => ep0.write(&[]).map(|_| ()),
            };
            if let Err(e) = r {
                debug!("USB request {rt:02x}/{rq:02x}: {e}");
            }
        }
        set_blocking(&ep0, false);
    }
}

/// Gives USB back to adbd (MTP + adb as before). Call it just before the process exits: adbd can
/// only take FunctionFS once our endpoints are closed, so it is started a moment later from a
/// shell that outlives us.
pub fn restore() {
    let saved = std::mem::take(&mut *SAVED.lock().unwrap());
    if saved.is_empty() {
        return;
    }
    wifi_power_save(true);
    set_attr("enable", "0");
    for (a, v) in &saved {
        set_attr(a, v);
    }
    let script = format!(
        "(sleep 1; /system/bin/setprop ctl.start adbd; sleep 3; echo 1 > {ANDROID}/enable) </dev/null >/dev/null 2>&1 &"
    );
    match Command::new("/system/bin/sh").args(["-c", &script]).status() {
        Ok(_) => info!("USB goes back to adb in a few seconds"),
        Err(e) => warn!("could not restart adb ({e}): reboot the phone to get adb back"),
    }
}
