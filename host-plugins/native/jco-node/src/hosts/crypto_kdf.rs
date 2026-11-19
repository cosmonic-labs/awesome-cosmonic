//! `jco:node/crypto-kdf`: the key derivations `node:crypto` leaves to the host.

use hmac::Hmac;
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::crypto_kdf::{Error, Host},
    plugin::NodeView,
    state::Capability,
};

type KdfResult = wasmtime::Result<Result<Vec<u8>, Error>>;

fn coded(name: &str, code: &str, message: impl Into<String>) -> Error {
    Error { name: name.into(), message: message.into(), code: Some(code.into()) }
}

fn invalid_digest(digest: &str) -> Error {
    coded("TypeError", "ERR_CRYPTO_INVALID_DIGEST", format!("Invalid digest: {digest}"))
}

/// Run `$body` with `$hash` bound to the digest type a Node digest name selects.
macro_rules! with_digest {
    ($name:expr, |$hash:ident| $body:expr) => {{
        // OpenSSL matches digest names without regard to case, and accepts both spellings.
        match $name.to_ascii_lowercase().replace('_', "-").as_str() {
            "sha1" | "sha-1" => { type $hash = sha1::Sha1; Ok($body) }
            "sha224" | "sha-224" => { type $hash = sha2::Sha224; Ok($body) }
            "sha256" | "sha-256" => { type $hash = sha2::Sha256; Ok($body) }
            "sha384" | "sha-384" => { type $hash = sha2::Sha384; Ok($body) }
            "sha512" | "sha-512" => { type $hash = sha2::Sha512; Ok($body) }
            "sha512-224" => { type $hash = sha2::Sha512_224; Ok($body) }
            "sha512-256" => { type $hash = sha2::Sha512_256; Ok($body) }
            "sha3-224" => { type $hash = sha3::Sha3_224; Ok($body) }
            "sha3-256" => { type $hash = sha3::Sha3_256; Ok($body) }
            "sha3-384" => { type $hash = sha3::Sha3_384; Ok($body) }
            "sha3-512" => { type $hash = sha3::Sha3_512; Ok($body) }
            _ => Err(invalid_digest(&$name)),
        }
    }};
}

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, length: u32, digest: &str) -> Result<Vec<u8>, Error> {
    let mut output = vec![0u8; length as usize];
    with_digest!(digest, |Hash| {
        pbkdf2::pbkdf2::<Hmac<Hash>>(password, salt, iterations.max(1), &mut output)
            .map_err(|_| coded("Error", "ERR_CRYPTO_OPERATION_FAILED", "PBKDF2 failed"))?;
    })?;
    Ok(output)
}

fn hkdf(digest: &str, key: &[u8], salt: &[u8], info: &[u8], length: u32) -> Result<Vec<u8>, Error> {
    let mut output = vec![0u8; length as usize];
    with_digest!(digest, |Hash| {
        // The only way expansion fails is asking for more than 255 blocks of output.
        hkdf::Hkdf::<Hash>::new(Some(salt), key)
            .expand(info, &mut output)
            .map_err(|_| coded("RangeError", "ERR_CRYPTO_INVALID_KEYLEN", "Invalid key length"))?;
    })?;
    Ok(output)
}

fn scrypt(
    password: &[u8],
    salt: &[u8],
    length: u32,
    cost: u32,
    block_size: u32,
    parallelization: u32,
    maxmem: u64,
) -> Result<Vec<u8>, Error> {
    let invalid = |reason: &str| {
        coded("RangeError", "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", format!("Invalid scrypt params: {reason}"))
    };
    if cost < 2 || !cost.is_power_of_two() {
        return Err(invalid("N must be a power of two greater than one"));
    }
    // OpenSSL's bound: the block mixing state must fit in `maxmem`.
    let needed = 128u128 * u128::from(cost) * u128::from(block_size)
        + 128 * u128::from(block_size) * u128::from(parallelization);
    if needed > u128::from(maxmem) {
        return Err(invalid("memory limit exceeded"));
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    let parameters = scrypt::Params::new(cost.trailing_zeros() as u8, block_size, parallelization, 32)
        .map_err(|_| invalid("invalid r or p"))?;
    let mut output = vec![0u8; length as usize];
    scrypt::scrypt(password, salt, &parameters, &mut output).map_err(|_| invalid("invalid output length"))?;
    Ok(output)
}

impl NodeView<'_> {
    fn kdf_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::CryptoKdf) {
            return Ok(());
        }
        // Jco words this refusal as an unsupported API rather than a missing adapter.
        Err(coded(
            "Error",
            "ERR_JCO_UNSUPPORTED_NODE_API",
            "node:crypto host derivation is not supported in a WebAssembly component: map a key derivation provider",
        ))
    }
}

impl Host for NodeView<'_> {
    async fn pbkdf2(
        &mut self,
        password: Vec<u8>,
        salt: Vec<u8>,
        iterations: u32,
        length: u32,
        digest: String,
    ) -> KdfResult {
        if let Err(error) = self.kdf_granted() {
            return Ok(Err(error));
        }
        // Derivation is deliberately slow, so it runs off the async workers.
        Ok(tokio::task::spawn_blocking(move || pbkdf2(&password, &salt, iterations, length, &digest)).await?)
    }

    async fn hkdf(&mut self, digest: String, key: Vec<u8>, salt: Vec<u8>, info: Vec<u8>, length: u32) -> KdfResult {
        if let Err(error) = self.kdf_granted() {
            return Ok(Err(error));
        }
        Ok(hkdf(&digest, &key, &salt, &info, length))
    }

    async fn scrypt(
        &mut self,
        password: Vec<u8>,
        salt: Vec<u8>,
        length: u32,
        cost: u32,
        block_size: u32,
        parallelization: u32,
        maxmem: u64,
    ) -> KdfResult {
        if let Err(error) = self.kdf_granted() {
            return Ok(Err(error));
        }
        Ok(tokio::task::spawn_blocking(move || {
            scrypt(&password, &salt, length, cost, block_size, parallelization, maxmem)
        })
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    // Expected values are what Node's `crypto.*Sync` return for the same inputs.
    #[test]
    fn pbkdf2_matches_node() {
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 1, 20, "sha1").unwrap()),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 2, 32, "SHA256").unwrap()),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(pbkdf2(b"p", b"s", 1, 8, "nope").unwrap_err().code.as_deref(), Some("ERR_CRYPTO_INVALID_DIGEST"));
    }

    #[test]
    fn hkdf_matches_rfc_5869() {
        let key = [0x0b; 22];
        let salt: Vec<u8> = (0..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        assert_eq!(
            hex(&hkdf("sha256", &key, &salt, &info, 42).unwrap()),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
        assert_eq!(
            hkdf("sha256", &key, &salt, &info, 255 * 32 + 1).unwrap_err().code.as_deref(),
            Some("ERR_CRYPTO_INVALID_KEYLEN")
        );
    }

    #[test]
    fn scrypt_matches_rfc_7914() {
        assert_eq!(
            hex(&scrypt(b"", b"", 64, 16, 1, 1, 32 << 20).unwrap()),
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442\
             fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906"
        );
        assert_eq!(
            scrypt(b"", b"", 64, 1 << 20, 8, 1, 32 << 20).unwrap_err().code.as_deref(),
            Some("ERR_CRYPTO_INVALID_SCRYPT_PARAMS")
        );
    }
}
