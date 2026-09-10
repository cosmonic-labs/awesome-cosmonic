//! A file-backed `wasi:keyvalue/store` provider, for tests only.
//!
//! `wasmtime serve` implements every WASI interface the server needs except
//! this one, so the e2e harness composes this component into the server with
//! `wac plug`. On Cosmonic Desktop the real host capability is used instead —
//! nothing here ships.
//!
//! **One key per file, not a map in memory.** `wasmtime serve` instantiates
//! the component afresh per request, so anything held in linear memory dies
//! with the request that wrote it — which is precisely the failure the real
//! keyvalue store exists to prevent. A preopened directory is the one place
//! separate instances can see each other's writes, so that is where state
//! goes. The harness passes it with `wasmtime serve --dir <host>::/kv`.

use std::path::PathBuf;

wit_bindgen::generate!({
    world: "kv-stub",
    path: "wit",
    generate_all,
});

use exports::wasi::keyvalue::store::{
    Bucket as BucketHandle, Error, Guest, GuestBucket, KeyResponse,
};

/// Guest path of the preopened directory holding the store.
fn store_dir() -> PathBuf {
    PathBuf::from(std::env::var("KV_STUB_DIR").unwrap_or_else(|_| "/kv".to_owned()))
}

/// Maps a key to a filename. Bridge keys contain `:`, which is not portable
/// in a path, and a key is never allowed to escape the directory.
fn key_path(key: &str) -> PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    store_dir().join(format!("{name}.bin"))
}

struct Component;

struct Bucket;

impl Guest for Component {
    type Bucket = Bucket;

    fn open(_identifier: String) -> Result<BucketHandle, Error> {
        // Any identifier resolves to the same directory: the harness only ever
        // asks for one bucket, and refusing unknown names would make the test
        // setup brittle for no benefit.
        std::fs::create_dir_all(store_dir())
            .map_err(|err| Error::Other(format!("create store dir: {err}")))?;
        Ok(BucketHandle::new(Bucket))
    }
}

impl GuestBucket for Bucket {
    fn get(&self, key: String) -> Result<Option<Vec<u8>>, Error> {
        match std::fs::read(key_path(&key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::Other(format!("read {key}: {err}"))),
        }
    }

    fn set(&self, key: String, value: Vec<u8>) -> Result<(), Error> {
        std::fs::write(key_path(&key), value)
            .map_err(|err| Error::Other(format!("write {key}: {err}")))
    }

    fn delete(&self, key: String) -> Result<(), Error> {
        match std::fs::remove_file(key_path(&key)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::Other(format!("delete {key}: {err}"))),
        }
    }

    fn exists(&self, key: String) -> Result<bool, Error> {
        Ok(key_path(&key).exists())
    }

    fn list_keys(&self, _cursor: Option<u64>) -> Result<KeyResponse, Error> {
        // Filenames are lossy (`:` became `_`), so this cannot round-trip the
        // original keys. The server never calls it; it exists to satisfy the
        // interface.
        Err(Error::Other(
            "list-keys is not supported by the test stub".into(),
        ))
    }
}

export!(Component);
