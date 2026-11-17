//! HID usage (Oink [`KeyCode`]) ↔ Linux evdev keycodes used by libei / portal.

use oink_foundation::input::KeyCode;

/// Evdev keycode for a keyboard or media HID usage, when supported.
pub(crate) fn hid_to_evdev(code: KeyCode) -> Option<u32> {
    match code.page() {
        KeyCode::PAGE_KEYBOARD => keyboard_to_evdev(code.usage()),
        KeyCode::PAGE_CONSUMER => consumer_to_evdev(code.usage()),
        _ => None,
    }
}

fn keyboard_to_evdev(usage: u16) -> Option<u32> {
    Some(match usage {
        0x04 => 30,  // A
        0x05 => 48,  // B
        0x06 => 46,  // C
        0x07 => 32,  // D
        0x08 => 18,  // E
        0x09 => 33,  // F
        0x0A => 34,  // G
        0x0B => 35,  // H
        0x0C => 23,  // I
        0x0D => 36,  // J
        0x0E => 37,  // K
        0x0F => 38,  // L
        0x10 => 50,  // M
        0x11 => 49,  // N
        0x12 => 24,  // O
        0x13 => 25,  // P
        0x14 => 16,  // Q
        0x15 => 19,  // R
        0x16 => 31,  // S
        0x17 => 20,  // T
        0x18 => 22,  // U
        0x19 => 47,  // V
        0x1A => 17,  // W
        0x1B => 45,  // X
        0x1C => 21,  // Y
        0x1D => 44,  // Z
        0x1E => 2,   // 1
        0x1F => 3,   // 2
        0x20 => 4,   // 3
        0x21 => 5,   // 4
        0x22 => 6,   // 5
        0x23 => 7,   // 6
        0x24 => 8,   // 7
        0x25 => 9,   // 8
        0x26 => 10,  // 9
        0x27 => 11,  // 0
        0x28 => 28,  // Enter
        0x29 => 1,   // Esc
        0x2A => 14,  // Backspace
        0x2B => 15,  // Tab
        0x2C => 57,  // Space
        0x2D => 12,  // -
        0x2E => 13,  // =
        0x2F => 26,  // [
        0x30 => 27,  // ]
        0x31 => 43,  // \
        0x32 => 43,  // Non-US # and ~
        0x33 => 39,  // ;
        0x34 => 40,  // '
        0x35 => 41,  // `
        0x36 => 51,  // ,
        0x37 => 52,  // .
        0x38 => 53,  // /
        0x39 => 58,  // CapsLock
        0x3A => 59,  // F1
        0x3B => 60,  // F2
        0x3C => 61,  // F3
        0x3D => 62,  // F4
        0x3E => 63,  // F5
        0x3F => 64,  // F6
        0x40 => 65,  // F7
        0x41 => 66,  // F8
        0x42 => 67,  // F9
        0x43 => 68,  // F10
        0x44 => 87,  // F11
        0x45 => 88,  // F12
        0x46 => 99,  // PrintScreen
        0x47 => 70,  // ScrollLock
        0x48 => 119, // Pause
        0x49 => 110, // Insert
        0x4A => 102, // Home
        0x4B => 104, // PageUp
        0x4C => 111, // Delete
        0x4D => 107, // End
        0x4E => 109, // PageDown
        0x4F => 106, // Right
        0x50 => 105, // Left
        0x51 => 108, // Down
        0x52 => 103, // Up
        0x53 => 69,  // NumLock
        0x54 => 98,  // KP /
        0x55 => 55,  // KP *
        0x56 => 74,  // KP -
        0x57 => 78,  // KP +
        0x58 => 96,  // KP Enter
        0x59 => 79,  // KP 1
        0x5A => 80,  // KP 2
        0x5B => 81,  // KP 3
        0x5C => 75,  // KP 4
        0x5D => 76,  // KP 5
        0x5E => 77,  // KP 6
        0x5F => 71,  // KP 7
        0x60 => 72,  // KP 8
        0x61 => 73,  // KP 9
        0x62 => 82,  // KP 0
        0x63 => 83,  // KP .
        0x64 => 86,  // Non-US \ (ISO 102nd)
        0x65 => 127, // Menu / Compose
        0x67 => 117, // KP =
        0x68 => 183, // F13
        0x69 => 184, // F14
        0x6A => 185, // F15
        0x6B => 186, // F16
        0x6C => 187, // F17
        0x6D => 188, // F18
        0x6E => 189, // F19
        0x6F => 190, // F20
        0x70 => 191, // F21
        0x71 => 192, // F22
        0x72 => 193, // F23
        0x73 => 194, // F24
        0x7F => 113, // Mute
        0x80 => 115, // VolumeUp
        0x81 => 114, // VolumeDown
        0xE0 => 29,  // LeftCtrl
        0xE1 => 42,  // LeftShift
        0xE2 => 56,  // LeftAlt
        0xE3 => 125, // LeftMeta
        0xE4 => 97,  // RightCtrl
        0xE5 => 54,  // RightShift
        0xE6 => 100, // RightAlt
        0xE7 => 126, // RightMeta
        _ => return None,
    })
}

fn consumer_to_evdev(usage: u16) -> Option<u32> {
    Some(match usage {
        0xB3 => 208, // Fast Forward
        0xB4 => 168, // Rewind
        0xB5 => 163, // Scan Next Track
        0xB6 => 165, // Scan Previous Track
        0xB7 => 166, // Stop
        0xCD => 164, // Play / Pause
        0xE2 => 113, // Mute
        0xE9 => 115, // Volume Increment
        0xEA => 114, // Volume Decrement
        _ => return None,
    })
}

#[allow(dead_code)]
pub(crate) fn evdev_to_hid(evdev: u32) -> Option<KeyCode> {
    // Emit Consumer-page usages for media/volume keys to match macOS/Windows capture.
    match evdev {
        113 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xE2)),
        114 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xEA)),
        115 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xE9)),
        163 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xB5)),
        164 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xCD)),
        165 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xB6)),
        166 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xB7)),
        168 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xB4)),
        208 => return Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xB3)),
        _ => {}
    }
    for usage in 0x04u16..=0xE7 {
        if matches!(usage, 0x32 | 0x7F..=0x81) {
            continue;
        }
        let code = KeyCode::keyboard(usage);
        if hid_to_evdev(code) == Some(evdev) {
            return Some(code);
        }
    }
    None
}

#[allow(dead_code)]
pub(crate) fn supported(code: KeyCode) -> bool {
    hid_to_evdev(code).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_canonical_usage_round_trips_through_evdev() {
        for usage in 0x04u16..=0xE7 {
            if matches!(usage, 0x32 | 0x7F..=0x81) {
                continue;
            }
            let code = KeyCode::keyboard(usage);
            if let Some(evdev) = hid_to_evdev(code) {
                assert_eq!(evdev_to_hid(evdev), Some(code), "usage {usage:#04x}");
            }
        }
        for usage in [0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xCD, 0xE2, 0xE9, 0xEA] {
            let code = KeyCode::new(KeyCode::PAGE_CONSUMER, usage);
            let evdev = hid_to_evdev(code).expect("consumer usage mapped");
            assert_eq!(evdev_to_hid(evdev), Some(code), "consumer {usage:#04x}");
        }
    }
}
