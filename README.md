# buzzrs

Phones as buzzers for the **Buzz!** quiz games on PS3 and RPCS3.

buzzrs shows a buzzer controller in the phone's browser (no app to install) and turns the
presses into an emulated **USB Buzz! dongle**. The game sees a real "Logitech Buzz(tm)
Controller V1" (`054c:0002`) with 4 wireless buzzers, red lights included.


- **No hardware buzzers needed:** every phone on the same Wi-Fi is one buzzer. Scan a QR code
  and you are in.
- **Real USB device:** works with the unmodified games on a real PS3, and in RPCS3 through USB
  passthrough.
- **Up to 8 players:** two dongles, for the Buzz! games that support a second set of buzzers.

## Where it runs

buzzrs emulates the dongle with the device-mode side of USB, so the machine that runs it has to
be able to act as a USB device:

| Setup | Dongle made with | Plugged into | Players |
|---|---|---|---|
| Linux PC | `dummy_hcd` (a virtual USB cable inside the PC) | RPCS3 on the same PC | 8 |
| Linux board with a USB device port (Raspberry Pi Zero 2 W / 4 / 5, Radxa, Orange Pi…) | configfs HID gadget | PS3 | 4 per board |
| Rooted Android phone (Magisk) | FunctionFS (no custom kernel needed) | PS3 or a PC with RPCS3 | 4 per phone |

Tested with RPCS3 on Linux and with a Samsung Galaxy J7 2017 (SM-J730F, Android 9, Magisk)
plugged into a PS3.

## Build

```
cargo build --release
```

For an Android phone, build one static binary. No NDK is needed, because `.cargo/config.toml`
links with `rust-lld`:

```
rustup target add armv7-unknown-linux-musleabihf
cargo build --release --target armv7-unknown-linux-musleabihf   # 32-bit Android (uname -m: armv7l / armv8l)

rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl       # 64-bit Android (aarch64)
```

## Linux PC + RPCS3

Run `target/release/buzzrs` without arguments, for example by double-clicking it:

1. If the dongles are not set up yet, the system asks for your password once (pkexec) and buzzrs
   creates two of them through `dummy_hcd`.
2. The **host page** opens in the browser. It shows the QR code and address for the phones, the
   state of the dongles, every buzzer live (phone connected, buttons, lights) and a Quit button.
3. Start a Buzz! game and open the address on the phones (same Wi-Fi).

The same steps by hand:

```
sudo ./target/release/buzzrs gadget-up      # two dongles; --sets 1 for one
./target/release/buzzrs serve               # serve the phones (as your user)
sudo ./target/release/buzzrs gadget-down    # remove the dongles
```

`gadget-up` loads `libcomposite`, `usb_f_hid` and `dummy_hcd`, creates the gadgets, gives
`/dev/hidg*` and the new USB devices to your user, and frees them from the kernel's HID driver.
`lsusb` then lists `054c:0002` once per dongle.

## Linux board + PS3

On a board with a USB device-mode port (Raspberry Pi: `dtoverlay=dwc2` in `config.txt`,
`modules-load=dwc2` in `cmdline.txt`), run the same commands. `gadget-up` picks the board's real
device controller instead of `dummy_hcd`. Plug the board's device port into the PS3; the phones
connect to the board over Wi-Fi.

A board has one device port, so one board is one set (4 players).

## Rooted Android phone + PS3

The phone itself becomes the dongle: plugged into the PS3 by its USB cable, it shows up as
`054c:0002`, and it also serves the controller page to the other phones.

1. Copy the static binary (see [Build](#build)) into Termux's home directory.
2. In Termux:
   ```
   pkg install root-repo iw    # once: lets buzzrs turn off Wi-Fi power saving
   chmod +x buzzrs
   termux-wake-lock            # keeps Android from suspending Termux
   su -c ./buzzrs
   ```
   The host page opens on the phone; the players scan its QR code.
3. Plug the phone into the PS3 and start the game.
4. Stop with the Quit button or Ctrl+C. adb and MTP come back a few seconds later; a reboot also
   restores everything.

Older Android kernels (for example Samsung's 3.18) have no configfs HID function. buzzrs uses the
FunctionFS instance that adbd normally owns (`/dev/usb-ffs/adb`): it stops adbd, describes a HID
interface with the Buzz! report descriptor, switches the gadget to `054c:0002` and answers the
host's requests itself. While it runs, adb over USB is not available.

To use the phone-dongle with RPCS3 on a Linux PC, allow your user to open it:

```
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="054c", ATTR{idProduct}=="0002", MODE="0666"' | sudo tee /etc/udev/rules.d/51-buzz.rules
sudo udevadm control --reload
```

## Playing

- **Controller page** (`http://<address>:8000/`): each phone takes the first free buzzer and shows
  its number at the top. `?p=3` in the address asks for buzzer 3, and a refreshed page keeps its
  buzzer. Multi-touch works (hold one colour and press another). On a PC, Space is the buzzer
  and 1–4 are blue, orange, green and yellow.
- **Host page** (`/host`): the QR code, the dongles and every buzzer live.
- **Test page** (`/test`): all buzzers in one browser window, to try the game without phones.

With two dongles on one machine, buzzers 1–4 are the first set and 5–8 the second. With two
separate dongles (for example a PC dongle and a phone-dongle, or two phones), each one runs its
own server: start the second one with `--set 2` (`su -c "./buzzrs --set 2"` on a phone) so its
buzzers are numbered 5–8. The game decides which dongle is its first set, so if the numbers come
out the other way round, swap the `--set` values.

## Troubleshooting

**RPCS3 does not see the buzzers.** Check that *Buzz emulated controller* is set to **Null**. If
the log shows `libusb couldn't open USB device … errno=13`, your user cannot open the USB device:
for the PC dongles run `gadget-up` again, for a phone add the udev rule above.

**RPCS3 sees only one of two dongles.** RPCS3 tells USB devices apart by port and address only,
not by bus, and every `dummy_hcd` bus puts its device on port 1, address 2. `gadget-up` detects
the clash and plugs the second dongle in again until it gets another address.

**Presses arrive late or get lost.** This is phone Wi-Fi power saving: a dozing phone holds
packets for up to about two seconds. On the dongle phone, buzzrs turns power saving off with `iw`
and keeps it off. On the players' phones, the controller page sends a small packet every 250 ms
to keep the Wi-Fi awake, and the ping shown in the corner is the median of the last ones. If a
player's phone still lags, turn off its battery saver or Wi-Fi power saving mode.

**The double-clicked app does not start.** buzzrs uses port 8000, or the next free one if
something else has it. Double-clicking while it runs just opens its page. Errors show as a desktop
notification.

**A phone-dongle disappears from the host.** Samsung's kernel takes a USB bus suspend for an
unplugged cable and switches the gadget off. buzzrs notices that the cable is still in and
switches the dongle on again within a couple of seconds.

**A phone lost adb.** buzzrs gives USB back when it stops. If it was killed hard, reboot the phone.

## How it works

The dongle (`src/report.rs`) has the same layout as RPCS3's `usb_device_buzz`:

- **Input report** (5 bytes, interrupt IN): two axes fixed at `0x7f`, then 20 button bits. Buzzer
  *i* uses bits `5i … 5i+4`, in the order red, yellow, green, orange, blue. The last 4 bits are
  vendor bits and are always set (`0xf0`).
- **Output report** (7 bytes, SET_REPORT on the control endpoint): bytes 1–4 are the four red
  lights.

The games refresh the lights by sending "all off" and, a few milliseconds later, the real state.
buzzrs ignores an "off" that is followed by another report within 40 ms, so the lights on the
phones do not flicker.

Backends:

- `gadget` (`src/gadget.rs`): Linux configfs with the `hid` function and `no_out_endpoint`.
- `ffs` (`src/ffs.rs`): Android FunctionFS. The control endpoint is polled instead of blocked on,
  because a blocking read on Samsung's 3.18 kernel sometimes misses the wake-up and the host's
  requests stall.
- `none`: only logs the reports, to try the pages.

## Command line

```
buzzrs [--set N]                                 app mode: set up, serve, open the host page
sudo buzzrs gadget-up [--sets N] [--owner USER]  create N Linux gadget dongles (default 2)
sudo buzzrs gadget-down                          remove them
buzzrs serve [--port 8000] [--backend gadget|ffs|none] [--set N] [--open]
```

`RUST_LOG=debug` logs every light change and USB event.

## License

MIT, see [LICENSE](LICENSE).

Buzz! is a trademark of Sony Interactive Entertainment. This project is not affiliated with Sony
or Logitech.
