//! Platform-neutral Bluetooth device address.
//!
//! BlueR's `Address` type is only available on Linux, so the rest of the
//! application uses this type instead. The bytes are stored in display order
//! (most significant byte first), exactly like `bluer::Address`, so the string
//! representation (`AA:BB:CC:DD:EE:FF`) is identical on every platform. That
//! matters because addresses are used as keys in `devices.json`.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub [u8; 6]);

impl Address {
    /// Bytes in over-the-air order (least significant byte first), as used by
    /// `bdaddr_t` on both BlueZ and the FreeBSD netgraph stack.
    #[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
    pub fn to_le_bytes(self) -> [u8; 6] {
        let mut b = self.0;
        b.reverse();
        b
    }

    #[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
    pub fn from_le_bytes(mut bytes: [u8; 6]) -> Self {
        bytes.reverse();
        Self(bytes)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAddress(pub String);

impl fmt::Display for InvalidAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Bluetooth address: {}", self.0)
    }
}

impl std::error::Error for InvalidAddress {}

impl FromStr for Address {
    type Err = InvalidAddress;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; 6];
        let mut parts = s.split(':');
        for byte in out.iter_mut() {
            let part = parts.next().ok_or_else(|| InvalidAddress(s.to_string()))?;
            if part.len() != 2 {
                return Err(InvalidAddress(s.to_string()));
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| InvalidAddress(s.to_string()))?;
        }
        if parts.next().is_some() {
            return Err(InvalidAddress(s.to_string()));
        }
        Ok(Self(out))
    }
}

#[cfg(target_os = "linux")]
impl From<bluer::Address> for Address {
    fn from(a: bluer::Address) -> Self {
        Self(a.0)
    }
}

#[cfg(target_os = "linux")]
impl From<Address> for bluer::Address {
    fn from(a: Address) -> Self {
        bluer::Address(a.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_string() {
        let a: Address = "a1:B2:c3:04:05:FF".parse().unwrap();
        assert_eq!(a.0, [0xA1, 0xB2, 0xC3, 0x04, 0x05, 0xFF]);
        assert_eq!(a.to_string(), "A1:B2:C3:04:05:FF");
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "A1:B2:C3:04:05", "A1:B2:C3:04:05:FF:00", "A1:B2:C3:04:05:G0", "A1B2:C3:04:05:FF:0"] {
            assert!(s.parse::<Address>().is_err(), "{s} should not parse");
        }
    }

    #[test]
    fn le_bytes_are_reversed() {
        let a = Address([1, 2, 3, 4, 5, 6]);
        assert_eq!(a.to_le_bytes(), [6, 5, 4, 3, 2, 1]);
        assert_eq!(Address::from_le_bytes([6, 5, 4, 3, 2, 1]), a);
    }
}
