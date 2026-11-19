//! The channel cryptography: SHA-256, HKDF-SHA256, and ChaCha20-Poly1305.
//!
//! Three primitives from their RFCs — FIPS 180-4, RFC 2104/5869, RFC 8439 —
//! each pinned to the RFCs' own test vectors in the tests at the bottom of
//! this file. Nothing here invents a primitive. What is custom is the framing
//! around them, and the honest limits of that framing live in the crate
//! documentation: the channel implements TLS-1.3-PSK *record-layer semantics*
//! (a pre-shared key, an HKDF key schedule, AEAD sealing with per-direction
//! keys, sequence-number replay rejection) without certificates or
//! negotiation. It is not TLS, and the guide says to front it with a
//! TLS-terminating proxy on untrusted networks rather than pretend otherwise.
//! The zero-dependency rule that every previous phase kept is why these three
//! primitives live here instead of behind `rustls`.

// ---------------------------------------------------------------- SHA-256
//
// FIPS 180-4, written straight from the spec: the message schedule in place,
// the compression function over the 64 rounds.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `data`, one-shot.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);

    let mut compress = |chunk: &[u8]| {
        debug_assert_eq!(chunk.len(), 64);
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    };

    let mut chunks = data.chunks_exact(64);
    for chunk in &mut chunks {
        compress(chunk);
    }
    let rem = chunks.remainder();
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let tail_len = if rem.len() < 56 { 64 } else { 128 };
    tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_be_bytes());
    compress(&tail[..64]);
    if tail_len == 128 {
        compress(&tail[64..]);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------- HMAC + HKDF
//
// RFC 2104 and RFC 5869. HMAC's key is at most one block (32 bytes), so the
// ipad/opad construction applies directly.

pub(crate) fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend(std::iter::repeat(0x36u8).take(64));
    inner[..64].iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
    inner.extend_from_slice(msg);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(96);
    outer.extend(std::iter::repeat(0x5cu8).take(64));
    outer[..64].iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

pub(crate) fn hkdf_extract(psk: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    if salt.len() > 32 {
        key = sha256(salt);
    } else {
        key[..salt.len()].copy_from_slice(salt);
    }
    hmac_sha256(&key, psk)
}

pub(crate) fn hkdf_expand(prk: &[u8; 32], info: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len);
    let mut t: [u8; 32] = [0u8; 32];
    let mut counter: u8 = 1;
    while out.len() < out_len {
        let mut input = Vec::with_capacity(32 + info.len() + 1);
        if counter > 1 {
            input.extend_from_slice(&t);
        }
        input.extend_from_slice(info);
        input.push(counter);
        t = hmac_sha256(prk, &input);
        let take = (out_len - out.len()).min(32);
        out.extend_from_slice(&t[..take]);
        counter = counter.wrapping_add(1);
    }
    out
}

// ------------------------------------------------- ChaCha20 (RFC 8439 §2.4)

fn chacha20_block(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> [u8; 64] {
    fn quarter(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        state[a] = state[a].wrapping_add(state[b]);
        state[d] = (state[d] ^ state[a]).rotate_left(16);
        state[c] = state[c].wrapping_add(state[d]);
        state[b] = (state[b] ^ state[c]).rotate_left(12);
        state[a] = state[a].wrapping_add(state[b]);
        state[d] = (state[d] ^ state[a]).rotate_left(8);
        state[c] = state[c].wrapping_add(state[d]);
        state[b] = (state[b] ^ state[c]).rotate_left(7);
    }

    let mut state = [0u32; 16];
    state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        state[4 + i] = u32::from_le_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes([
            nonce[i * 4],
            nonce[i * 4 + 1],
            nonce[i * 4 + 2],
            nonce[i * 4 + 3],
        ]);
    }

    let mut working = state;
    for _ in 0..10 {
        quarter(&mut working, 0, 4, 8, 12);
        quarter(&mut working, 1, 5, 9, 13);
        quarter(&mut working, 2, 6, 10, 14);
        quarter(&mut working, 3, 7, 11, 15);
        quarter(&mut working, 0, 5, 10, 15);
        quarter(&mut working, 1, 6, 11, 12);
        quarter(&mut working, 2, 7, 8, 13);
        quarter(&mut working, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        let word = working[i].wrapping_add(state[i]);
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], data: &mut [u8], start_counter: u32) {
    for (block_index, chunk) in data.chunks_mut(64).enumerate() {
        let keystream = chacha20_block(key, nonce, start_counter.wrapping_add(block_index as u32));
        for (b, k) in chunk.iter_mut().zip(keystream.iter()) {
            *b ^= k;
        }
    }
}

// ------------------------------------------------- Poly1305 (RFC 8439 §2.5)
//
// The standard five-limb (26-bit) formulation; every product stays inside u64
// and partial reductions fold the high bits times 5, so no intermediate ever
// needs more than 64 bits. This is the arithmetic poly1305-donna popularised,
// checked here against the RFC's own vector.

pub(crate) fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    const M: u64 = (1 << 26) - 1;

    // Clamp: top nibble cleared in bytes 3/7/11/15, bottom two bits cleared
    // in bytes 4/8/12 — per limb, byte 4 of t0 and bytes 0/4 of t1 carry the
    // bottom-bit masks.
    let t0 = u64::from_le_bytes(key[0..8].try_into().unwrap()) & 0x0fff_fffc_0fff_ffff;
    let t1 = u64::from_le_bytes(key[8..16].try_into().unwrap()) & 0x0fff_fffc_0fff_fffc;
    let r0 = t0 & M;
    let r1 = (t0 >> 26) & M;
    let r2 = ((t0 >> 52) | (t1 << 12)) & M;
    let r3 = (t1 >> 14) & M;
    let r4 = t1 >> 40; // 24 bits after clamping
    let s0 = u64::from_le_bytes(key[16..24].try_into().unwrap());
    let s1 = u64::from_le_bytes(key[24..32].try_into().unwrap());

    let mut h = [0u64; 5];
    for block in msg.chunks(16) {
        // Absorb h += m: full 16-byte blocks carry the implicit hibit at
        // 1<<24; a short final block appends 0x01 and is zero-padded instead.
        let final_block = block.len() < 16;
        let mut b = [0u8; 16];
        b[..block.len()].copy_from_slice(block);
        if final_block {
            b[block.len()] = 1;
        }
        let w0 = u32::from_le_bytes(b[0..4].try_into().unwrap()) as u64;
        let w1 = u32::from_le_bytes(b[4..8].try_into().unwrap()) as u64;
        let w2 = u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64;
        let w3 = u32::from_le_bytes(b[12..16].try_into().unwrap()) as u64;

        h[0] += w0 & M;
        h[1] += ((w0 >> 26) | (w1 << 6)) & M;
        h[2] += ((w1 >> 20) | (w2 << 12)) & M;
        h[3] += ((w2 >> 14) | (w3 << 18)) & M;
        h[4] += (w3 >> 8) & M;
        if !final_block {
            h[4] += 1 << 24;
        }

        // Multiply h * r mod 2^130 - 5: r's high limbs wrap multiplied by 5.
        let d0 = h[0] * r0 + h[1] * (5 * r4) + h[2] * (5 * r3) + h[3] * (5 * r2) + h[4] * (5 * r1);
        let d1 = h[0] * r1 + h[1] * r0 + h[2] * (5 * r4) + h[3] * (5 * r3) + h[4] * (5 * r2);
        let d2 = h[0] * r2 + h[1] * r1 + h[2] * r0 + h[3] * (5 * r4) + h[4] * (5 * r3);
        let d3 = h[0] * r3 + h[1] * r2 + h[2] * r1 + h[3] * r0 + h[4] * (5 * r4);
        let d4 = h[0] * r4 + h[1] * r3 + h[2] * r2 + h[3] * r1 + h[4] * r0;

        let mut c = d0 >> 26;
        h[0] = d0 & M;
        let d1 = d1 + c;
        c = d1 >> 26;
        h[1] = d1 & M;
        let d2 = d2 + c;
        c = d2 >> 26;
        h[2] = d2 & M;
        let d3 = d3 + c;
        c = d3 >> 26;
        h[3] = d3 & M;
        let d4 = d4 + c;
        c = d4 >> 26;
        h[4] = d4 & M;
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= M;
        h[1] += c;
    }

    // Full carry, then one conditional subtract of p = 2^130 - 5 via the
    // h + 5 - 2^130 trick (limb4 is 24 bits, so 2^130 is limb4's 1<<26).
    let mut c = h[1] >> 26;
    h[1] &= M;
    h[2] += c;
    c = h[2] >> 26;
    h[2] &= M;
    h[3] += c;
    c = h[3] >> 26;
    h[3] &= M;
    h[4] += c;
    c = h[4] >> 26;
    h[4] &= M;
    h[0] += c * 5;
    c = h[0] >> 26;
    h[0] &= M;
    h[1] += c;

    let mut g = [0u64; 5];
    g[0] = h[0] + 5;
    c = g[0] >> 26;
    g[0] &= M;
    g[1] = h[1] + c;
    c = g[1] >> 26;
    g[1] &= M;
    g[2] = h[2] + c;
    c = g[2] >> 26;
    g[2] &= M;
    g[3] = h[3] + c;
    c = g[3] >> 26;
    g[3] &= M;
    // The subtraction borrows on purpose: g[4]'s top bit then tells whether
    // h < p. wrapping_sub keeps that signal in debug builds.
    g[4] = (h[4] + c).wrapping_sub(1 << 26);

    // g4 borrows (top bit set) exactly when h < p: then keep h, else keep g.
    let mask = (g[4] >> 63).wrapping_sub(1);
    for i in 0..5 {
        g[i] &= mask;
    }
    let not_mask = !mask;
    for i in 0..5 {
        h[i] = (h[i] & not_mask) | g[i];
    }

    // h += s, rippling the carries, then serialize four 32-bit words.
    h[0] += s0 & M;
    c = h[0] >> 26;
    h[0] &= M;
    h[1] += (s0 >> 26) & M;
    h[1] += c;
    c = h[1] >> 26;
    h[1] &= M;
    h[2] += ((s0 >> 52) | (s1 << 12)) & M;
    h[2] += c;
    c = h[2] >> 26;
    h[2] &= M;
    h[3] += (s1 >> 14) & M;
    h[3] += c;
    c = h[3] >> 26;
    h[3] &= M;
    h[4] += s1 >> 40;
    h[4] += c;

    let mut out = [0u8; 16];
    let words = [
        ((h[0] | (h[1] << 26)) & 0xffff_ffff) as u32,
        (((h[1] >> 6) | (h[2] << 20)) & 0xffff_ffff) as u32,
        (((h[2] >> 12) | (h[3] << 14)) & 0xffff_ffff) as u32,
        (((h[3] >> 18) | (h[4] << 8)) & 0xffff_ffff) as u32,
    ];
    for (i, word) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

// ------------------------------------------- ChaCha20-Poly1305 AEAD (§2.8)
//
// `aead_seal`/`aead_open` are the RFC 8439 construction with an explicit
// nonce and AAD. The record layer derives both from the sequence number, but
// keeping them parameters here is what lets the tests pin the RFC vector.

fn mac_data(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(aad.len() + ciphertext.len() + 32);
    out.extend_from_slice(aad);
    out.extend(std::iter::repeat(0u8).take((16 - aad.len() % 16) % 16));
    out.extend_from_slice(ciphertext);
    out.extend(std::iter::repeat(0u8).take((16 - ciphertext.len() % 16) % 16));
    out.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    out.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    out
}

pub(crate) fn aead_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let poly_key: [u8; 32] = chacha20_block(key, nonce, 0)[..32].try_into().unwrap();
    let mut ciphertext = plaintext.to_vec();
    chacha20_xor(key, nonce, &mut ciphertext, 1);
    let tag = poly1305(&poly_key, &mac_data(aad, &ciphertext));
    let mut out = Vec::with_capacity(16 + ciphertext.len());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&ciphertext);
    out
}

pub(crate) fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    if sealed.len() < 16 {
        return None;
    }
    let (tag, ciphertext) = sealed.split_at(16);
    let poly_key: [u8; 32] = chacha20_block(key, nonce, 0)[..32].try_into().unwrap();
    let expected = poly1305(&poly_key, &mac_data(aad, ciphertext));
    if !constant_time_eq(tag, &expected) {
        return None;
    }
    let mut plaintext = ciphertext.to_vec();
    chacha20_xor(key, nonce, &mut plaintext, 1);
    Some(plaintext)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ------------------------------------------------------- channel key schedule

/// Derive the two directional record keys from a PSK and the handshake
/// nonces. The layout is TLS-1.3-PSK-shaped: extract with an empty salt, one
/// expand with a transcript-shaped info string, 64 bytes split c2s / s2c.
pub(crate) fn channel_keys(psk: &[u8; 32], client_nonce: &[u8; 12], server_nonce: &[u8; 12]) -> ([u8; 32], [u8; 32]) {
    let prk = hkdf_extract(psk, &[]);
    let mut info = Vec::with_capacity(24 + 24);
    info.extend_from_slice(b"omni-cluster channel v1");
    info.extend_from_slice(client_nonce);
    info.extend_from_slice(server_nonce);
    let okm = hkdf_expand(&prk, &info, 64);
    let c2s: [u8; 32] = okm[..32].try_into().unwrap();
    let s2c: [u8; 32] = okm[32..].try_into().unwrap();
    (c2s, s2c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // FIPS 180-4 examples (§7.1 onwards).
    #[test]
    fn sha256_vectors() {
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex_encode(&sha256(&million_a)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        // A 55/56/64-byte boundary sweep to exercise both tail shapes.
        let base = vec![b'x'; 55];
        for len in [55usize, 56, 63, 64, 65, 119, 120, 128] {
            let input = vec![b'x'; len];
            let _ = sha256(&input);
        }
        let _ = base;
        assert_eq!(
            hex_encode(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    // RFC 5869 case 1 (SHA-256).
    #[test]
    fn hkdf_rfc5869_case1() {
        let ikm = [0x0bu8; 22];
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let prk = hkdf_extract(&ikm, &salt);
        assert_eq!(
            hex_encode(&prk),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        let okm = hkdf_expand(&prk, &info, 42);
        assert_eq!(
            hex_encode(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    // RFC 8439 §2.3.2: one ChaCha20 block at counter 1.
    #[test]
    fn chacha20_block_vector() {
        let key: [u8; 32] = (0..32u8).collect::<Vec<_>>().try_into().unwrap();
        let nonce: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        let block = chacha20_block(&key, &nonce, 1);
        let expect = hex(
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4ed2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
        );
        assert_eq!(block.to_vec(), expect);
    }

    // RFC 8439 §2.5.2: the Poly1305 standalone vector.
    #[test]
    fn poly1305_vector() {
        let key = hex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        let msg = b"Cryptographic Forum Research Group";
        let tag = poly1305(key[..32].try_into().unwrap(), msg);
        assert_eq!(hex_encode(&tag), "a8061dc1305136c6c22b8baf0c0127a9");
    }

    // RFC 8439 §2.8.2: the AEAD vector, including the AAD.
    #[test]
    fn aead_rfc8439_vector() {
        let key = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let nonce = hex("070000004041424344454647");
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let sealed = aead_seal(key[..32].try_into().unwrap(), nonce[..12].try_into().unwrap(), &aad, plaintext);
        let expect_ct = hex("d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116");
        assert_eq!(sealed[16..], expect_ct[..]);
        assert_eq!(
            hex_encode(&sealed[..16]),
            "1ae10b594f09e26a7e902ecbd0600691"
        );
        let opened = aead_open(
            key[..32].try_into().unwrap(),
            nonce[..12].try_into().unwrap(),
            &aad,
            &sealed,
        );
        assert_eq!(opened.unwrap(), plaintext.to_vec());
    }

    #[test]
    fn aead_rejects_tampering_and_wrong_aad() {
        let key = [7u8; 32];
        let nonce = [9u8; 12];
        let mut sealed = aead_seal(&key, &nonce, b"aad", b"payload");
        assert_eq!(
            aead_open(&key, &nonce, b"aad", &sealed).unwrap(),
            b"payload".to_vec()
        );
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(aead_open(&key, &nonce, b"aad", &sealed).is_none());
        let sealed = aead_seal(&key, &nonce, b"aad", b"payload");
        assert!(aead_open(&key, &nonce, b"other", &sealed).is_none());
    }

    #[test]
    fn channel_keys_are_directional_and_psk_bound() {
        let psk = [1u8; 32];
        let c = [2u8; 12];
        let s = [3u8; 12];
        let (c2s, s2c) = channel_keys(&psk, &c, &s);
        let (c2s2, _s2c2) = channel_keys(&psk, &c, &s);
        assert_eq!(c2s, c2s2);
        assert_ne!(c2s, s2c);
        let other = channel_keys(&[4u8; 32], &c, &s);
        assert_ne!(c2s, other.0);
        let flipped = channel_keys(&psk, &s, &c);
        assert_ne!(c2s, flipped.0);
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
