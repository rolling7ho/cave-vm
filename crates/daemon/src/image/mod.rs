//! Base images: names, digests and the shared cache (ROADMAP P3.12).
//!
//! A project names its image in `cave.toml` as `base`, `debian` or
//! `ubuntu`, and on first use the name is pinned to a digest, written as
//! `debian@sha256:<hex>` ([D22]). From then on the digest is what counts:
//! the cache stores images by digest, and a download whose bytes do not hash
//! to it is thrown away.
//!
//! [D22]: ../../../ROADMAP.md#decisions

use std::fmt;
use std::str::FromStr;

mod cache;

pub use cache::{Cache, FetchError, Source};

/// The name `base` stands for.
pub const BASE_ALIAS_TARGET: &str = "debian";

/// A SHA-256 digest, written `sha256:<64 lowercase hex digits>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn from_bytes(bytes: [u8; 32]) -> Digest {
        Digest(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The 64 hex digits, without the `sha256:` prefix.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The first 8 hex digits, as `cave images` shows them.
    pub fn short(&self) -> String {
        self.hex()[..8].to_string()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex())
    }
}

impl FromStr for Digest {
    type Err = RefError;

    fn from_str(s: &str) -> Result<Digest, RefError> {
        let bad = || RefError::Digest(s.to_string());
        let hex = s.strip_prefix("sha256:").ok_or_else(bad)?;
        if hex.len() != 64 {
            return Err(bad());
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let pair = &hex[2 * i..2 * i + 2];
            if pair
                .bytes()
                .any(|c| !matches!(c, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(bad());
            }
            *byte = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Digest(bytes))
    }
}

/// The `image` value in `cave.toml`: a name, optionally pinned to a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// As written, so `base` stays `base` when the pin is added.
    pub name: String,
    pub digest: Option<Digest>,
}

impl ImageRef {
    /// The image the name stands for: `base` is the recommended default.
    pub fn resolved_name(&self) -> &str {
        if self.name == "base" {
            BASE_ALIAS_TARGET
        } else {
            &self.name
        }
    }

    /// The same name pinned to `digest`.
    pub fn pinned(&self, digest: Digest) -> ImageRef {
        ImageRef {
            name: self.name.clone(),
            digest: Some(digest),
        }
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

impl FromStr for ImageRef {
    type Err = RefError;

    fn from_str(s: &str) -> Result<ImageRef, RefError> {
        let (name, digest) = match s.split_once('@') {
            Some((name, digest)) => (name, Some(digest.parse()?)),
            None => (s, None),
        };
        let valid = !name.is_empty()
            && name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if !valid {
            return Err(RefError::Name(name.to_string()));
        }
        Ok(ImageRef {
            name: name.to_string(),
            digest,
        })
    }
}

/// Why an `image` value could not be read. The messages say how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    Name(String),
    Digest(String),
}

impl fmt::Display for RefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefError::Name(name) => write!(
                f,
                "\"{name}\" is not an image name\nUse a name from cave images, such as base, debian or ubuntu"
            ),
            RefError::Digest(digest) => write!(
                f,
                "\"{digest}\" is not an image digest\nA pinned image looks like debian@sha256: followed by 64 lowercase hex digits; remove the @ part to pin again"
            ),
        }
    }
}

impl std::error::Error for RefError {}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "3f2a9c1e00000000000000000000000000000000000000000000000000000001";

    #[test]
    fn names_and_pinned_names_parse_and_print_back() {
        for s in ["base", "debian", "ubuntu", "my-image-2"] {
            let r: ImageRef = s.parse().unwrap();
            assert_eq!(r.digest, None);
            assert_eq!(r.to_string(), s);
        }
        let pinned: ImageRef = format!("base@sha256:{HEX}").parse().unwrap();
        assert_eq!(pinned.name, "base");
        assert_eq!(pinned.digest.unwrap().hex(), HEX);
        assert_eq!(pinned.to_string(), format!("base@sha256:{HEX}"));
        assert_eq!(pinned.digest.unwrap().short(), "3f2a9c1e");
    }

    #[test]
    fn base_stands_for_debian_and_keeps_its_name_when_pinned() {
        let base: ImageRef = "base".parse().unwrap();
        assert_eq!(base.resolved_name(), "debian");
        let digest: Digest = format!("sha256:{HEX}").parse().unwrap();
        assert_eq!(
            base.pinned(digest).to_string(),
            format!("base@sha256:{HEX}")
        );
        assert_eq!(
            "ubuntu".parse::<ImageRef>().unwrap().resolved_name(),
            "ubuntu"
        );
    }

    #[test]
    fn bad_names_and_digests_say_how_to_fix_them() {
        for bad in [
            "",
            "Debian",
            "deb ian",
            "-x",
            "1abc",
            "a/b",
            "debian:latest",
        ] {
            let err = bad.parse::<ImageRef>().unwrap_err();
            assert!(matches!(err, RefError::Name(_)), "{bad:?}");
            assert!(err.to_string().contains("cave images"));
        }
        for bad in [
            "debian@",
            "debian@sha256:",
            "debian@sha512:00",
            &format!("debian@sha256:{}", &HEX[1..]),
            &format!("debian@sha256:{}", HEX.to_uppercase()),
            &format!("debian@sha256:{HEX}0"),
        ] {
            let err = bad.parse::<ImageRef>().unwrap_err();
            assert!(matches!(err, RefError::Digest(_)), "{bad:?}");
            assert!(err.to_string().contains("pin again"));
        }
    }
}
