//! Colour space description, independent of ffmpeg's and libplacebo's enums.

use std::fmt;
use std::str::FromStr;

use crate::Error;

/// Defines a `Copy` enum with string names (`as_str`, `Display`, `FromStr`). Expects `fmt`,
/// `FromStr` and `Error` in scope at the call site.
#[doc(hidden)]
#[macro_export]
macro_rules! named_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name { $($(#[$vm])* $v),+ }

        impl $name {
            /// The ffmpeg/libplacebo option name (`bt709`, `pc`, ...).
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$v => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                    $($s => Ok(Self::$v),)+
                    _ => Err(Error::Invalid { what: stringify!($name), value: s.to_owned() }),
                }
            }
        }
    };
}
#[doc(hidden)]
pub use crate::named_enum;

named_enum!(
    /// Colour primaries.
    Primaries {
        Bt709 = "bt709",
        Bt2020 = "bt2020",
        /// Display P3 (iPhone Live Photos).
        DisplayP3 = "smpte432",
        DciP3 = "smpte431",
        Bt601 = "smpte170m",
        Unknown = "unknown",
    }
);

named_enum!(
    /// Transfer characteristic.
    Transfer {
        Bt709 = "bt709",
        Srgb = "iec61966-2-1",
        Pq = "smpte2084",
        Hlg = "arib-std-b67",
        Linear = "linear",
        Unknown = "unknown",
    }
);

named_enum!(
    /// YCbCr matrix (`Rgb` for RGB formats).
    Matrix {
        Bt709 = "bt709",
        Bt2020Ncl = "bt2020nc",
        Bt601 = "smpte170m",
        Rgb = "gbr",
        Unknown = "unknown",
    }
);

named_enum!(
    /// Signal range.
    Range {
        /// Limited / tv / mpeg (16-235).
        Limited = "tv",
        /// Full / pc / jpeg (0-255).
        Full = "pc",
        Unknown = "unknown",
    }
);

/// A complete colour description, as tagged on a stream or requested for an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorSpace {
    pub primaries: Primaries,
    pub transfer: Transfer,
    pub matrix: Matrix,
    pub range: Range,
}

impl ColorSpace {
    /// Rec.709, limited range YCbCr: what NVENC gets (the prototype's `PL_ENC`).
    pub const BT709_TV: Self = Self {
        primaries: Primaries::Bt709,
        transfer: Transfer::Bt709,
        matrix: Matrix::Bt709,
        range: Range::Limited,
    };

    /// Rec.709, full-range RGB: intermediate/CPU stages (the prototype's `PL_RGB`).
    pub const BT709_RGB: Self = Self {
        primaries: Primaries::Bt709,
        transfer: Transfer::Bt709,
        matrix: Matrix::Rgb,
        range: Range::Full,
    };

    pub const UNKNOWN: Self = Self {
        primaries: Primaries::Unknown,
        transfer: Transfer::Unknown,
        matrix: Matrix::Unknown,
        range: Range::Unknown,
    };

    /// PQ or HLG: needs tone mapping to reach an SDR output.
    pub fn is_hdr(&self) -> bool {
        matches!(self.transfer, Transfer::Pq | Transfer::Hlg)
    }
}

impl Default for ColorSpace {
    fn default() -> Self {
        Self::BT709_TV
    }
}

impl fmt::Display for ColorSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}/{}",
            self.matrix, self.primaries, self.transfer, self.range
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!("arib-std-b67".parse::<Transfer>().unwrap(), Transfer::Hlg);
        assert_eq!(Range::Full.to_string(), "pc");
        assert!("nope".parse::<Primaries>().is_err());
        assert_eq!(ColorSpace::BT709_TV.to_string(), "bt709/bt709/bt709/tv");
    }

    #[test]
    fn hdr() {
        let hlg = ColorSpace {
            transfer: Transfer::Hlg,
            ..ColorSpace::BT709_TV
        };
        assert!(hlg.is_hdr());
        assert!(!ColorSpace::BT709_RGB.is_hdr());
    }
}
