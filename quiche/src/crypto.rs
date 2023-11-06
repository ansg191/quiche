// Copyright (C) 2018-2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::convert::TryFrom;
use std::mem::MaybeUninit;

use libc::c_int;
use libc::c_void;

use crate::Error;
use crate::Result;

use crate::packet;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Initial   = 0,
    ZeroRTT   = 1,
    Handshake = 2,
    OneRTT    = 3,
}

impl Level {
    pub fn from_epoch(e: packet::Epoch) -> Level {
        match e {
            packet::Epoch::Initial => Level::Initial,

            packet::Epoch::Handshake => Level::Handshake,

            packet::Epoch::Application => Level::OneRTT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    #[allow(non_camel_case_types)]
    AES128_GCM,

    #[allow(non_camel_case_types)]
    AES256_GCM,

    #[allow(non_camel_case_types)]
    ChaCha20_Poly1305,
}

impl Algorithm {
    fn get_evp_aead(self) -> *const EVP_AEAD {
        match self {
            Algorithm::AES128_GCM => unsafe { EVP_aead_aes_128_gcm() },
            Algorithm::AES256_GCM => unsafe { EVP_aead_aes_256_gcm() },
            Algorithm::ChaCha20_Poly1305 => unsafe {
                EVP_aead_chacha20_poly1305()
            },
        }
    }

    fn get_evp_md(self) -> *const EVP_MD {
        match self {
            Self::AES128_GCM => unsafe { EVP_sha256() },
            Self::AES256_GCM => unsafe { EVP_sha384() },
            Self::ChaCha20_Poly1305 => unsafe { EVP_sha256() },
        }
    }

    fn prk_len(self) -> usize {
        match self {
            Algorithm::AES128_GCM => 32,
            Algorithm::AES256_GCM => 48,
            Algorithm::ChaCha20_Poly1305 => 32,
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            Algorithm::AES128_GCM => 16,
            Algorithm::AES256_GCM => 32,
            Algorithm::ChaCha20_Poly1305 => 32,
        }
    }

    pub fn tag_len(self) -> usize {
        if cfg!(feature = "fuzzing") {
            return 0;
        }

        match self {
            Algorithm::AES128_GCM => 16,
            Algorithm::AES256_GCM => 16,
            Algorithm::ChaCha20_Poly1305 => 16,
        }
    }

    pub fn nonce_len(self) -> usize {
        match self {
            Algorithm::AES128_GCM => 12,
            Algorithm::AES256_GCM => 12,
            Algorithm::ChaCha20_Poly1305 => 12,
        }
    }
}

pub struct Open {
    alg: Algorithm,

    secret: Vec<u8>,

    header: HeaderProtectionKey,

    packet: PacketKey,
}

impl Open {
    pub fn new(
        alg: Algorithm, key: Vec<u8>, iv: Vec<u8>, hp_key: Vec<u8>,
        secret: Vec<u8>,
    ) -> Result<Open> {
        Ok(Open {
            alg,

            header: HeaderProtectionKey::new(alg, hp_key)?,

            packet: PacketKey::new(alg, key, iv)?,

            secret,
        })
    }

    pub fn from_secret(aead: Algorithm, secret: Vec<u8>) -> Result<Open> {
        Ok(Open {
            alg: aead,

            header: HeaderProtectionKey::from_secret(aead, &secret)?,

            packet: PacketKey::from_secret(aead, &secret)?,

            secret,
        })
    }

    pub fn open_with_u64_counter(
        &self, counter: u64, ad: &[u8], buf: &mut [u8],
    ) -> Result<usize> {
        if cfg!(feature = "fuzzing") {
            return Ok(buf.len());
        }

        let tag_len = self.alg().tag_len();
        if tag_len > buf.len() {
            return Err(Error::CryptoFail);
        }

        let nonce = make_nonce(&self.packet.nonce, counter);

        self.packet.ctx.open(buf, &nonce, ad)
    }

    pub fn new_mask(&self, sample: &[u8]) -> Result<[u8; 5]> {
        if cfg!(feature = "fuzzing") {
            return Ok(<[u8; 5]>::default());
        }

        let mask = self.header.new_mask(sample)?;

        Ok(mask)
    }

    pub fn alg(&self) -> Algorithm {
        self.alg
    }

    pub fn derive_next_packet_key(&self) -> Result<Open> {
        let next_secret = derive_next_secret(self.alg, &self.secret)?;

        let next_packet_key = PacketKey::from_secret(self.alg, &next_secret)?;

        Ok(Open {
            alg: self.alg,

            secret: next_secret,

            header: HeaderProtectionKey::new(
                self.alg,
                self.header.hp_key.clone(),
            )?,

            packet: next_packet_key,
        })
    }
}

pub struct Seal {
    alg: Algorithm,

    secret: Vec<u8>,

    header: HeaderProtectionKey,

    packet: PacketKey,
}

impl Seal {
    pub fn new(
        alg: Algorithm, key: Vec<u8>, iv: Vec<u8>, hp_key: Vec<u8>,
        secret: Vec<u8>,
    ) -> Result<Seal> {
        Ok(Seal {
            alg,

            header: HeaderProtectionKey::new(alg, hp_key)?,

            packet: PacketKey::new(alg, key, iv)?,

            secret,
        })
    }

    pub fn from_secret(aead: Algorithm, secret: Vec<u8>) -> Result<Seal> {
        Ok(Seal {
            alg: aead,

            header: HeaderProtectionKey::from_secret(aead, &secret)?,

            packet: PacketKey::from_secret(aead, &secret)?,

            secret,
        })
    }

    pub fn seal_with_u64_counter(
        &self, counter: u64, ad: &[u8], buf: &mut [u8], in_len: usize,
        extra_in: Option<&[u8]>,
    ) -> Result<usize> {
        if cfg!(feature = "fuzzing") {
            if let Some(extra) = extra_in {
                buf[in_len..in_len + extra.len()].copy_from_slice(extra);
                return Ok(in_len + extra.len());
            }

            return Ok(in_len);
        }

        let tag_len = self.alg().tag_len();

        let extra_in_len = extra_in.map_or(0, |v| v.len());

        // Make sure all the outputs combined fit in the buffer.
        if in_len + tag_len + extra_in_len > buf.len() {
            return Err(Error::CryptoFail);
        }

        let nonce = make_nonce(&self.packet.nonce, counter);

        let (in_out, out_tag) = buf.split_at_mut(in_len);

        let out_tag_len = self
            .packet
            .ctx
            .seal_scatter(in_out, out_tag, &nonce, extra_in, ad)?;

        Ok(in_len + out_tag_len)
    }

    pub fn new_mask(&self, sample: &[u8]) -> Result<[u8; 5]> {
        if cfg!(feature = "fuzzing") {
            return Ok(<[u8; 5]>::default());
        }

        let mask = self.header.new_mask(sample)?;

        Ok(mask)
    }

    pub fn alg(&self) -> Algorithm {
        self.alg
    }

    pub fn derive_next_packet_key(&self) -> Result<Seal> {
        let next_secret = derive_next_secret(self.alg, &self.secret)?;

        let next_packet_key = PacketKey::from_secret(self.alg, &next_secret)?;

        Ok(Seal {
            alg: self.alg,

            secret: next_secret,

            header: HeaderProtectionKey::new(
                self.alg,
                self.header.hp_key.clone(),
            )?,

            packet: next_packet_key,
        })
    }
}

pub struct HeaderProtectionKey {
    alg: Algorithm,
    hp_key: Vec<u8>,
}

impl HeaderProtectionKey {
    pub fn new(alg: Algorithm, hp_key: Vec<u8>) -> Result<Self> {
        if hp_key.len() == alg.key_len() {
            Ok(Self { alg, hp_key })
        } else {
            Err(Error::CryptoFail)
        }
    }

    pub fn from_secret(aead: Algorithm, secret: &[u8]) -> Result<Self> {
        let key_len = aead.key_len();

        let mut hp_key = vec![0; key_len];

        derive_hdr_key(aead, secret, &mut hp_key)?;

        Self::new(aead, hp_key)
    }

    pub fn new_mask(&self, sample: &[u8]) -> Result<[u8; 5]> {
        const SAMPLE_LEN: usize = 16;
        let sample: &[u8; SAMPLE_LEN] =
            TryFrom::try_from(sample).map_err(|_| Error::CryptoFail)?;

        Ok(match self.alg {
            Algorithm::AES128_GCM =>
                AES_KEY::new(128, &self.hp_key)?.new_mask(sample),
            Algorithm::AES256_GCM =>
                AES_KEY::new(256, &self.hp_key)?.new_mask(sample),
            Algorithm::ChaCha20_Poly1305 => chacha_mask(&self.hp_key, sample)?,
        })
    }
}

pub struct PacketKey {
    ctx: EVP_AEAD_CTX,

    nonce: Vec<u8>,
}

impl PacketKey {
    pub fn new(alg: Algorithm, key: Vec<u8>, iv: Vec<u8>) -> Result<Self> {
        Ok(Self {
            ctx: EVP_AEAD_CTX::new(alg, &key)?,

            nonce: iv,
        })
    }

    pub fn from_secret(aead: Algorithm, secret: &[u8]) -> Result<Self> {
        let key_len = aead.key_len();
        let nonce_len = aead.nonce_len();

        let mut key = vec![0; key_len];
        let mut iv = vec![0; nonce_len];

        derive_pkt_key(aead, secret, &mut key)?;
        derive_pkt_iv(aead, secret, &mut iv)?;

        Self::new(aead, key, iv)
    }
}

pub struct Prk {
    alg: Algorithm,
    key: Vec<u8>,
}

impl Prk {
    pub fn new(alg: Algorithm, salt: &[u8], secret: &[u8]) -> Result<Self> {
        let md = alg.get_evp_md();

        let mut prk = vec![0; alg.prk_len()];
        let mut prk_len = 0;

        let result = unsafe {
            HKDF_extract(
                prk.as_mut_ptr(), // out_key
                &mut prk_len,     // out_len
                md,               // digest
                secret.as_ptr(),  // secret
                secret.len(),     // secret_len
                salt.as_ptr(),    // salt
                salt.len(),       // salt_len
            )
        };
        if result == 1 {
            debug_assert_eq!(prk_len, prk.len());
            Ok(Self { alg, key: prk })
        } else {
            Err(Error::CryptoFail)
        }
    }

    pub fn new_less_safe(alg: Algorithm, value: &[u8]) -> Self {
        Self {
            alg,
            key: Vec::from(value),
        }
    }

    pub fn expand(
        &self, info: &[&[u8]], len: usize, out: &mut [u8],
    ) -> Result<()> {
        let md = self.alg.get_evp_md();

        if len > 255 * self.alg.prk_len() {
            return Err(Error::CryptoFail);
        }

        let info: Vec<u8> =
            info.iter().flat_map(|&x| x.iter()).copied().collect();

        let result = unsafe {
            HKDF_expand(
                out.as_mut_ptr(),  // out_key
                len,               // out_len
                md,                // digest
                self.key.as_ptr(), // prk
                self.key.len(),    // prk_len
                info.as_ptr(),     // info
                info.len(),        // info_len
            )
        };
        if result == 1 {
            Ok(())
        } else {
            Err(Error::CryptoFail)
        }
    }
}

pub fn derive_initial_key_material(
    cid: &[u8], version: u32, is_server: bool,
) -> Result<(Open, Seal)> {
    let mut client_secret = [0; 32];
    let mut server_secret = [0; 32];

    let aead = Algorithm::AES128_GCM;

    let key_len = aead.key_len();
    let nonce_len = aead.nonce_len();

    let initial_secret = derive_initial_secret(cid, version)?;

    // Client.
    let mut client_key = vec![0; key_len];
    let mut client_iv = vec![0; nonce_len];
    let mut client_hp_key = vec![0; key_len];

    derive_client_initial_secret(&initial_secret, &mut client_secret)?;
    derive_pkt_key(aead, &client_secret, &mut client_key)?;
    derive_pkt_iv(aead, &client_secret, &mut client_iv)?;
    derive_hdr_key(aead, &client_secret, &mut client_hp_key)?;

    // Server.
    let mut server_key = vec![0; key_len];
    let mut server_iv = vec![0; nonce_len];
    let mut server_hp_key = vec![0; key_len];

    derive_server_initial_secret(&initial_secret, &mut server_secret)?;
    derive_pkt_key(aead, &server_secret, &mut server_key)?;
    derive_pkt_iv(aead, &server_secret, &mut server_iv)?;
    derive_hdr_key(aead, &server_secret, &mut server_hp_key)?;

    let (open, seal) = if is_server {
        (
            Open::new(
                aead,
                client_key,
                client_iv,
                client_hp_key,
                client_secret.to_vec(),
            )?,
            Seal::new(
                aead,
                server_key,
                server_iv,
                server_hp_key,
                server_secret.to_vec(),
            )?,
        )
    } else {
        (
            Open::new(
                aead,
                server_key,
                server_iv,
                server_hp_key,
                server_secret.to_vec(),
            )?,
            Seal::new(
                aead,
                client_key,
                client_iv,
                client_hp_key,
                client_secret.to_vec(),
            )?,
        )
    };

    Ok((open, seal))
}

fn derive_initial_secret(secret: &[u8], version: u32) -> Result<Prk> {
    const INITIAL_SALT_V1: [u8; 20] = [
        0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6,
        0xa4, 0xc8, 0x0c, 0xad, 0xcc, 0xbb, 0x7f, 0x0a,
    ];

    let salt = match version {
        crate::PROTOCOL_VERSION_V1 => &INITIAL_SALT_V1,

        _ => &INITIAL_SALT_V1,
    };

    Prk::new(Algorithm::AES128_GCM, salt, secret)
}

fn derive_client_initial_secret(prk: &Prk, out: &mut [u8]) -> Result<()> {
    const LABEL: &[u8] = b"client in";
    hkdf_expand_label(prk, LABEL, out)
}

fn derive_server_initial_secret(prk: &Prk, out: &mut [u8]) -> Result<()> {
    const LABEL: &[u8] = b"server in";
    hkdf_expand_label(prk, LABEL, out)
}

fn derive_next_secret(aead: Algorithm, secret: &[u8]) -> Result<Vec<u8>> {
    const LABEL: &[u8] = b"quic ku";

    let mut next_secret = vec![0; secret.len()];

    let secret_prk = Prk::new_less_safe(aead, secret);
    hkdf_expand_label(&secret_prk, LABEL, &mut next_secret)?;

    Ok(next_secret)
}

pub fn derive_hdr_key(
    aead: Algorithm, secret: &[u8], out: &mut [u8],
) -> Result<()> {
    const LABEL: &[u8] = b"quic hp";

    let key_len = aead.key_len();

    if key_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = Prk::new_less_safe(aead, secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..key_len])
}

pub fn derive_pkt_key(
    aead: Algorithm, secret: &[u8], out: &mut [u8],
) -> Result<()> {
    const LABEL: &[u8] = b"quic key";

    let key_len = aead.key_len();

    if key_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = Prk::new_less_safe(aead, secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..key_len])
}

pub fn derive_pkt_iv(
    aead: Algorithm, secret: &[u8], out: &mut [u8],
) -> Result<()> {
    const LABEL: &[u8] = b"quic iv";

    let nonce_len = aead.nonce_len();

    if nonce_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = Prk::new_less_safe(aead, secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..nonce_len])
}

fn hkdf_expand_label(prk: &Prk, label: &[u8], out: &mut [u8]) -> Result<()> {
    const LABEL_PREFIX: &[u8] = b"tls13 ";

    let out_len = (out.len() as u16).to_be_bytes();
    let label_len = (LABEL_PREFIX.len() + label.len()) as u8;

    let info: [&[u8]; 5] =
        [&out_len, &[label_len][..], LABEL_PREFIX, label, &[0][..]];

    prk.expand(&info, out.len(), out)
}

fn make_nonce(iv: &[u8], counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce.copy_from_slice(iv);

    // XOR the last bytes of the IV with the counter. This is equivalent to
    // left-padding the counter with zero bytes.
    for (a, b) in nonce[4..].iter_mut().zip(counter.to_be_bytes().iter()) {
        *a ^= b;
    }

    nonce
}

fn chacha_mask(key: &[u8], sample: &[u8; 16]) -> Result<[u8; 5]> {
    let mut out = [0; 5];

    let key: &[u8; 32] = TryFrom::try_from(key).map_err(|_| Error::CryptoFail)?;

    let counter = u32::from_le_bytes(
        TryFrom::try_from(&sample[..4]).unwrap_or_else(|_| unreachable!()),
    );
    let nonce: &[u8; 12] =
        TryFrom::try_from(&sample[4..16]).unwrap_or_else(|_| unreachable!());

    unsafe {
        CRYPTO_chacha_20(
            out.as_mut_ptr(), // out
            out.as_ptr(),     // in
            out.len(),        // in_len
            key,              // key
            nonce,            // nonce
            counter,          // counter
        )
    }

    Ok(out)
}

#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct EVP_AEAD(c_void);

// NOTE: This structure is copied from <openssl/aead.h> in order to be able to
// statically allocate it. While it is not often modified upstream, it needs to
// be kept in sync.
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct EVP_AEAD_CTX {
    aead: *const EVP_AEAD,
    opaque: [u8; 580],
    alignment: u64,
    tag_len: u8,
}

impl Drop for EVP_AEAD_CTX {
    fn drop(&mut self) {
        unsafe {
            EVP_AEAD_CTX_cleanup(self);
        }
    }
}

unsafe impl Send for EVP_AEAD_CTX {}
unsafe impl Sync for EVP_AEAD_CTX {}

impl EVP_AEAD_CTX {
    pub fn new(alg: Algorithm, key: &[u8]) -> Result<Self> {
        if key.len() != alg.key_len() {
            return Err(Error::CryptoFail);
        }

        let mut ctx = MaybeUninit::uninit();

        // SAFETY: `key` & `ctx` are correctly sized.
        // `ctx` will be initialized by `EVP_AEAD_CTX_init`.
        let ctx = unsafe {
            let aead = alg.get_evp_aead();

            let rc = EVP_AEAD_CTX_init(
                ctx.as_mut_ptr(),     // ctx
                aead,                 // aead
                key.as_ptr(),         // key
                alg.key_len(),        // key_len
                alg.tag_len(),        // tag_len
                std::ptr::null_mut(), // engine
            );

            if rc != 1 {
                return Err(Error::CryptoFail);
            }

            ctx.assume_init()
        };

        Ok(ctx)
    }

    pub fn open(
        &self, in_out: &mut [u8], nonce: &[u8; 12], ad: &[u8],
    ) -> Result<usize> {
        let mut out_len = 0;
        let rc = unsafe {
            EVP_AEAD_CTX_open(
                self,                // ctx
                in_out.as_mut_ptr(), // out
                &mut out_len,        // out_len
                in_out.len(),        // max_out_len
                nonce.as_ptr(),      // nonce
                nonce.len(),         // nonce_len
                in_out.as_ptr(),     // inp
                in_out.len(),        // in_len
                ad.as_ptr(),         // ad
                ad.len(),            // ad_len
            )
        };
        if rc == 1 {
            Ok(out_len)
        } else {
            Err(Error::CryptoFail)
        }
    }

    pub fn seal_scatter(
        &self, in_out: &mut [u8], out_tag: &mut [u8], nonce: &[u8; 12],
        extra_in: Option<&[u8]>, ad: &[u8],
    ) -> Result<usize> {
        let extra_in_len = extra_in.map_or(0, |v| v.len());
        let max_out_tag_len = self.overhead() + extra_in_len;

        // Ensure out_tag is large enough
        if max_out_tag_len > out_tag.len() {
            return Err(Error::CryptoFail);
        }

        let extra_in = extra_in.map_or(std::ptr::null(), |v| v.as_ptr());

        let mut out_tag_len = 0;
        let rc = unsafe {
            EVP_AEAD_CTX_seal_scatter(
                self,                 // ctx
                in_out.as_mut_ptr(),  // out
                out_tag.as_mut_ptr(), // out_tag
                &mut out_tag_len,     // out_tag_len
                max_out_tag_len,      // max_out_tag_len
                nonce.as_ptr(),       // nonce
                nonce.len(),          // nonce_len
                in_out.as_ptr(),      // inp
                in_out.len(),         // in_len
                extra_in,             // extra_in
                extra_in_len,         // extra_in_len
                ad.as_ptr(),          // ad
                ad.len(),             // ad_len
            )
        };

        if rc == 1 {
            Ok(out_tag_len)
        } else {
            Err(Error::CryptoFail)
        }
    }

    fn overhead(&self) -> usize {
        unsafe { EVP_AEAD_max_overhead(self.aead) }
    }
}

#[allow(non_camel_case_types)]
#[repr(transparent)]
struct EVP_MD(c_void);

// NOTE: This structure is copied `aes_key_st` from <openssl/aes.h>
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct AES_KEY {
    rd_key: [u32; 240],
    rounds: libc::c_uint,
}

impl AES_KEY {
    pub fn new(bits: u16, key: &[u8]) -> Result<Self> {
        if key.len() != bits as usize / 8 {
            return Err(Error::CryptoFail);
        }

        let mut aes_key = MaybeUninit::uninit();

        // SAFETY: `key` & `aes_key` are correctly sized.
        // `aes_key` will be initialized by `AES_set_encrypt_key`.
        let aes_key = unsafe {
            let rc = AES_set_encrypt_key(
                key.as_ptr(),         // key
                bits as libc::c_uint, // bits
                aes_key.as_mut_ptr(), // aes_key
            );

            if rc != 0 {
                return Err(Error::CryptoFail);
            }

            aes_key.assume_init()
        };

        Ok(aes_key)
    }

    pub fn new_mask(&self, sample: &[u8; 16]) -> [u8; 5] {
        let mut block = [0; 16];
        unsafe {
            AES_encrypt(sample.as_ptr(), block.as_mut_ptr(), self);
        }

        let mut out = [0; 5];
        out.copy_from_slice(&block[..5]);
        out
    }
}

extern {
    // EVP_AEAD
    fn EVP_aead_aes_128_gcm() -> *const EVP_AEAD;

    fn EVP_aead_aes_256_gcm() -> *const EVP_AEAD;

    fn EVP_aead_chacha20_poly1305() -> *const EVP_AEAD;

    fn EVP_AEAD_max_overhead(aead: *const EVP_AEAD) -> usize;

    // EVP_AEAD_CTX
    fn EVP_AEAD_CTX_init(
        ctx: *mut EVP_AEAD_CTX, aead: *const EVP_AEAD, key: *const u8,
        key_len: usize, tag_len: usize, engine: *mut c_void,
    ) -> c_int;

    fn EVP_AEAD_CTX_cleanup(ctx: *mut EVP_AEAD_CTX);

    fn EVP_AEAD_CTX_open(
        ctx: *const EVP_AEAD_CTX, out: *mut u8, out_len: *mut usize,
        max_out_len: usize, nonce: *const u8, nonce_len: usize, inp: *const u8,
        in_len: usize, ad: *const u8, ad_len: usize,
    ) -> c_int;

    fn EVP_AEAD_CTX_seal_scatter(
        ctx: *const EVP_AEAD_CTX, out: *mut u8, out_tag: *mut u8,
        out_tag_len: *mut usize, max_out_tag_len: usize, nonce: *const u8,
        nonce_len: usize, inp: *const u8, in_len: usize, extra_in: *const u8,
        extra_in_len: usize, ad: *const u8, ad_len: usize,
    ) -> c_int;

    // EVP_MD
    fn EVP_sha256() -> *const EVP_MD;

    fn EVP_sha384() -> *const EVP_MD;

    // HKDF
    fn HKDF_extract(
        out_key: *mut u8, out_len: *mut usize, digest: *const EVP_MD,
        secret: *const u8, secret_len: usize, salt: *const u8, salt_len: usize,
    ) -> c_int;

    fn HKDF_expand(
        out_key: *mut u8, out_len: usize, digest: *const EVP_MD, prk: *const u8,
        prk_len: usize, info: *const u8, info_len: usize,
    ) -> c_int;

    // AES
    fn AES_set_encrypt_key(
        key: *const u8, bits: libc::c_uint, aes_key: *mut AES_KEY,
    ) -> c_int;

    fn AES_encrypt(input: *const u8, output: *mut u8, key: *const AES_KEY);

    // ChaCha20
    fn CRYPTO_chacha_20(
        out: *mut u8, inp: *const u8, in_len: usize, key: *const [u8; 32],
        nonce: *const [u8; 12], counter: u32,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_initial_secrets_v1() {
        let dcid = [0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];

        let mut secret = [0; 32];
        let mut pkt_key = [0; 16];
        let mut pkt_iv = [0; 12];
        let mut hdr_key = [0; 16];

        let aead = Algorithm::AES128_GCM;

        let initial_secret =
            derive_initial_secret(&dcid, crate::PROTOCOL_VERSION_V1).unwrap();

        // Client.
        assert!(
            derive_client_initial_secret(&initial_secret, &mut secret).is_ok()
        );
        let expected_client_initial_secret = [
            0xc0, 0x0c, 0xf1, 0x51, 0xca, 0x5b, 0xe0, 0x75, 0xed, 0x0e, 0xbf,
            0xb5, 0xc8, 0x03, 0x23, 0xc4, 0x2d, 0x6b, 0x7d, 0xb6, 0x78, 0x81,
            0x28, 0x9a, 0xf4, 0x00, 0x8f, 0x1f, 0x6c, 0x35, 0x7a, 0xea,
        ];
        assert_eq!(&secret, &expected_client_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_client_pkt_key = [
            0x1f, 0x36, 0x96, 0x13, 0xdd, 0x76, 0xd5, 0x46, 0x77, 0x30, 0xef,
            0xcb, 0xe3, 0xb1, 0xa2, 0x2d,
        ];
        assert_eq!(&pkt_key, &expected_client_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_client_pkt_iv = [
            0xfa, 0x04, 0x4b, 0x2f, 0x42, 0xa3, 0xfd, 0x3b, 0x46, 0xfb, 0x25,
            0x5c,
        ];
        assert_eq!(&pkt_iv, &expected_client_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_client_hdr_key = [
            0x9f, 0x50, 0x44, 0x9e, 0x04, 0xa0, 0xe8, 0x10, 0x28, 0x3a, 0x1e,
            0x99, 0x33, 0xad, 0xed, 0xd2,
        ];
        assert_eq!(&hdr_key, &expected_client_hdr_key);

        // Server.
        assert!(
            derive_server_initial_secret(&initial_secret, &mut secret).is_ok()
        );
        let expected_server_initial_secret = [
            0x3c, 0x19, 0x98, 0x28, 0xfd, 0x13, 0x9e, 0xfd, 0x21, 0x6c, 0x15,
            0x5a, 0xd8, 0x44, 0xcc, 0x81, 0xfb, 0x82, 0xfa, 0x8d, 0x74, 0x46,
            0xfa, 0x7d, 0x78, 0xbe, 0x80, 0x3a, 0xcd, 0xda, 0x95, 0x1b,
        ];
        assert_eq!(&secret, &expected_server_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_server_pkt_key = [
            0xcf, 0x3a, 0x53, 0x31, 0x65, 0x3c, 0x36, 0x4c, 0x88, 0xf0, 0xf3,
            0x79, 0xb6, 0x06, 0x7e, 0x37,
        ];
        assert_eq!(&pkt_key, &expected_server_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_server_pkt_iv = [
            0x0a, 0xc1, 0x49, 0x3c, 0xa1, 0x90, 0x58, 0x53, 0xb0, 0xbb, 0xa0,
            0x3e,
        ];
        assert_eq!(&pkt_iv, &expected_server_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_server_hdr_key = [
            0xc2, 0x06, 0xb8, 0xd9, 0xb9, 0xf0, 0xf3, 0x76, 0x44, 0x43, 0x0b,
            0x49, 0x0e, 0xea, 0xa3, 0x14,
        ];
        assert_eq!(&hdr_key, &expected_server_hdr_key);
    }

    #[test]
    fn derive_chacha20_secrets() {
        let secret = [
            0x9a, 0xc3, 0x12, 0xa7, 0xf8, 0x77, 0x46, 0x8e, 0xbe, 0x69, 0x42,
            0x27, 0x48, 0xad, 0x00, 0xa1, 0x54, 0x43, 0xf1, 0x82, 0x03, 0xa0,
            0x7d, 0x60, 0x60, 0xf6, 0x88, 0xf3, 0x0f, 0x21, 0x63, 0x2b,
        ];

        let aead = Algorithm::ChaCha20_Poly1305;

        let mut pkt_key = [0; 32];
        let mut pkt_iv = [0; 12];
        let mut hdr_key = [0; 32];

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_pkt_key = [
            0xc6, 0xd9, 0x8f, 0xf3, 0x44, 0x1c, 0x3f, 0xe1, 0xb2, 0x18, 0x20,
            0x94, 0xf6, 0x9c, 0xaa, 0x2e, 0xd4, 0xb7, 0x16, 0xb6, 0x54, 0x88,
            0x96, 0x0a, 0x7a, 0x98, 0x49, 0x79, 0xfb, 0x23, 0xe1, 0xc8,
        ];
        assert_eq!(&pkt_key, &expected_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_pkt_iv = [
            0xe0, 0x45, 0x9b, 0x34, 0x74, 0xbd, 0xd0, 0xe4, 0x4a, 0x41, 0xc1,
            0x44,
        ];
        assert_eq!(&pkt_iv, &expected_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_hdr_key = [
            0x25, 0xa2, 0x82, 0xb9, 0xe8, 0x2f, 0x06, 0xf2, 0x1f, 0x48, 0x89,
            0x17, 0xa4, 0xfc, 0x8f, 0x1b, 0x73, 0x57, 0x36, 0x85, 0x60, 0x85,
            0x97, 0xd0, 0xef, 0xcb, 0x07, 0x6b, 0x0a, 0xb7, 0xa7, 0xa4,
        ];
        assert_eq!(&hdr_key, &expected_hdr_key);
    }
}
