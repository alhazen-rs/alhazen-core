//! `METHOD=AES-128`: whole segments encrypted with AES-128-CBC and PKCS#7 padding.

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};

use crate::{Error, Result};

type Decryptor = cbc::Decryptor<aes::Aes128>;

pub(crate) fn decrypt_aes128(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    Decryptor::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Error::Demux("AES-128: bad padding (wrong key?)".into()))
}

/// The IV of a segment whose key has none: its media sequence number, big-endian.
pub(crate) fn sequence_iv(sequence: u64) -> [u8; 16] {
    (sequence as u128).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const IV: [u8; 16] = [15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0];
    /// `printf 'hello hls world!!' | openssl enc -aes-128-cbc -K 000102…0f -iv 0f0e…00`
    const CIPHER: [u8; 32] = [
        0xc1, 0xad, 0xf1, 0x74, 0xff, 0xe0, 0x6a, 0x48, 0xd0, 0x51, 0xad, 0x13, 0xc1, 0xd8, 0x8b, 0xf5, 0x35, 0x6a, 0xba,
        0xba, 0x5e, 0x20, 0xb7, 0xba, 0xd6, 0x1d, 0xaa, 0x55, 0x0c, 0xc0, 0x1e, 0x3f,
    ];

    #[test]
    fn decrypts_openssl_output() {
        assert_eq!(decrypt_aes128(&CIPHER, &KEY, &IV).unwrap(), b"hello hls world!!");
    }

    #[test]
    fn bad_padding_is_an_error() {
        assert!(decrypt_aes128(&[0u8; 16], &[0; 16], &[0; 16]).is_err());
        assert!(decrypt_aes128(&CIPHER[..20], &KEY, &IV).is_err(), "not whole blocks");
    }

    #[test]
    fn sequence_iv_is_big_endian() {
        let iv = sequence_iv(0x0102);
        assert_eq!(&iv[14..], &[1, 2]);
        assert!(iv[..14].iter().all(|&b| b == 0));
    }
}
