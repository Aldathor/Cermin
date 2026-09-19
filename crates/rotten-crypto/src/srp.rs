use num_bigint::{BigUint, RandBigInt};
use rand::thread_rng;
use sha2::{Digest, Sha512};

/// SRP-6a client credentials (unused placeholder, kept for API compatibility).
#[derive(Debug, Clone)]
pub struct SrpCredentials {
    pub username: String,
    pub password: String,
    pub salt: Vec<u8>,
    pub verifier: Vec<u8>,
}

/// SRP-6a client for AirPlay 2 HomeKit pairing (RFC 5054 3072-bit group, SHA-512).
pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    a_pub: BigUint,
    session_key: Option<Vec<u8>>,
}

impl SrpClient {
    pub fn new() -> Self {
        let n = airplay_n();
        let g = BigUint::from(5u32);
        let mut rng = thread_rng();
        let a = rng.gen_biguint(1024);
        let a_pub = g.modpow(&a, &n);
        Self {
            n,
            g,
            a,
            a_pub,
            session_key: None,
        }
    }

    pub fn client_public(&self) -> Vec<u8> {
        pad_to_384(&self.a_pub)
    }

    /// SRP session key `K` from the last completed challenge (SHA-512 of S).
    pub fn session_key(&self) -> Option<Vec<u8>> {
        self.session_key.clone()
    }

    pub fn process_challenge(
        &mut self,
        salt: &[u8],
        server_public: &[u8],
        username: &str,
        pin: &str,
    ) -> (Vec<u8>, Vec<u8>) {
        let b = BigUint::from_bytes_be(server_public);
        let u = compute_u(&self.a_pub, &b);
        let x = compute_x(salt, username, pin);
        let k = compute_k(&self.n, &self.g);
        let s = compute_session_key(&self.n, &self.g, &k, &self.a, &b, &x, &u);
        let key = compute_key(&s);
        self.session_key = Some(key.clone());
        let proof = compute_proof(&self.n, &self.g, username, salt, &self.a_pub, &b, &key);
        let verifier = compute_server_proof(&self.a_pub, &proof, &key);
        (proof, verifier)
    }
}

impl Default for SrpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SrpClient {
    pub fn with_private(a: BigUint) -> Self {
        let n = airplay_n();
        let g = BigUint::from(5u32);
        let a_pub = g.modpow(&a, &n);
        Self {
            n,
            g,
            a,
            a_pub,
            session_key: None,
        }
    }
}

fn airplay_n() -> BigUint {
    // RFC 5054 Appendix A, 3072-bit group (HAP / AirPlay 2 uses the same group).
    let hex = include_str!("srp_modulus.txt").trim();
    BigUint::parse_bytes(hex.as_bytes(), 16).expect("valid modulus")
}

fn pad_to_384(n: &BigUint) -> Vec<u8> {
    let mut bytes = n.to_bytes_be();
    while bytes.len() < 384 {
        bytes.insert(0, 0);
    }
    bytes
}

/// Minimal big-endian bytes (leading zeros stripped), matching srptools `int_to_bytes`.
fn uint_bytes(n: &BigUint) -> Vec<u8> {
    if n == &BigUint::from(0u32) {
        return vec![0];
    }
    n.to_bytes_be()
}

fn compute_u(a_pub: &BigUint, b_pub: &BigUint) -> BigUint {
    let mut data = pad_to_384(a_pub);
    data.extend(pad_to_384(b_pub));
    hash_to_int(&data)
}

fn compute_x(salt: &[u8], username: &str, pin: &str) -> BigUint {
    let mut inner = Sha512::new();
    inner.update(format!("{username}:{pin}").as_bytes());
    let inner_hash = inner.finalize();
    let mut outer = Sha512::new();
    outer.update(salt);
    outer.update(&inner_hash);
    // x = H(salt | H(I | ":" | P)) — do NOT hash the digest twice.
    BigUint::from_bytes_be(&outer.finalize())
}

fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    let mut data = n.to_bytes_be();
    data.extend(pad_to_384(g));
    hash_to_int(&data)
}

fn compute_session_key(
    n: &BigUint,
    g: &BigUint,
    k: &BigUint,
    a: &BigUint,
    b: &BigUint,
    x: &BigUint,
    u: &BigUint,
) -> BigUint {
    let gx = g.modpow(x, n);
    let kgx = k * gx % n;
    // (B - k*g^x) mod N, avoiding BigUint underflow (add N before subtracting).
    let base = (&*b + n - kgx) % n;
    let exp = a + u * x;
    base.modpow(&exp, n)
}

/// K = H(S) with minimal big-endian S (matches srptools / RFC 5054).
fn compute_key(s: &BigUint) -> Vec<u8> {
    Sha512::digest(uint_bytes(s)).to_vec()
}

/// M1 = H( H(N) XOR H(g) | H(I) | s | A | B | K )
fn compute_proof(
    n: &BigUint,
    g: &BigUint,
    username: &str,
    salt: &[u8],
    a_pub: &BigUint,
    b_pub: &BigUint,
    key: &[u8],
) -> Vec<u8> {
    let h_n = Sha512::digest(uint_bytes(n));
    let h_g = Sha512::digest(uint_bytes(g));
    let h_xor = xor_be(&h_n, &h_g);
    let h_i = Sha512::digest(username.as_bytes());
    let mut h = Sha512::new();
    h.update(&h_xor);
    h.update(&h_i);
    h.update(salt);
    h.update(uint_bytes(a_pub));
    h.update(uint_bytes(b_pub));
    h.update(key);
    h.finalize().to_vec()
}

/// M2 = H(A | M1 | K )
fn compute_server_proof(a_pub: &BigUint, client_proof: &[u8], key: &[u8]) -> Vec<u8> {
    let mut h = Sha512::new();
    h.update(uint_bytes(a_pub));
    h.update(client_proof);
    h.update(key);
    h.finalize().to_vec()
}

fn xor_be(a: &[u8], b: &[u8]) -> Vec<u8> {
    let len = a.len().max(b.len());
    let mut out = vec![0u8; len];
    for (i, byte) in a.iter().rev().enumerate() {
        out[len - 1 - i] ^= byte;
    }
    for (i, byte) in b.iter().rev().enumerate() {
        out[len - 1 - i] ^= byte;
    }
    let first = out
        .iter()
        .position(|&x| x != 0)
        .unwrap_or(out.len().saturating_sub(1));
    out[first..].to_vec()
}

fn hash_to_int(data: &[u8]) -> BigUint {
    let hash = Sha512::digest(data);
    BigUint::from_bytes_be(&hash)
}

pub fn generate_salt() -> Vec<u8> {
    use rand::RngCore;
    let mut salt = vec![0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    #[test]
    fn hap_srp_vector() {
        let a = BigUint::parse_bytes(
            b"0f0e0d0c0b0a09080706050403020100ffeeddccbbaa99887766554433221100",
            16,
        )
        .unwrap();
        let mut c = SrpClient::with_private(a);
        let salt = h("00112233445566778899aabbccddeeff");
        let b = h(
            "12d494ca2d1d2f0f9709e5aac3de3c62da2626a6b04e5a84e16e483e1aaf1a6615803c42c14fe73b373069adc60beddbac5bfcdbf0ca05ecc22ac724d6db95328b2dc98c2d7857b12af131314bbf408796a00a4280062a0de93047a24ebdd71a7d8e246f42cc5243d4120ebbfcea127dab721973f29acea671cba485bfb8f524026e24bff8889d80bb5cbb9bf354981403273a4b63a1c6665b736bcc87f4e504f30ee7d25fe0c0756d492dc6d08aaeb54e887cfb4620103e77398ce7c8ae1d7ba3e0e5f10d746006e022cc8066d7f5ac204a4dafad27112b7e79de4044eb7b3ad2460fdf230a8163cf22bec37e5b1216fa286c5e021dac7dfa06df51b8698f0e9b6156a1fd9c7e6dffa5e909b67b86b787e9df5ae6daa658e7269e44d450df1bf159b68e26de327c17a0a2b80171ba3bc432f3103cf5fbfb3ca7bac4ee0fd253b55abd5476b3d01d7b741075314a2b06efa8889f0c546edc58f3f6ab361b309a2c38b9ec02a5cabcaae50d884500996479f13a7e3571aa2eb84bfc5bd5e41026",
        );
        let (proof, server_proof) = c.process_challenge(&salt, &b, "Pair-Setup", "1234");
        let x = compute_x(&salt, "Pair-Setup", "1234");
        let k = compute_k(&c.n, &c.g);
        let b_int = BigUint::from_bytes_be(&b);
        let u = compute_u(&c.a_pub, &b_int);
        let s = compute_session_key(&c.n, &c.g, &k, &c.a, &b_int, &x, &u);
        println!("x={x:x}");
        println!("k={k:x}");
        println!("u={u:x}");
        println!("S={s:x}");
        println!("K={}", hex::encode(compute_key(&s)));
        let mut ih = Sha512::new();
        ih.update(b"Pair-Setup:1234");
        println!("inner={}", hex::encode(ih.finalize()));
        println!("salt={}", hex::encode(&salt));
        println!("rustx={:x}", compute_x(&salt, "Pair-Setup", "1234"));
        assert_eq!(
            hex::encode(c.client_public()),
            "5773dc30e9525dc10e0e8e55eef3869c736755052deef4d351d4ec5f79a3e6c8ebcbc6edd8fe338eb6cc82c77f24cac639fcc05e0b9a432dc1c7d0cdd6ddc14fc9b148e106babfc8f30bebab3fb2811f6a6d52b7ed7a1078fc444c5b6298b78f3652fead285bd60b663acf9df598d5735284419b4f1353ad2a523ea0d3ec3f053eff87ff8eac551d81355b50458980f989c3e11cc43777837506507501cf6ba2abc688992e9da9b0e7fa32ff74d5253ef38f7c3c0a9412a43f61f1cab8be46f6635775ba60c44c11bc10c629ec0b5464886fa33c1abb859c6bd286d0d1a0752345549a9159aea634cfb026dcd541416d1e4229edfa42c71d8e395db989da7728b95c9b4c09f492abbe8c52a3d2e4962acb18126c9c0780a6e4390a23bea0e24ab39a9e956f100ba1e15916949619f3e54be59a64d7cc6d204a4ef839a7b1db81647a87d4ed3fec1cf2fa13d041e84ed5b9f7134cc875322e6df05bb28d8ad6fcca6b13a6d57082c3ed38eb5b49cd1d45e20b5396e8b353ec26fc28f5a1deb2c5"
        );
        assert_eq!(
            hex::encode(c.session_key().unwrap()),
            "698ccd0d33fce9fa72dd5d4d7be7ffa07101411872b1382fea75c9b824b959cf2df42f38f0019873f66b19396c17a21e641d4e184482a70432ba80ca4cc6f23d"
        );
        assert_eq!(
            hex::encode(&proof),
            "e3000bb07be253a566b848c78672b0196e8531e5407e6831c0a2c70b9861b81e35de71d6c2de0a272d401d8eead2491d7be71bfe3fb5b1d6d583e817fe261896"
        );
        assert_eq!(
            hex::encode(&server_proof),
            "2425f6a5529c0c5c807e943c49786c1b0c522e4dd683784659321304b0cd6565f8230b71b7db6a8a278a0ac9da16e96a16ae4b2e26d2a70b7fc084c592ffb855"
        );
    }
}
