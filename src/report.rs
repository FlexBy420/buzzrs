//! The Buzz! USB dongle (Logitech Buzz(tm) Controller V1, 054c:0002) as the PS3 sees it.
//!
//! One dongle serves 4 buzzers. Input report (5 bytes, interrupt IN every 10 ms):
//!   byte 0, 1: two 8-bit axes, always 0x7f
//!   then 20 button bits from byte 2 bit 0: buzzer i (0..3) uses bits 5i .. 5i+4 =
//!   red, yellow, green, orange, blue
//!   the last 4 bits (byte 4 high nibble) are vendor bits, set (0xf0)
//! Output report (SET_REPORT, 7 bytes): byte 1..4 = the 4 red lights, 0xff on, 0x00 off.
//! Same layout as RPCS3's usb_device_buzz (rpcs3/Emu/Io/Buzz.cpp).

/// HID report descriptor of the real controller (78 bytes, wDescriptorLength 0x004e).
pub const REPORT_DESCRIPTOR: [u8; 78] = [
    0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0xa1, 0x02, 0x75, 0x08, 0x95, 0x02, 0x15, 0x00, 0x26, 0xff, 0x00, 0x35, 0x00,
    0x46, 0xff, 0x00, 0x09, 0x30, 0x09, 0x31, 0x81, 0x02, 0x75, 0x01, 0x95, 0x14, 0x25, 0x01, 0x45, 0x01, 0x05, 0x09,
    0x19, 0x01, 0x29, 0x14, 0x81, 0x02, 0x06, 0x00, 0xff, 0x75, 0x01, 0x95, 0x04, 0x25, 0x01, 0x45, 0x01, 0x09, 0x01,
    0x81, 0x02, 0xc0, 0xa1, 0x02, 0x75, 0x08, 0x95, 0x07, 0x46, 0xff, 0x00, 0x26, 0xff, 0x00, 0x09, 0x02, 0x91, 0x02,
    0xc0, 0xc0,
];

pub const PLAYERS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Red = 0,
    Yellow = 1,
    Green = 2,
    Orange = 3,
    Blue = 4,
}

impl Button {
    pub fn from_name(s: &str) -> Option<Button> {
        Some(match s {
            "red" => Button::Red,
            "yellow" => Button::Yellow,
            "green" => Button::Green,
            "orange" => Button::Orange,
            "blue" => Button::Blue,
            _ => return None,
        })
    }
}

/// Pressed buttons of the 4 buzzers: 5 bits each (bit = Button as u8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buttons(pub [u8; PLAYERS]);

impl Buttons {
    pub fn set(&mut self, player: usize, b: Button, down: bool) {
        if player < PLAYERS {
            if down {
                self.0[player] |= 1 << b as u8;
            } else {
                self.0[player] &= !(1 << b as u8);
            }
        }
    }

    pub fn clear(&mut self, player: usize) {
        if player < PLAYERS {
            self.0[player] = 0;
        }
    }

    pub fn report(&self) -> [u8; 5] {
        let mut bits: u32 = 0;
        for (i, b) in self.0.iter().enumerate() {
            bits |= ((*b & 0x1f) as u32) << (5 * i);
        }
        bits |= 0xf << 20;
        [0x7f, 0x7f, bits as u8, (bits >> 8) as u8, (bits >> 16) as u8]
    }
}

pub fn lights(report: &[u8]) -> Option<[bool; PLAYERS]> {
    if report.len() < 5 {
        return None;
    }
    Some([report[1] != 0, report[2] != 0, report[3] != 0, report[4] != 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RPCS3: buf[2 + (k + 5 * index) / 8] |= 1 << ((k + 5 * index) % 8)
    fn rpcs3(pressed: &[(usize, u8)]) -> [u8; 5] {
        let mut buf = [0x7f, 0x7f, 0, 0, 0xf0];
        for &(index, k) in pressed {
            let n = k as usize + 5 * index;
            buf[2 + n / 8] |= 1 << (n % 8);
        }
        buf
    }

    #[test]
    fn same_as_rpcs3() {
        assert_eq!(Buttons::default().report(), [0x7f, 0x7f, 0, 0, 0xf0]);
        let all = [Button::Red, Button::Yellow, Button::Green, Button::Orange, Button::Blue];
        for p in 0..PLAYERS {
            for b in all {
                let mut s = Buttons::default();
                s.set(p, b, true);
                assert_eq!(s.report(), rpcs3(&[(p, b as u8)]), "player {p} {b:?}");
            }
        }
        let mut s = Buttons::default();
        s.set(0, Button::Red, true);
        s.set(3, Button::Blue, true);
        s.set(2, Button::Orange, true);
        assert_eq!(s.report(), rpcs3(&[(0, 0), (3, 4), (2, 3)]));
        s.set(3, Button::Blue, false);
        assert_eq!(s.report(), rpcs3(&[(0, 0), (2, 3)]));
        assert_eq!(REPORT_DESCRIPTOR.len(), 0x4e);
        assert_eq!(lights(&[0, 0xff, 0, 0, 0xff, 0, 0]), Some([true, false, false, true]));
    }
}
