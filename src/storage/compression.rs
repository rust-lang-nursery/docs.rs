use bzip2::read::{BzDecoder, BzEncoder};
use bzip2::Compression;
use failure::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt,
    io::{self, Read},
};

pub type CompressionAlgorithms = HashSet<CompressionAlgorithm>;

macro_rules! enum_id {
    ($vis:vis enum $name:ident { $($variant:ident = $discriminant:expr,)* }) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        $vis enum $name {
            $($variant = $discriminant,)*
        }

        impl $name {
            #[cfg(test)]
            const AVAILABLE: &'static [Self] = &[$(Self::$variant,)*];
        }

        impl fmt::Display for CompressionAlgorithm {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant => write!(f, stringify!($variant)),)*
                }
            }
        }

        impl std::str::FromStr for CompressionAlgorithm {
            type Err = ();
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $(stringify!($variant) => Ok(Self::$variant),)*
                    _ => Err(()),
                }
            }
        }

        impl std::convert::TryFrom<i32> for CompressionAlgorithm {
            type Error = i32;
            fn try_from(i: i32) -> Result<Self, Self::Error> {
                match i {
                    $($discriminant => Ok(Self::$variant),)*
                    _ => Err(i),
                }
            }
        }
    }
}

enum_id! {
    pub enum CompressionAlgorithm {
        Zstd = 0,
        Bzip2 = 1,
    }
}

impl Default for CompressionAlgorithm {
    fn default() -> Self {
        CompressionAlgorithm::Zstd
    }
}

// public for benchmarking
pub fn compress(content: impl Read, algorithm: CompressionAlgorithm) -> Result<Vec<u8>, Error> {
    match algorithm {
        CompressionAlgorithm::Zstd => Ok(zstd::encode_all(content, 9)?),
        CompressionAlgorithm::Bzip2 => {
            let mut compressor = BzEncoder::new(content, Compression::best());

            let mut data = vec![];
            compressor.read_to_end(&mut data)?;
            Ok(data)
        }
    }
}

pub fn decompress(
    content: impl Read,
    algorithm: CompressionAlgorithm,
    max_size: usize,
) -> Result<Vec<u8>, Error> {
    // The sized buffer prevents a malicious file from decompressing to multiple times its size.
    let mut buffer = crate::utils::sized_buffer::SizedBuffer::new(max_size);

    match algorithm {
        CompressionAlgorithm::Zstd => zstd::stream::copy_decode(content, &mut buffer)?,
        CompressionAlgorithm::Bzip2 => {
            io::copy(&mut BzDecoder::new(content), &mut buffer)?;
        }
    }

    Ok(buffer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression() {
        let orig = "fn main() {}";
        for alg in CompressionAlgorithm::AVAILABLE {
            println!("testing algorithm {}", alg);

            let data = compress(orig.as_bytes(), *alg).unwrap();
            assert_eq!(
                decompress(data.as_slice(), *alg, std::usize::MAX).unwrap(),
                orig.as_bytes()
            );
        }
    }

    #[test]
    fn test_decompression_too_big() {
        const MAX_SIZE: usize = 1024;

        let small = &[b'A'; MAX_SIZE / 2] as &[u8];
        let exact = &[b'A'; MAX_SIZE] as &[u8];
        let big = &[b'A'; MAX_SIZE * 2] as &[u8];

        for &alg in CompressionAlgorithm::AVAILABLE {
            let compressed_small = compress(small, alg).unwrap();
            let compressed_exact = compress(exact, alg).unwrap();
            let compressed_big = compress(big, alg).unwrap();

            // Ensure decompressing within the limit works.
            assert_eq!(
                small.len(),
                decompress(compressed_small.as_slice(), alg, MAX_SIZE)
                    .unwrap()
                    .len()
            );
            assert_eq!(
                exact.len(),
                decompress(compressed_exact.as_slice(), alg, MAX_SIZE)
                    .unwrap()
                    .len()
            );

            // Ensure decompressing a file over the limit returns a SizeLimitReached error.
            let err = decompress(compressed_big.as_slice(), alg, MAX_SIZE).unwrap_err();
            assert!(err
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .and_then(|err| err.downcast_ref::<crate::error::SizeLimitReached>())
                .is_some());
        }
    }
}
