//! Loading an arm64 Linux `Image` for direct boot.
//!
//! Follows the kernel's `Documentation/arch/arm64/booting.rst`: the image
//! goes at a 2 MiB-aligned base plus `text_offset`, the device tree blob at
//! an 8-byte-aligned address, and the CPU starts at the image's first byte
//! with `x0` holding the device tree address.

use std::fmt;

/// Size of the arm64 `Image` header.
const HEADER_SIZE: usize = 64;
/// `ARM\x64` at offset 0x38.
const MAGIC: u32 = 0x644d_5241;
/// The image base must be 2 MiB aligned.
const IMAGE_ALIGN: u64 = 2 << 20;
/// The device tree is at most 2 MiB and placed in its own aligned block.
const DTB_MAX: u64 = 2 << 20;

/// An error loading the kernel.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The file is shorter than the header.
    TooShort,
    /// The header lacks the arm64 magic number. The file is probably not an
    /// uncompressed arm64 `Image` (a `vmlinuz` or `Image.gz` must be
    /// decompressed first).
    BadMagic,
    /// Kernel, initramfs and device tree do not fit in guest RAM.
    TooBig { needed: u64, available: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => f.write_str("kernel file is too short to be an arm64 Image"),
            Error::BadMagic => f.write_str(
                "kernel is not an uncompressed arm64 Image; decompress Image.gz or vmlinuz first",
            ),
            Error::TooBig { needed, available } => write!(
                f,
                "kernel, initramfs and device tree need {needed} bytes but the VM has {available}; give it more memory"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Fields read from the arm64 `Image` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    /// Offset from the 2 MiB-aligned base where the image must be loaded.
    pub text_offset: u64,
    /// Memory the image needs, including its BSS. Zero on very old kernels.
    pub image_size: u64,
}

impl ImageHeader {
    /// Parses the header at the start of `image`.
    pub fn parse(image: &[u8]) -> Result<ImageHeader, Error> {
        if image.len() < HEADER_SIZE {
            return Err(Error::TooShort);
        }
        let le64 = |at: usize| u64::from_le_bytes(image[at..at + 8].try_into().unwrap());
        let magic = u32::from_le_bytes(image[0x38..0x3c].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::BadMagic);
        }
        Ok(ImageHeader {
            text_offset: le64(0x08),
            image_size: le64(0x10),
        })
    }
}

/// Where everything goes in guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootLayout {
    /// Guest physical address of the kernel's first byte, and the entry point.
    pub kernel: u64,
    /// Guest physical start of the initramfs, if any.
    pub initrd: Option<u64>,
    /// Guest physical address of the device tree blob.
    pub dtb: u64,
}

/// Places the kernel at the start of RAM, the device tree in the last 2 MiB
/// and the initramfs just below it.
pub fn plan(
    header: &ImageHeader,
    kernel_len: u64,
    initrd_len: Option<u64>,
    ram_base: u64,
    ram_size: u64,
) -> Result<BootLayout, Error> {
    let kernel = ram_base.next_multiple_of(IMAGE_ALIGN) + header.text_offset;
    let kernel_end = kernel + header.image_size.max(kernel_len);
    let ram_end = ram_base + ram_size;
    let too_big = |needed: u64| Error::TooBig {
        needed,
        available: ram_size,
    };

    let dtb = ram_end
        .checked_sub(DTB_MAX)
        .filter(|d| *d >= kernel_end)
        .ok_or_else(|| too_big(kernel_end - ram_base + DTB_MAX))?;
    let initrd = match initrd_len {
        None => None,
        Some(len) => {
            let start = dtb.checked_sub(len).map(|s| s & !(IMAGE_ALIGN - 1));
            match start {
                Some(s) if s >= kernel_end => Some(s),
                _ => return Err(too_big(kernel_end - ram_base + len + DTB_MAX)),
            }
        }
    };
    Ok(BootLayout {
        kernel,
        initrd,
        dtb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const RAM: u64 = 0x4000_0000;

    fn image(text_offset: u64, image_size: u64) -> Vec<u8> {
        let mut h = vec![0u8; 4096];
        h[0x08..0x10].copy_from_slice(&text_offset.to_le_bytes());
        h[0x10..0x18].copy_from_slice(&image_size.to_le_bytes());
        h[0x38..0x3c].copy_from_slice(&MAGIC.to_le_bytes());
        h
    }

    #[test]
    fn parses_header() {
        let h = ImageHeader::parse(&image(0, 30 * MIB)).unwrap();
        assert_eq!(
            h,
            ImageHeader {
                text_offset: 0,
                image_size: 30 * MIB
            }
        );
    }

    #[test]
    fn rejects_compressed_or_short_files() {
        assert_eq!(
            ImageHeader::parse(&[0x1f, 0x8b, 0, 0]),
            Err(Error::TooShort)
        );
        assert_eq!(ImageHeader::parse(&[0u8; 64]), Err(Error::BadMagic));
    }

    #[test]
    fn layout_puts_kernel_first_and_dtb_last() {
        let h = ImageHeader {
            text_offset: 0,
            image_size: 40 * MIB,
        };
        let l = plan(&h, 30 * MIB, Some(5 * MIB), RAM, 512 * MIB).unwrap();
        assert_eq!(l.kernel, RAM);
        assert_eq!(l.dtb, RAM + 510 * MIB);
        let initrd = l.initrd.unwrap();
        assert_eq!(initrd % (2 * MIB), 0);
        assert!(initrd + 5 * MIB <= l.dtb);
        assert!(initrd >= RAM + 40 * MIB, "initrd clears the kernel's BSS");
    }

    #[test]
    fn honours_text_offset() {
        let h = ImageHeader {
            text_offset: 0x8_0000,
            image_size: 0,
        };
        assert_eq!(
            plan(&h, MIB, None, RAM, 64 * MIB).unwrap().kernel,
            RAM + 0x8_0000
        );
    }

    #[test]
    fn too_little_memory_is_an_error_that_says_so() {
        let h = ImageHeader {
            text_offset: 0,
            image_size: 40 * MIB,
        };
        let err = plan(&h, 40 * MIB, Some(20 * MIB), RAM, 48 * MIB).unwrap_err();
        assert!(matches!(err, Error::TooBig { .. }));
        assert!(err.to_string().contains("more memory"));
    }
}
