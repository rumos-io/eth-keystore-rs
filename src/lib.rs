#![cfg_attr(docsrs, feature(doc_cfg))]
//! A minimalist library to interact with encrypted JSON keystores as per the
//! [Web3 Secret Storage Definition](https://github.com/ethereum/wiki/wiki/Web3-Secret-Storage-Definition).

use aes::{
    cipher::{self, InnerIvInit, KeyInit, StreamCipherCore},
    Aes128,
};
use digest::{Digest, Update};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::{CryptoRng, Rng};
use scrypt::{scrypt, Params as ScryptParams};
use sha2::Sha256;
use sha3::Keccak256;
use uuid::{Builder, Bytes, Uuid};

use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

mod error;
mod keystore;
mod utils;

#[cfg(feature = "geth-compat")]
use utils::geth_compat::address_from_pk;

pub use error::KeystoreError;
pub use keystore::{CipherparamsJson, CryptoJson, EthKeystore, KdfType, KdfparamsType};

const DEFAULT_CIPHER: &str = "aes-128-ctr";
const DEFAULT_KEY_SIZE: usize = 32usize;
const DEFAULT_IV_SIZE: usize = 16usize;
const DEFAULT_KDF_PARAMS_DKLEN: u8 = 32u8;
const DEFAULT_KDF_PARAMS_LOG_N: u8 = 13u8;
const DEFAULT_KDF_PARAMS_R: u32 = 8u32;
const DEFAULT_KDF_PARAMS_P: u32 = 1u32;
const DEFAULT_KDF_LEN: usize = ScryptParams::RECOMMENDED_LEN; // This parameter is not used in the scrypt function, see https://github.com/RustCrypto/password-hashes/blob/f2b3d07499a39bfd999f9b53fb92e8a18f7db5d4/scrypt/src/params.rs#L14

/// Creates a new JSON keystore using the [Scrypt](https://tools.ietf.org/html/rfc7914.html)
/// key derivation function. The keystore is encrypted by a key derived from the provided `password`
/// and stored in the provided directory with either the user-provided filename, or a generated
/// Uuid `id`.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::new;
/// use std::path::Path;
///
/// # fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = Path::new("./keys");
/// let mut rng = rand::thread_rng();
/// // here `None` signifies we don't specify a filename for the keystore.
/// // the default filename is a generated Uuid for the keystore.
/// let (private_key, name) = new(&dir, &mut rng, "password_to_keystore", None)?;
///
/// // here `Some("my_key")` denotes a custom filename passed by the caller.
/// let (private_key, name) = new(&dir, &mut rng, "password_to_keystore", Some("my_key"))?;
/// # Ok(())
/// # }
/// ```
pub fn new<P, R, S>(
    dir: P,
    rng: &mut R,
    password: S,
    name: Option<&str>,
) -> Result<(Vec<u8>, String), KeystoreError>
where
    P: AsRef<Path>,
    R: Rng + CryptoRng,
    S: AsRef<[u8]>,
{
    // Generate a random private key.
    let mut pk = vec![0u8; DEFAULT_KEY_SIZE];
    rng.fill_bytes(pk.as_mut_slice());

    let name = encrypt_key(dir, rng, &pk, password, name)?;
    Ok((pk, name))
}

/// Decrypts an encrypted JSON keystore at the provided `path` using the provided `password`.
/// Decryption supports the [Scrypt](https://tools.ietf.org/html/rfc7914.html) and
/// [PBKDF2](https://ietf.org/rfc/rfc2898.txt) key derivation functions.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::decrypt_key;
/// use std::path::Path;
///
/// # fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let keypath = Path::new("./keys/my-key");
/// let private_key = decrypt_key(&keypath, "password_to_keystore")?;
/// # Ok(())
/// # }
/// ```
pub fn decrypt_key<P, S>(path: P, password: S) -> Result<Vec<u8>, KeystoreError>
where
    P: AsRef<Path>,
    S: AsRef<[u8]>,
{
    // Read the file contents as string and deserialize it.
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    decrypt_key_string(&contents, password)
}

/// Encrypts the given private key using the [Scrypt](https://tools.ietf.org/html/rfc7914.html)
/// password-based key derivation function, and stores it in the provided directory. On success, it
/// returns the `id` (Uuid) generated for this keystore.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::encrypt_key;
/// use rand::RngCore;
/// use std::path::Path;
///
/// # fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = Path::new("./keys");
/// let mut rng = rand::thread_rng();
///
/// // Construct a 32-byte random private key.
/// let mut private_key = vec![0u8; 32];
/// rng.fill_bytes(private_key.as_mut_slice());
///
/// // Since we specify a custom filename for the keystore, it will be stored in `$dir/my-key`
/// let name = encrypt_key(&dir, &mut rng, &private_key, "password_to_keystore", Some("my-key"))?;
/// # Ok(())
/// # }
/// ```
pub fn encrypt_key<P, R, B, S>(
    dir: P,
    rng: &mut R,
    pk: B,
    password: S,
    name: Option<&str>,
) -> Result<String, KeystoreError>
where
    P: AsRef<Path>,
    R: Rng + CryptoRng,
    B: AsRef<[u8]>,
    S: AsRef<[u8]>,
{
    let (contents, id) = encrypt_key_string(rng, pk, password);

    // If a file name is not specified for the keystore, simply use the stringified uuid.
    let name = if let Some(name) = name {
        name.to_string()
    } else {
        id.to_string()
    };

    // Create a file in write-only mode, to store the encrypted JSON keystore.
    let mut file = File::create(dir.as_ref().join(name))?;
    file.write_all(contents.as_bytes())?;

    Ok(id.to_string())
}

/// Encrypts the given private key using the [Scrypt](https://tools.ietf.org/html/rfc7914.html)
/// password-based key derivation function returning the encrypted JSON keystore as a string
/// and the derived Uuid.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::encrypt_key_string;
/// use rand::RngCore;
/// use std::path::Path;
/// use uuid::Uuid;
///
/// # fn foobar() -> (String, Uuid) {
/// let dir = Path::new("./keys");
/// let mut rng = rand::thread_rng();
///
/// // Construct a 32-byte random private key.
/// let mut private_key = vec![0u8; 32];
/// rng.fill_bytes(private_key.as_mut_slice());
///
/// encrypt_key_string(&mut rng, &private_key, "keystore_password")
/// # }
/// ```
pub fn encrypt_key_string<R, B, S>(rng: &mut R, pk: B, password: S) -> (String, Uuid)
where
    R: Rng + CryptoRng,
    B: AsRef<[u8]>,
    S: AsRef<[u8]>,
{
    // Generate a random salt.
    let mut salt = vec![0u8; DEFAULT_KEY_SIZE];
    rng.fill_bytes(salt.as_mut_slice());

    // Derive the key.
    let mut key = vec![0u8; DEFAULT_KDF_PARAMS_DKLEN as usize];
    let scrypt_params = ScryptParams::new(
        DEFAULT_KDF_PARAMS_LOG_N,
        DEFAULT_KDF_PARAMS_R,
        DEFAULT_KDF_PARAMS_P,
        DEFAULT_KDF_LEN,
    )
    .expect("hard coded values are valid");
    scrypt(password.as_ref(), &salt, &scrypt_params, key.as_mut_slice())
        .expect("key.len() > 0 && key.len() <= (2^32 - 1) * 32");

    // Encrypt the private key using AES-128-CTR.
    let mut iv = vec![0u8; DEFAULT_IV_SIZE];
    rng.fill_bytes(iv.as_mut_slice());

    let encryptor = Aes128Ctr::new(&key[..16], &iv[..16]).expect("invalid length");

    let mut ciphertext = pk.as_ref().to_vec();
    encryptor.apply_keystream(&mut ciphertext);

    // Calculate the MAC.
    let mac = Keccak256::new()
        .chain(&key[16..32])
        .chain(&ciphertext)
        .finalize();

    // Use provided rng instead of Uuid::new_v4() so that the caller can control the randomness.
    // This is useful for testing.
    let mut raw_id: Bytes = [0u8; 16];
    rng.fill_bytes(raw_id.as_mut_slice());
    let id = Builder::from_random_bytes(raw_id).into_uuid();

    // Construct and serialize the encrypted JSON key file.
    let key_file = EthKeystore {
        id,
        version: 3,
        crypto: CryptoJson {
            cipher: String::from(DEFAULT_CIPHER),
            cipherparams: CipherparamsJson { iv },
            ciphertext: ciphertext.to_vec(),
            kdf: KdfType::Scrypt,
            kdfparams: KdfparamsType::Scrypt {
                dklen: DEFAULT_KDF_PARAMS_DKLEN,
                n: 2u32.pow(DEFAULT_KDF_PARAMS_LOG_N as u32),
                p: DEFAULT_KDF_PARAMS_P,
                r: DEFAULT_KDF_PARAMS_R,
                salt,
            },
            mac: mac.to_vec(),
        },
        #[cfg(feature = "geth-compat")]
        address: address_from_pk(&pk)?,
    };

    (
        serde_json::to_string_pretty(&key_file).expect("serialization cannot fail"),
        id,
    )
}

/// Decrypts an encrypted JSON keystore from the provided `keystore` using the provided `password`.
/// Decryption supports the [Scrypt](https://tools.ietf.org/html/rfc7914.html) and
/// [PBKDF2](https://ietf.org/rfc/rfc2898.txt) key derivation functions.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::decrypt_key_string;
///
/// # fn foobar(keystore: &str) -> Result<(), Box<dyn std::error::Error>> {
/// let private_key = decrypt_key_string(&keystore, "password_to_keystore")?;
/// # Ok(())
/// # }
/// ```
pub fn decrypt_key_string<K, S>(keystore: K, password: S) -> Result<Vec<u8>, KeystoreError>
where
    K: AsRef<str>,
    S: AsRef<[u8]>,
{
    let keystore: EthKeystore = serde_json::from_str(keystore.as_ref())?;

    // Derive the key.
    let key = match keystore.crypto.kdfparams {
        KdfparamsType::Pbkdf2 {
            c,
            dklen,
            prf: _,
            salt,
        } => {
            let mut key = vec![0u8; dklen as usize];
            pbkdf2::<Hmac<Sha256>>(password.as_ref(), &salt, c, key.as_mut_slice())
                .expect("HMAC can be initialized with any key length"); // see https://github.com/RustCrypto/password-hashes/blob/165f4a8907354e89dbdc0cab0545f4d7fe8a89bd/pbkdf2/src/lib.rs#L160
            key
        }
        KdfparamsType::Scrypt {
            dklen,
            n,
            p,
            r,
            salt,
        } => {
            let mut key = vec![0u8; dklen as usize];
            // TODO: use int_log https://github.com/rust-lang/rust/issues/70887
            // TODO: when it is stable
            let log_n = (n as f32).log2().ceil() as u8;
            let scrypt_params = ScryptParams::new(log_n, r, p, DEFAULT_KDF_LEN)
                .expect("hard coded values are valid");
            scrypt(password.as_ref(), &salt, &scrypt_params, key.as_mut_slice())
                .expect("key.len() > 0 && key.len() <= (2^32 - 1) * 32");
            key
        }
    };

    // Derive the MAC from the derived key and ciphertext.
    let derived_mac = Keccak256::new()
        .chain(&key[16..32])
        .chain(&keystore.crypto.ciphertext)
        .finalize();

    if derived_mac.as_slice() != keystore.crypto.mac.as_slice() {
        return Err(KeystoreError::MacMismatch);
    }

    // Decrypt the private key bytes using AES-128-CTR
    let decryptor = Aes128Ctr::new(&key[..16], &keystore.crypto.cipherparams.iv[..16])
        .expect("hard coded lengths are valid");

    let mut pk = keystore.crypto.ciphertext;
    decryptor.apply_keystream(&mut pk);

    Ok(pk)
}
struct Aes128Ctr {
    inner: ctr::CtrCore<Aes128, ctr::flavors::Ctr128BE>,
}

impl Aes128Ctr {
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, cipher::InvalidLength> {
        let cipher = aes::Aes128::new_from_slice(key)?;
        let inner = ctr::CtrCore::inner_iv_slice_init(cipher, iv)?;
        Ok(Self { inner })
    }

    fn apply_keystream(self, buf: &mut [u8]) {
        self.inner.apply_keystream_partial(buf.into());
    }
}
