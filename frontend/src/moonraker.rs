use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use bevy::tasks::Task;
use reqwest::blocking::{multipart, Client};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::{Certificate, Identity, StatusCode};
use serde_json::Value;

use crate::config::{MoonrakerNetworkConfig, PrinterNetworkProtocol};

/// Result of testing connectivity to a Moonraker (Klipper) or PrusaLink (Prusa Buddy) printer without printing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoonrakerConnectionReport {
    pub protocol: PrinterNetworkProtocol,
    pub normalized_uri: String,
    pub moonraker_version: String,
    pub klippy_state: String,
    pub hostname: String,
    pub state_message: String,
}

impl MoonrakerConnectionReport {
    pub fn summary_line(&self) -> String {
        let host_part = if self.hostname.is_empty() {
            String::new()
        } else {
            format!(" on '{}'", self.hostname)
        };
        let msg_part = if self.state_message.is_empty()
            || self.state_message.eq_ignore_ascii_case("Printer is ready")
        {
            String::new()
        } else {
            format!(" — {}", self.state_message)
        };
        match self.protocol {
            PrinterNetworkProtocol::Moonraker => format!(
                "Connected{host_part}: Moonraker {}, Klippy '{}'{}",
                self.moonraker_version, self.klippy_state, msg_part
            ),
            PrinterNetworkProtocol::PrusaLink => format!(
                "Connected{host_part}: PrusaLink FW {}, State '{}'{}",
                self.moonraker_version, self.klippy_state, msg_part
            ),
        }
    }
}

/// Live printer status queried from Moonraker (`/printer/objects/query`) or PrusaLink (`/api/v1/status` & `/api/v1/job`).
#[derive(Clone, Debug, PartialEq)]
pub struct MoonrakerPrintStatus {
    /// Printer state: `"standby"`, `"printing"`, `"paused"`, `"complete"`, `"cancelled"`, `"error"`, or `"offline"`.
    pub state: String,
    /// Active G-code filename (empty if idle).
    pub filename: String,
    /// Normalized print progress in `[0.0 ..= 1.0]`.
    pub progress: f32,
    /// Active print duration in seconds.
    pub print_duration_s: f64,
    /// Total elapsed duration in seconds.
    pub total_duration_s: f64,
    /// Slicer estimated total print time in seconds, if known.
    pub slicer_estimated_time_s: Option<f64>,
    /// Computed remaining time (ETA) in seconds when printing or paused.
    pub eta_remaining_s: Option<f64>,
    /// Optional status message or telemetry summary.
    pub message: String,
}

impl MoonrakerPrintStatus {
    pub fn is_printing_or_paused(&self) -> bool {
        self.state.eq_ignore_ascii_case("printing") || self.state.eq_ignore_ascii_case("paused")
    }

    pub fn formatted_eta(&self) -> Option<String> {
        self.eta_remaining_s.map(format_duration_hms)
    }

    pub fn formatted_elapsed(&self) -> String {
        format_duration_hms(self.print_duration_s.max(0.0))
    }
}

/// Result of uploading a G-code file to Moonraker (`POST /server/files/upload`) or PrusaLink (`PUT /api/v1/files/{storage}/{path}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoonrakerUploadReport {
    pub protocol: PrinterNetworkProtocol,
    pub remote_path: String,
    pub bytes_uploaded: usize,
    pub print_started: bool,
}

impl MoonrakerUploadReport {
    pub fn summary_line(&self) -> String {
        let kb = (self.bytes_uploaded as f64) / 1024.0;
        let proto_name = self.protocol.short_label();
        if self.print_started {
            format!(
                "Uploaded '{}' ({kb:.1} KB) to {proto_name} and started print.",
                self.remote_path
            )
        } else {
            format!(
                "Uploaded '{}' ({kb:.1} KB) to {proto_name}.",
                self.remote_path
            )
        }
    }
}

/// Bevy resource holding non-blocking background tasks and latest network printer state for the UI.
#[derive(Resource, Default)]
pub struct MoonrakerUiState {
    pub show_api_key: bool,
    pub show_password: bool,
    pub last_status: Option<Result<MoonrakerPrintStatus, String>>,
    pub last_test_result: Option<Result<MoonrakerConnectionReport, String>>,
    pub last_upload_result: Option<Result<MoonrakerUploadReport, String>>,
    pub pending_status_task: Option<Task<(Result<MoonrakerPrintStatus, String>, Option<(String, Option<f64>)>)>>,
    pub pending_test_task: Option<Task<Result<MoonrakerConnectionReport, String>>>,
    pub pending_upload_task: Option<Task<Result<MoonrakerUploadReport, String>>>,
    pub pending_cert_pick_task: Option<(CertPickKind, Task<Option<String>>)>,
    pub last_poll_instant: Option<Instant>,
    pub cached_metadata_estimate: Option<(String, Option<f64>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertPickKind {
    CaCert,
    ClientCert,
    ClientKey,
}

/// Formats a duration in seconds as `HH:MM:SS` (or `MM:SS` when `< 1 hour`).
pub fn format_duration_hms(seconds: f64) -> String {
    let total_secs = seconds.max(0.0).round() as u64;
    let hrs = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hrs > 0 {
        format!("{hrs:02}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

/// Normalizes a user-configured Moonraker or PrusaLink base URI:
/// - Trims whitespace and trailing `/` characters
/// - Prepends `http://` if no `http://` or `https://` scheme is present
pub fn normalize_moonraker_base_uri(raw_uri: &str) -> Result<String, String> {
    let trimmed = raw_uri.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(
            "Printer URI is empty. Enter a Printer URL such as http://printer.local:7125 or http://prusa-core-one.local"
                .to_string(),
        );
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let after_scheme = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("https://"))
            .unwrap_or("");
        if after_scheme.trim().is_empty() {
            return Err("Printer URI is missing a hostname after http:// or https://".to_string());
        }
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(format!(
            "Unsupported URI scheme in '{trimmed}' (expected http:// or https://)"
        ));
    } else {
        format!("http://{trimmed}")
    };
    Ok(with_scheme)
}

// ============================================================================
// Pure-Rust Cryptographic Helpers for PrusaLink HTTP Digest & Basic Auth
// (RFC 1321 MD5, FIPS 180-4 SHA-256, RFC 4648 Base64, RFC 2617 / RFC 7616)
// ============================================================================

/// Computes the lowercase hexadecimal RFC 1321 MD5 digest of `data`.
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let off = i * 4;
            *word = u32::from_le_bytes([
                chunk[off],
                chunk[off + 1],
                chunk[off + 2],
                chunk[off + 3],
            ]);
        }
        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };
            let temp = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(temp.rotate_left(S[i]));
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// Computes the lowercase hexadecimal FIPS 180-4 SHA-256 digest of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
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

    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            let off = i * 4;
            w[i] = u32::from_be_bytes([
                chunk[off],
                chunk[off + 1],
                chunk[off + 2],
                chunk[off + 3],
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

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        for byte in word.to_be_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// Standard RFC 4648 Base64 encoder for HTTP Basic authentication fallback.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Parsed HTTP `WWW-Authenticate: Digest ...` challenge (RFC 2617 / RFC 7616 / RFC 2069 Buddy `nhttp`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
    pub qop_auth: bool,
    pub stale: bool,
}

impl DigestChallenge {
    /// Parses a `WWW-Authenticate` header value starting with `Digest `.
    pub fn parse(header_val: &str) -> Option<Self> {
        let trimmed = header_val.trim();
        let rest = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("digest ") {
            &trimmed[7..]
        } else {
            return None;
        };

        let params = parse_auth_key_values(rest);
        let realm = params.get("realm")?.clone();
        let nonce = params.get("nonce")?.clone();
        let opaque = params.get("opaque").cloned();
        let algorithm = params.get("algorithm").cloned();
        let qop_auth = params
            .get("qop")
            .is_some_and(|q| q.split(',').any(|tok| tok.trim().eq_ignore_ascii_case("auth")));
        let stale = params
            .get("stale")
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"));

        Some(Self {
            realm,
            nonce,
            opaque,
            algorithm,
            qop_auth,
            stale,
        })
    }

    /// Computes an RFC 2617 / RFC 7616 / RFC 2069 `Authorization: Digest ...` header value.
    pub fn build_authorization_header(
        &self,
        username: &str,
        password: &str,
        method: &str,
        uri_path_and_query: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let alg_upper = self
            .algorithm
            .as_deref()
            .unwrap_or("MD5")
            .to_ascii_uppercase();
        let use_sha256 = alg_upper.starts_with("SHA-256");
        let is_sess = alg_upper.ends_with("-SESS");

        let hash_fn = |input: &str| -> String {
            if use_sha256 {
                sha256_hex(input.as_bytes())
            } else {
                md5_hex(input.as_bytes())
            }
        };

        let ha1_base = hash_fn(&format!("{username}:{}:{password}", self.realm));
        let ha1 = if is_sess {
            hash_fn(&format!("{ha1_base}:{}:{cnonce}", self.nonce))
        } else {
            ha1_base
        };
        let ha2 = hash_fn(&format!("{method}:{uri_path_and_query}"));

        let nc_hex = format!("{nc:08x}");
        let response = if self.qop_auth {
            hash_fn(&format!(
                "{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}",
                self.nonce
            ))
        } else {
            hash_fn(&format!("{ha1}:{}:{ha2}", self.nonce))
        };

        let esc_user = username.replace('\\', "\\\\").replace('"', "\\\"");
        let esc_realm = self.realm.replace('\\', "\\\\").replace('"', "\\\"");
        let esc_nonce = self.nonce.replace('\\', "\\\\").replace('"', "\\\"");
        let esc_uri = uri_path_and_query.replace('"', "%22");

        let mut hdr = format!(
            "Digest username=\"{esc_user}\", realm=\"{esc_realm}\", nonce=\"{esc_nonce}\", uri=\"{esc_uri}\", response=\"{response}\""
        );
        if let Some(ref alg) = self.algorithm {
            hdr.push_str(&format!(", algorithm={alg}"));
        }
        if let Some(ref opq) = self.opaque {
            let esc_opq = opq.replace('\\', "\\\\").replace('"', "\\\"");
            hdr.push_str(&format!(", opaque=\"{esc_opq}\""));
        }
        if self.qop_auth {
            let esc_cnonce = cnonce.replace('\\', "\\\\").replace('"', "\\\"");
            hdr.push_str(&format!(", qop=auth, nc={nc_hex}, cnonce=\"{esc_cnonce}\""));
        }
        hdr
    }
}

/// Parses comma-separated `key="quoted value"` / `key=token` parameters from an HTTP `WWW-Authenticate` header.
fn parse_auth_key_values(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    while i < n {
        while i < n && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        let key_start = i;
        while i < n && bytes[i] != b'=' && bytes[i] != b',' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = input[key_start..i].trim().to_ascii_lowercase();
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if key.is_empty() || i >= n || bytes[i] != b'=' {
            while i < n && bytes[i] != b',' {
                i += 1;
            }
            continue;
        }
        i += 1; // skip '='
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let val = if i < n && bytes[i] == b'"' {
            i += 1; // skip opening '"'
            let mut buf = String::new();
            while i < n {
                if bytes[i] == b'\\' && i + 1 < n {
                    i += 1;
                    buf.push(bytes[i] as char);
                    i += 1;
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    let ch = input[i..].chars().next().unwrap();
                    buf.push(ch);
                    i += ch.len_utf8();
                }
            }
            buf
        } else {
            let val_start = i;
            while i < n && bytes[i] != b',' {
                i += 1;
            }
            input[val_start..i].trim().to_string()
        };

        map.insert(key, val);
    }

    map
}

fn generate_digest_cnonce(nc: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = md5_hex(format!("manifold2:{nanos}:{nc}").as_bytes());
    digest[..16].to_string()
}

/// Cached HTTP Digest challenges per `(normalized_base_uri, username)` so polling & `PUT` file uploads
/// attach a valid preemptive `Authorization: Digest ...` header on the very first transmission.
static CACHED_DIGEST_CHALLENGES: OnceLock<
    Mutex<HashMap<(String, String), (DigestChallenge, u32)>>,
> = OnceLock::new();

fn next_preemptive_digest_header(
    base_uri: &str,
    username: &str,
    password: &str,
    method: &str,
    uri_path_and_query: &str,
) -> Option<String> {
    if password.is_empty() {
        return None;
    }
    let cache = CACHED_DIGEST_CHALLENGES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().ok()?;
    let (challenge, nc) = guard.get_mut(&(base_uri.to_string(), username.to_string()))?;
    *nc = nc.wrapping_add(1).max(1);
    let cur_nc = *nc;
    let cnonce = generate_digest_cnonce(cur_nc);
    Some(challenge.build_authorization_header(
        username,
        password,
        method,
        uri_path_and_query,
        cur_nc,
        &cnonce,
    ))
}

fn store_digest_challenge_and_build_header(
    base_uri: &str,
    username: &str,
    password: &str,
    method: &str,
    uri_path_and_query: &str,
    challenge: DigestChallenge,
) -> String {
    let nc: u32 = 1;
    let cnonce = generate_digest_cnonce(nc);
    let hdr = challenge.build_authorization_header(
        username,
        password,
        method,
        uri_path_and_query,
        nc,
        &cnonce,
    );
    let cache = CACHED_DIGEST_CHALLENGES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            (base_uri.to_string(), username.to_string()),
            (challenge, nc),
        );
    }
    hdr
}

fn extract_uri_path_and_query(full_url: &str) -> String {
    if let Ok(parsed) = reqwest::Url::parse(full_url) {
        match parsed.query() {
            Some(q) => format!("{}?{q}", parsed.path()),
            None => parsed.path().to_string(),
        }
    } else {
        "/".to_string()
    }
}

/// Extracts either a `Digest` challenge or indicates `Basic` challenge from response `WWW-Authenticate` headers.
enum ParsedWwwAuth {
    Digest(DigestChallenge),
    Basic,
}

fn parse_response_www_authenticate(headers: &HeaderMap) -> Option<ParsedWwwAuth> {
    let mut saw_basic = false;
    for val in headers.get_all(WWW_AUTHENTICATE) {
        if let Ok(s) = val.to_str() {
            if let Some(dig) = DigestChallenge::parse(s) {
                return Some(ParsedWwwAuth::Digest(dig));
            }
            if s.trim().len() >= 5 && s.trim()[..5].eq_ignore_ascii_case("basic") {
                saw_basic = true;
            }
        }
    }
    if saw_basic {
        Some(ParsedWwwAuth::Basic)
    } else {
        None
    }
}

/// Executes an HTTP request against PrusaLink with automatic HTTP Digest (RFC 2617 / 7616 / 2069)
/// and HTTP Basic challenge-response negotiation, preemptive Digest nonce reuse, and transient connect retries.
fn send_prusalink_request<F>(
    client: &Client,
    cfg: &MoonrakerNetworkConfig,
    base_uri: &str,
    method_name: &str,
    full_url: &str,
    max_connect_attempts: usize,
    mut build_req: F,
) -> Result<reqwest::blocking::Response, reqwest::Error>
where
    F: FnMut(&Client, &str) -> reqwest::blocking::RequestBuilder,
{
    let username = cfg.effective_prusa_username();
    let password = cfg.password.as_str();
    let uri_pq = extract_uri_path_and_query(full_url);

    let preemptive_auth =
        next_preemptive_digest_header(base_uri, username, password, method_name, &uri_pq);

    let attempts = max_connect_attempts.max(1);
    let mut first_resp = None;
    let mut last_err = None;
    for attempt in 0..attempts {
        let mut rb = build_req(client, full_url);
        if let Some(ref auth_hdr) = preemptive_auth {
            rb = rb.header(AUTHORIZATION, auth_hdr);
        }
        match rb.send() {
            Ok(resp) => {
                first_resp = Some(resp);
                break;
            }
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    let resp = match first_resp {
        Some(r) => r,
        None => return Err(last_err.expect("at least one connect attempt executed")),
    };

    if resp.status() != StatusCode::UNAUTHORIZED || password.is_empty() {
        return Ok(resp);
    }

    // Complete HTTP 401 WWW-Authenticate Digest or Basic challenge
    if let Some(challenge) = parse_response_www_authenticate(resp.headers()) {
        let auth_value = match challenge {
            ParsedWwwAuth::Digest(dig) => store_digest_challenge_and_build_header(
                base_uri, username, password, method_name, &uri_pq, dig,
            ),
            ParsedWwwAuth::Basic => {
                let encoded = base64_encode(format!("{username}:{password}").as_bytes());
                format!("Basic {encoded}")
            }
        };
        let retry_rb = build_req(client, full_url).header(AUTHORIZATION, auth_value);
        return retry_rb.send();
    }

    Ok(resp)
}

fn format_prusalink_http_error(
    status: StatusCode,
    cfg: &MoonrakerNetworkConfig,
    endpoint_desc: &str,
    body_snippet: &str,
) -> String {
    if status == StatusCode::UNAUTHORIZED {
        if cfg.password.is_empty() && cfg.api_key.trim().is_empty() {
            return format!(
                "PrusaLink requires authentication (HTTP 401 on {endpoint_desc}): enter your PrusaLink Password (from Printer Settings -> Network -> PrusaLink; default username is 'maker') or API Key."
            );
        }
        return format!(
            "PrusaLink authentication failed (HTTP 401 on {endpoint_desc}): verify Username ('{}') and Password (from Printer Settings -> Network -> PrusaLink).",
            cfg.effective_prusa_username()
        );
    }
    if body_snippet.trim().is_empty() {
        format!("PrusaLink returned HTTP {status} from {endpoint_desc}")
    } else {
        format!(
            "PrusaLink returned HTTP {status} from {endpoint_desc}: {}",
            body_snippet.trim().chars().take(200).collect::<String>()
        )
    }
}

/// Resolves the Web UI URL (Mainsail / Fluidd / PrusaLink) to open in the system browser:
/// - If `cfg.web_ui_uri` is non-empty, normalizes and returns it.
/// - Otherwise derives it from `cfg.printer_uri` (stripping `:7125` for Moonraker so it points to
///   the standard HTTP/HTTPS web interface port on the printer host).
pub fn resolve_printer_web_ui_url(cfg: &MoonrakerNetworkConfig) -> Result<String, String> {
    if !cfg.web_ui_uri.trim().is_empty() {
        return normalize_moonraker_base_uri(&cfg.web_ui_uri);
    }
    let base = normalize_moonraker_base_uri(&cfg.printer_uri)?;
    if cfg.protocol == PrinterNetworkProtocol::Moonraker {
        if let Some(stripped) = base.strip_suffix(":7125") {
            return Ok(stripped.to_string());
        } else if base.contains(":7125/") {
            return Ok(base.replacen(":7125/", "/", 1));
        }
    }
    Ok(base)
}

/// Launches the resolved printer Web UI URL in the operating system's default web browser.
pub fn open_printer_web_ui_in_browser(cfg: &MoonrakerNetworkConfig) -> Result<String, String> {
    let url = resolve_printer_web_ui_url(cfg)?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("Refusing to open non-HTTP/HTTPS URL: {url}"));
    }

    #[cfg(target_os = "macos")]
    let spawn_res = std::process::Command::new("open").arg(&url).spawn();

    #[cfg(target_os = "linux")]
    let spawn_res = std::process::Command::new("xdg-open").arg(&url).spawn();

    #[cfg(target_os = "windows")]
    let spawn_res = std::process::Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(&url)
        .spawn();

    spawn_res
        .map(|_| url)
        .map_err(|e| format!("Failed to launch system browser: {e}"))
}

/// Formats a `reqwest::Error` with its full underlying `std::error::Error::source()` chain
/// so the user sees the actual OS/socket/TLS error rather than just `"error sending request"`.
pub fn format_reqwest_error(context: &str, err: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut chain = vec![err.to_string()];
    let mut curr = err.source();
    while let Some(src) = curr {
        let s = src.to_string();
        if !chain.iter().any(|prev| prev.contains(&s)) {
            chain.push(s);
        }
        curr = src.source();
    }
    let joined = chain.join(": ");
    if joined.contains("No route to host") || joined.contains("os error 65") {
        format!(
            "{context}: {joined} (On macOS, ensure Local Network access is allowed in System Settings -> Privacy & Security -> Local Network, or check printer IP/Wi-Fi)"
        )
    } else {
        format!("{context}: {joined}")
    }
}

/// Resolves `(host, port)` via OS DNS/mDNS (`ToSocketAddrs`), prioritizing IPv4 ahead of IPv6,
/// and probes TCP connectivity (with a brief retry to wake sleeping Wi-Fi / ARP caches on Raspberry Pi hosts)
/// so `reqwest` is handed a known-reachable `SocketAddr`.
fn resolve_and_probe_host_addrs(host: &str, port: u16) -> Vec<SocketAddr> {
    if host.parse::<IpAddr>().is_ok() {
        return Vec::new();
    }
    let mut all_addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(_) => return Vec::new(),
    };
    if all_addrs.is_empty() {
        return Vec::new();
    }
    // Order IPv4 addresses before IPv6 addresses
    all_addrs.sort_by_key(|a| a.is_ipv6());

    // Probe TCP reachability (up to 2 passes so a sleeping Wi-Fi printer has time to answer ARP/NDP)
    for pass in 0..2 {
        for &addr in &all_addrs {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok() {
                return vec![addr];
            }
        }
        if pass == 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let ipv4_addrs: Vec<SocketAddr> = all_addrs.iter().copied().filter(|a| a.is_ipv4()).collect();
    if !ipv4_addrs.is_empty() {
        ipv4_addrs
    } else {
        all_addrs
    }
}

/// Cached shared `reqwest::blocking::Client` so periodic status polling reuses HTTP keep-alive
/// connections and a single background runtime thread rather than spawning a new runtime every poll.
static CACHED_CLIENT: OnceLock<Mutex<Option<(MoonrakerNetworkConfig, Instant, Client)>>> =
    OnceLock::new();

fn invalidate_cached_moonraker_client() {
    if let Some(lock) = CACHED_CLIENT.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
}

/// Returns a cached `reqwest::blocking::Client` if `cfg` matches and the client was created within
/// the last 60 seconds, or builds a fresh client otherwise.
pub fn get_or_build_moonraker_client(
    cfg: &MoonrakerNetworkConfig,
    timeout: Duration,
) -> Result<Client, String> {
    let cache_mutex = CACHED_CLIENT.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache_mutex.lock() {
        if let Some((ref cached_cfg, created_at, ref client)) = *guard {
            if cached_cfg == cfg && created_at.elapsed() < Duration::from_secs(60) {
                return Ok(client.clone());
            }
        }
    }

    let client = build_moonraker_client(cfg, timeout)?;
    if let Ok(mut guard) = cache_mutex.lock() {
        *guard = Some((cfg.clone(), Instant::now(), client.clone()));
    }
    Ok(client)
}

/// Builds a blocking `reqwest::blocking::Client` configured with optional `X-Api-Key`,
/// custom CA root certificate PEM, and mTLS client certificate + key PEM.
/// Also resolves `.local` / LAN hostnames upfront and orders IPv4 addresses ahead of IPv6
/// so dual-stack mDNS hosts where Moonraker only listens on IPv4 (or where IPv6 routes fail)
/// connect immediately without stalling or failing on unreachable IPv6 addresses.
pub fn build_moonraker_client(
    cfg: &MoonrakerNetworkConfig,
    timeout: Duration,
) -> Result<Client, String> {
    let mut builder = Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .user_agent("Manifold-Slicer/0.1");

    if !cfg.api_key.trim().is_empty() {
        let mut headers = HeaderMap::new();
        let val = HeaderValue::from_str(cfg.api_key.trim())
            .map_err(|e| format!("Invalid characters in Moonraker API key: {e}"))?;
        headers.insert("X-Api-Key", val);
        builder = builder.default_headers(headers);
    }

    // Load optional custom Root / CA certificate(s) from PEM file
    let ca_path = cfg.ca_cert_pem_path.trim();
    if !ca_path.is_empty() {
        let pem_bytes = std::fs::read(ca_path)
            .map_err(|e| format!("Failed to read CA certificate PEM file '{ca_path}': {e}"))?;
        let certs = Certificate::from_pem_bundle(&pem_bytes)
            .or_else(|_| Certificate::from_pem(&pem_bytes).map(|c| vec![c]))
            .map_err(|e| format!("Invalid CA certificate PEM in '{ca_path}': {e}"))?;
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }

    // Load optional mTLS client certificate + private key from PEM file(s)
    let client_cert_path = cfg.client_cert_pem_path.trim();
    let client_key_path = cfg.client_key_pem_path.trim();
    if !client_cert_path.is_empty() {
        let mut combined_pem = std::fs::read(client_cert_path).map_err(|e| {
            format!("Failed to read client certificate PEM file '{client_cert_path}': {e}")
        })?;
        if !client_key_path.is_empty() && client_key_path != client_cert_path {
            if !combined_pem.ends_with(b"\n") {
                combined_pem.push(b'\n');
            }
            let key_bytes = std::fs::read(client_key_path).map_err(|e| {
                format!("Failed to read client private key PEM file '{client_key_path}': {e}")
            })?;
            combined_pem.extend_from_slice(&key_bytes);
        }
        let identity = Identity::from_pem(&combined_pem).map_err(|e| {
            format!(
                "Invalid mTLS client certificate/key PEM ('{client_cert_path}'): {e}. Ensure both the certificate and private key are PEM-encoded."
            )
        })?;
        builder = builder.identity(identity);
    } else if !client_key_path.is_empty() {
        return Err(
            "Client Private Key PEM is set, but Client Certificate PEM path is empty.".to_string(),
        );
    }

    if let Ok(base) = normalize_moonraker_base_uri(&cfg.printer_uri) {
        if let Ok(parsed_url) = reqwest::Url::parse(&base) {
            if let (Some(host), Some(port)) =
                (parsed_url.host_str(), parsed_url.port_or_known_default())
            {
                let chosen = resolve_and_probe_host_addrs(host, port);
                if !chosen.is_empty() {
                    builder = builder.resolve_to_addrs(host, &chosen);
                }
            }
        }
    }

    builder
        .build()
        .map_err(|e| format!("Failed to initialize HTTP/TLS client: {e}"))
}

/// Parses Moonraker `/server/info` and `/printer/info` JSON payloads into a `MoonrakerConnectionReport`.
pub fn parse_connection_info_json(
    normalized_uri: &str,
    server_json: &Value,
    printer_json: Option<&Value>,
) -> Result<MoonrakerConnectionReport, String> {
    let srv_res = server_json
        .get("result")
        .ok_or_else(|| "Moonraker /server/info response missing 'result' object".to_string())?;
    let moonraker_version = srv_res
        .get("moonraker_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let mut klippy_state = srv_res
        .get("klippy_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let mut hostname = String::new();
    let mut state_message = String::new();

    if let Some(p_res) = printer_json.and_then(|v| v.get("result")) {
        if let Some(st) = p_res.get("state").and_then(Value::as_str) {
            klippy_state = st.to_string();
        }
        if let Some(h) = p_res.get("hostname").and_then(Value::as_str) {
            hostname = h.to_string();
        }
        if let Some(msg) = p_res.get("state_message").and_then(Value::as_str) {
            state_message = msg.to_string();
        }
    }

    Ok(MoonrakerConnectionReport {
        protocol: PrinterNetworkProtocol::Moonraker,
        normalized_uri: normalized_uri.to_string(),
        moonraker_version,
        klippy_state,
        hostname,
        state_message,
    })
}

/// Parses PrusaLink `/api/v1/info` (or `/api/version`) and `/api/v1/status` JSON payloads
/// into a `MoonrakerConnectionReport`.
pub fn parse_prusalink_info_and_status_json(
    normalized_uri: &str,
    info_json: &Value,
    status_json: Option<&Value>,
) -> MoonrakerConnectionReport {
    let firmware = info_json
        .get("firmware")
        .or_else(|| info_json.get("server"))
        .or_else(|| info_json.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("PrusaLink v1")
        .to_string();

    let hostname = info_json
        .get("hostname")
        .or_else(|| info_json.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut printer_state = "ready".to_string();
    let mut telemetry_parts = Vec::new();

    if let Some(printer_obj) = status_json.and_then(|v| v.get("printer")) {
        if let Some(raw_st) = printer_obj.get("state").and_then(Value::as_str) {
            printer_state = raw_st.to_ascii_lowercase();
        }
        if let Some(t_noz) = printer_obj.get("temp_nozzle").and_then(Value::as_f64) {
            telemetry_parts.push(format!("Nozzle {t_noz:.1}°C"));
        }
        if let Some(t_bed) = printer_obj.get("temp_bed").and_then(Value::as_f64) {
            telemetry_parts.push(format!("Bed {t_bed:.1}°C"));
        }
    }

    if let Some(serial) = info_json
        .get("serial")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        if telemetry_parts.is_empty() {
            telemetry_parts.push(format!("SN {serial}"));
        }
    }

    MoonrakerConnectionReport {
        protocol: PrinterNetworkProtocol::PrusaLink,
        normalized_uri: normalized_uri.to_string(),
        moonraker_version: firmware,
        klippy_state: printer_state,
        hostname,
        state_message: telemetry_parts.join(", "),
    }
}

/// Sends a GET request with automatic retry on transient connect/ARP/mDNS wakeup errors.
fn send_get_with_retry(
    client: &Client,
    url: &str,
    max_attempts: usize,
) -> Result<reqwest::blocking::Response, reqwest::Error> {
    let attempts = max_attempts.max(1);
    let mut last_err = None;
    for attempt in 0..attempts {
        match client.get(url).send() {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt executed"))
}

/// Tests connectivity to PrusaLink (`/api/v1/info` and `/api/v1/status`) using HTTP Digest / Basic / X-Api-Key.
pub fn test_prusalink_connection(
    cfg: &MoonrakerNetworkConfig,
) -> Result<MoonrakerConnectionReport, String> {
    let base = normalize_moonraker_base_uri(&cfg.printer_uri)?;
    let client = get_or_build_moonraker_client(cfg, Duration::from_secs(8))?;

    let info_url = format!("{base}/api/v1/info");
    let mut info_resp = send_prusalink_request(&client, cfg, &base, "GET", &info_url, 3, |c, u| {
        c.get(u)
    })
    .map_err(|e| {
        invalidate_cached_moonraker_client();
        format_reqwest_error(&format!("Failed to reach {info_url}"), &e)
    })?;

    let mut active_info_url = info_url;
    if info_resp.status() == StatusCode::NOT_FOUND {
        let fallback_url = format!("{base}/api/version");
        if let Ok(fb_resp) =
            send_prusalink_request(&client, cfg, &base, "GET", &fallback_url, 1, |c, u| {
                c.get(u)
            })
        {
            info_resp = fb_resp;
            active_info_url = fallback_url;
        }
    }

    let info_status = info_resp.status();
    if !info_status.is_success() {
        let body = info_resp.text().unwrap_or_default();
        return Err(format_prusalink_http_error(
            info_status,
            cfg,
            &active_info_url,
            &body,
        ));
    }

    let info_json: Value = info_resp
        .json()
        .map_err(|e| format_reqwest_error(&format!("Invalid JSON from {active_info_url}"), &e))?;

    let status_url = format!("{base}/api/v1/status");
    let status_json: Option<Value> =
        send_prusalink_request(&client, cfg, &base, "GET", &status_url, 2, |c, u| c.get(u))
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| r.json().ok());

    Ok(parse_prusalink_info_and_status_json(
        &base,
        &info_json,
        status_json.as_ref(),
    ))
}

/// Tests connectivity to the configured printer (`Moonraker` or `PrusaLink`) without uploading or printing anything.
pub fn test_moonraker_connection(
    cfg: &MoonrakerNetworkConfig,
) -> Result<MoonrakerConnectionReport, String> {
    if cfg.protocol == PrinterNetworkProtocol::PrusaLink {
        return test_prusalink_connection(cfg);
    }

    let base = normalize_moonraker_base_uri(&cfg.printer_uri)?;
    let client = get_or_build_moonraker_client(cfg, Duration::from_secs(8))?;

    let server_url = format!("{base}/server/info");
    let srv_resp = send_get_with_retry(&client, &server_url, 3).map_err(|e| {
        invalidate_cached_moonraker_client();
        format_reqwest_error(&format!("Failed to reach {server_url}"), &e)
    })?;
    if !srv_resp.status().is_success() {
        return Err(format!(
            "Moonraker returned HTTP {} from {server_url}",
            srv_resp.status()
        ));
    }
    let srv_json: Value = srv_resp
        .json()
        .map_err(|e| format_reqwest_error(&format!("Invalid JSON from {server_url}"), &e))?;

    let printer_url = format!("{base}/printer/info");
    let printer_json: Option<Value> = send_get_with_retry(&client, &printer_url, 2)
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json().ok());

    parse_connection_info_json(&base, &srv_json, printer_json.as_ref())
}

/// Computes remaining print time (ETA in seconds) from current progress `[0.0 ..= 1.0]`,
/// active print duration `print_duration_s`, and optional slicer metadata `estimated_time`.
pub fn compute_print_eta_seconds(
    state: &str,
    progress: f32,
    print_duration_s: f64,
    slicer_estimated_time_s: Option<f64>,
) -> Option<f64> {
    if !state.eq_ignore_ascii_case("printing") && !state.eq_ignore_ascii_case("paused") {
        return None;
    }
    let p = (progress as f64).clamp(0.0, 1.0);
    if p >= 0.9999 {
        return Some(0.0);
    }
    let rem_fraction = 1.0 - p;
    let linear_eta = if p >= 0.01 && print_duration_s > 1.0 {
        Some((print_duration_s * rem_fraction) / p)
    } else {
        None
    };
    let metadata_eta = slicer_estimated_time_s
        .filter(|&est| est > 1.0)
        .map(|est| (est * rem_fraction).max(0.0));

    match (metadata_eta, linear_eta) {
        (Some(m_eta), Some(l_eta)) => {
            // Smoothly blend from slicer metadata estimate early in the print toward empirical progress rate
            let w_linear = p.clamp(0.0, 1.0);
            Some((1.0 - w_linear) * m_eta + w_linear * l_eta)
        }
        (Some(m_eta), None) => Some(m_eta),
        (None, Some(l_eta)) => Some(l_eta),
        (None, None) => None,
    }
}

/// Parses Moonraker's `/printer/objects/query?print_stats&virtual_sdcard&display_status` JSON response.
pub fn parse_printer_objects_status_json(
    query_json: &Value,
    slicer_estimated_time_s: Option<f64>,
) -> Result<MoonrakerPrintStatus, String> {
    let status_obj = query_json
        .get("result")
        .and_then(|r| r.get("status"))
        .ok_or_else(|| {
            "Moonraker /printer/objects/query response missing 'result.status'".to_string()
        })?;

    let print_stats = status_obj.get("print_stats");
    let vsd = status_obj.get("virtual_sdcard");
    let disp = status_obj.get("display_status");

    let state = print_stats
        .and_then(|ps| ps.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("standby")
        .to_string();
    let filename = print_stats
        .and_then(|ps| ps.get("filename"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let print_duration_s = print_stats
        .and_then(|ps| ps.get("print_duration"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let total_duration_s = print_stats
        .and_then(|ps| ps.get("total_duration"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let vsd_prog = vsd
        .and_then(|v| v.get("progress"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let disp_prog = disp
        .and_then(|d| d.get("progress"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    let progress = if state.eq_ignore_ascii_case("complete") {
        1.0
    } else {
        vsd_prog.max(disp_prog).clamp(0.0, 1.0)
    };

    let message = disp
        .and_then(|d| d.get("message"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            print_stats
                .and_then(|ps| ps.get("message"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string();

    let eta_remaining_s =
        compute_print_eta_seconds(&state, progress, print_duration_s, slicer_estimated_time_s);

    Ok(MoonrakerPrintStatus {
        state,
        filename,
        progress,
        print_duration_s,
        total_duration_s,
        slicer_estimated_time_s,
        eta_remaining_s,
        message,
    })
}

/// Percent-encodes a filename query parameter for `/server/files/metadata?filename=...`.
fn url_encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a single URL path segment (for PrusaLink `PUT /api/v1/files/{storage}/{filename}`).
fn url_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Maps PrusaLink v1 `printer.state` strings (`IDLE`, `READY`, `PRINTING`, `PAUSED`, `FINISHED`, `STOPPED`, `ERROR`, `ATTENTION`)
/// into Manifold2's normalized status badge states.
fn map_prusalink_state(raw_state: &str) -> String {
    match raw_state.trim().to_ascii_uppercase().as_str() {
        "IDLE" | "READY" | "BUSY" => "standby".to_string(),
        "PRINTING" => "printing".to_string(),
        "PAUSED" => "paused".to_string(),
        "FINISHED" => "complete".to_string(),
        "STOPPED" => "cancelled".to_string(),
        "ERROR" | "ATTENTION" => "error".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Parses PrusaLink `/api/v1/status` and optional `/api/v1/job` JSON payloads into `MoonrakerPrintStatus`.
pub fn parse_prusalink_status_and_job_json(
    status_json: &Value,
    job_json: Option<&Value>,
) -> Result<MoonrakerPrintStatus, String> {
    let printer_obj = status_json
        .get("printer")
        .ok_or_else(|| "PrusaLink /api/v1/status response missing 'printer' object".to_string())?;

    let raw_state = printer_obj
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("IDLE");
    let state = map_prusalink_state(raw_state);

    let status_job = status_json.get("job");
    let progress_pct = status_job
        .and_then(|j| j.get("progress"))
        .and_then(Value::as_f64)
        .or_else(|| job_json.and_then(|j| j.get("progress")).and_then(Value::as_f64))
        .unwrap_or(0.0);

    let progress = if state == "complete" {
        1.0
    } else if progress_pct > 1.0001 {
        (progress_pct / 100.0).clamp(0.0, 1.0) as f32
    } else {
        // Note: PrusaLink reports `progress` as 0..100 (e.g. 42.5 -> 42.5%, 1.0 -> 1.0%).
        // Only treat <= 1.0 as percentage too since PrusaLink OpenAPI defines progress in [0..100].
        (progress_pct / 100.0).clamp(0.0, 1.0) as f32
    };

    let print_duration_s = status_job
        .and_then(|j| j.get("time_printing"))
        .and_then(Value::as_f64)
        .or_else(|| {
            job_json
                .and_then(|j| j.get("time_printing"))
                .and_then(Value::as_f64)
        })
        .unwrap_or(0.0);

    let reported_remaining_s = status_job
        .and_then(|j| j.get("time_remaining"))
        .and_then(Value::as_f64)
        .or_else(|| {
            job_json
                .and_then(|j| j.get("time_remaining"))
                .and_then(Value::as_f64)
        });

    let filename = job_json
        .and_then(|j| j.get("file"))
        .and_then(|f| {
            f.get("display_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| f.get("name").and_then(Value::as_str))
        })
        .unwrap_or("")
        .to_string();

    let eta_remaining_s = if state == "printing" || state == "paused" {
        reported_remaining_s
            .filter(|&rem| rem >= 0.0)
            .or_else(|| compute_print_eta_seconds(&state, progress, print_duration_s, None))
    } else {
        None
    };

    let mut msg_parts = Vec::new();
    if let Some(t_noz) = printer_obj.get("temp_nozzle").and_then(Value::as_f64) {
        if let Some(tgt_noz) = printer_obj
            .get("target_nozzle")
            .and_then(Value::as_f64)
            .filter(|&t| t > 0.5)
        {
            msg_parts.push(format!("Nozzle {t_noz:.0}/{tgt_noz:.0}°C"));
        } else {
            msg_parts.push(format!("Nozzle {t_noz:.0}°C"));
        }
    }
    if let Some(t_bed) = printer_obj.get("temp_bed").and_then(Value::as_f64) {
        if let Some(tgt_bed) = printer_obj
            .get("target_bed")
            .and_then(Value::as_f64)
            .filter(|&t| t > 0.5)
        {
            msg_parts.push(format!("Bed {t_bed:.0}/{tgt_bed:.0}°C"));
        } else {
            msg_parts.push(format!("Bed {t_bed:.0}°C"));
        }
    }

    Ok(MoonrakerPrintStatus {
        state,
        filename,
        progress,
        print_duration_s,
        total_duration_s: print_duration_s,
        slicer_estimated_time_s: reported_remaining_s.map(|rem| print_duration_s + rem),
        eta_remaining_s,
        message: msg_parts.join(" | "),
    })
}

/// Queries PrusaLink (`/api/v1/status` + `/api/v1/job`) for live print status, temperatures, and active filename.
pub fn query_prusalink_print_status(
    cfg: &MoonrakerNetworkConfig,
    cached_metadata: Option<(String, Option<f64>)>,
) -> (
    Result<MoonrakerPrintStatus, String>,
    Option<(String, Option<f64>)>,
) {
    let base = match normalize_moonraker_base_uri(&cfg.printer_uri) {
        Ok(u) => u,
        Err(e) => return (Err(e), cached_metadata),
    };
    let client = match get_or_build_moonraker_client(cfg, Duration::from_secs(8)) {
        Ok(c) => c,
        Err(e) => return (Err(e), cached_metadata),
    };

    let status_url = format!("{base}/api/v1/status");
    let resp = match send_prusalink_request(&client, cfg, &base, "GET", &status_url, 2, |c, u| {
        c.get(u)
    }) {
        Ok(r) => r,
        Err(e) => {
            invalidate_cached_moonraker_client();
            return (
                Err(format_reqwest_error("Offline", &e)),
                cached_metadata,
            );
        }
    };

    let status_code = resp.status();
    if !status_code.is_success() {
        let body = resp.text().unwrap_or_default();
        return (
            Err(format_prusalink_http_error(
                status_code,
                cfg,
                &status_url,
                &body,
            )),
            cached_metadata,
        );
    }

    let status_json: Value = match resp.json() {
        Ok(j) => j,
        Err(e) => {
            return (
                Err(format_reqwest_error("Invalid PrusaLink status JSON", &e)),
                cached_metadata,
            )
        }
    };

    let has_active_job = status_json.get("job").is_some_and(|j| !j.is_null());
    let job_json: Option<Value> = if has_active_job {
        let job_url = format!("{base}/api/v1/job");
        send_prusalink_request(&client, cfg, &base, "GET", &job_url, 1, |c, u| c.get(u))
            .ok()
            .filter(|r| r.status() == StatusCode::OK)
            .and_then(|r| r.json::<Value>().ok())
    } else {
        None
    };

    let parsed = parse_prusalink_status_and_job_json(&status_json, job_json.as_ref());
    (parsed, cached_metadata)
}

/// Queries Moonraker or PrusaLink for live print status.
pub fn query_moonraker_print_status(
    cfg: &MoonrakerNetworkConfig,
    cached_metadata: Option<(String, Option<f64>)>,
) -> (
    Result<MoonrakerPrintStatus, String>,
    Option<(String, Option<f64>)>,
) {
    if cfg.protocol == PrinterNetworkProtocol::PrusaLink {
        return query_prusalink_print_status(cfg, cached_metadata);
    }

    let base = match normalize_moonraker_base_uri(&cfg.printer_uri) {
        Ok(u) => u,
        Err(e) => return (Err(e), cached_metadata),
    };
    let client = match get_or_build_moonraker_client(cfg, Duration::from_secs(8)) {
        Ok(c) => c,
        Err(e) => return (Err(e), cached_metadata),
    };

    let query_url =
        format!("{base}/printer/objects/query?print_stats&virtual_sdcard&display_status");
    let resp = match send_get_with_retry(&client, &query_url, 2) {
        Ok(r) => r,
        Err(e) => {
            invalidate_cached_moonraker_client();
            return (
                Err(format_reqwest_error("Offline", &e)),
                cached_metadata,
            );
        }
    };
    if !resp.status().is_success() {
        return (
            Err(format!("Moonraker HTTP {}", resp.status())),
            cached_metadata,
        );
    }
    let query_json: Value = match resp.json() {
        Ok(j) => j,
        Err(e) => {
            return (
                Err(format_reqwest_error("Invalid status JSON", &e)),
                cached_metadata,
            )
        }
    };

    let prelim = match parse_printer_objects_status_json(&query_json, None) {
        Ok(s) => s,
        Err(e) => return (Err(e), cached_metadata),
    };

    let mut updated_cache = cached_metadata;
    let mut est_time_s: Option<f64> = None;

    if !prelim.filename.is_empty() {
        if let Some((ref cached_name, cached_est)) = updated_cache {
            if cached_name == &prelim.filename {
                est_time_s = cached_est;
            }
        }
        if updated_cache
            .as_ref()
            .is_none_or(|(name, _)| name != &prelim.filename)
        {
            let encoded = url_encode_query_value(&prelim.filename);
            let meta_url = format!("{base}/server/files/metadata?filename={encoded}");
            let fetched_est = client
                .get(&meta_url)
                .send()
                .ok()
                .filter(|r| r.status().is_success())
                .and_then(|r| r.json::<Value>().ok())
                .and_then(|v| {
                    v.get("result")
                        .and_then(|res| res.get("estimated_time"))
                        .and_then(Value::as_f64)
                });
            est_time_s = fetched_est;
            updated_cache = Some((prelim.filename.clone(), fetched_est));
        }
    }

    let final_status = parse_printer_objects_status_json(&query_json, est_time_s);
    (final_status, updated_cache)
}

/// Selects the preferred writable storage target from PrusaLink `/api/v1/storage` JSON
/// (e.g. `"usb"` on Prusa CORE One / MK4 / XL Buddy boards, or `"local"` on RPi PrusaLink).
fn select_prusalink_writable_storage(storage_json: &Value) -> String {
    let entries = storage_json
        .get("storage_list")
        .and_then(Value::as_array)
        .or_else(|| storage_json.as_array());

    if let Some(list) = entries {
        for item in list {
            let available = item
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let read_only = item
                .get("read_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !available || read_only {
                continue;
            }
            if let Some(raw_path) = item
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| item.get("type").and_then(Value::as_str))
            {
                let clean = raw_path.trim_matches('/');
                if !clean.is_empty() {
                    return clean.to_string();
                }
            }
        }
    }
    "usb".to_string()
}

/// Uploads G-code to a PrusaLink printer via `PUT /api/v1/files/{storage}/{filename}`
/// (`Print-After-Upload: ?1` / `?0`, `Overwrite: ?1`), pre-negotiating HTTP Digest auth
/// via `GET /api/v1/storage` and falling back to legacy `POST /api/files/local` on HTTP 404/405.
pub fn upload_gcode_to_prusalink(
    cfg: &MoonrakerNetworkConfig,
    filename: &str,
    gcode_text: String,
    start_print: bool,
) -> Result<MoonrakerUploadReport, String> {
    let base = normalize_moonraker_base_uri(&cfg.printer_uri)?;
    let client = build_moonraker_client(cfg, Duration::from_secs(120))?;

    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("manifold2_print.gcode");
    let clean_filename = if basename.ends_with(".gcode")
        || basename.ends_with(".bgcode")
        || basename.ends_with(".gco")
    {
        basename.to_string()
    } else {
        format!("{basename}.gcode")
    };

    // Step 1: Query `/api/v1/storage` to discover writable storage ("usb" / "local") AND
    // pre-negotiate the HTTP Digest `nonce` before streaming the G-code payload.
    let storage_url = format!("{base}/api/v1/storage");
    let mut storage_target = "usb".to_string();
    if let Ok(st_resp) =
        send_prusalink_request(&client, cfg, &base, "GET", &storage_url, 2, |c, u| c.get(u))
    {
        if st_resp.status() == StatusCode::UNAUTHORIZED {
            let body = st_resp.text().unwrap_or_default();
            return Err(format_prusalink_http_error(
                StatusCode::UNAUTHORIZED,
                cfg,
                &storage_url,
                &body,
            ));
        }
        if st_resp.status().is_success() {
            if let Ok(st_json) = st_resp.json::<Value>() {
                storage_target = select_prusalink_writable_storage(&st_json);
            }
        }
    }

    let gcode_bytes = gcode_text.into_bytes();
    let bytes_uploaded = gcode_bytes.len();
    let encoded_name = url_encode_path_segment(&clean_filename);
    let put_url = format!("{base}/api/v1/files/{storage_target}/{encoded_name}");
    let print_after_hdr = if start_print { "?1" } else { "?0" };

    let put_resp = send_prusalink_request(&client, cfg, &base, "PUT", &put_url, 1, |c, u| {
        c.put(u)
            .header("Content-Type", "text/x.gcode")
            .header("Overwrite", "?1")
            .header("Print-After-Upload", print_after_hdr)
            .body(gcode_bytes.clone())
    })
    .map_err(|e| format_reqwest_error(&format!("Failed to upload G-code to {put_url}"), &e))?;

    let put_status = put_resp.status();
    if put_status.is_success() {
        return Ok(MoonrakerUploadReport {
            protocol: PrinterNetworkProtocol::PrusaLink,
            remote_path: format!("{storage_target}/{clean_filename}"),
            bytes_uploaded,
            print_started: start_print,
        });
    }

    // Legacy OctoPrint-compatible PrusaLink fallback (`POST /api/files/local`) on 404 / 405
    if put_status == StatusCode::NOT_FOUND || put_status == StatusCode::METHOD_NOT_ALLOWED {
        let legacy_url = format!("{base}/api/files/local");
        let legacy_resp =
            send_prusalink_request(&client, cfg, &base, "POST", &legacy_url, 1, |c, u| {
                let part = multipart::Part::bytes(gcode_bytes.clone())
                    .file_name(clean_filename.clone())
                    .mime_str("application/octet-stream")
                    .expect("valid mime");
                let form = multipart::Form::new()
                    .text("select", "true")
                    .text("print", if start_print { "true" } else { "false" })
                    .part("file", part);
                c.post(u).multipart(form)
            })
            .map_err(|e| {
                format_reqwest_error(&format!("Failed to upload G-code to {legacy_url}"), &e)
            })?;

        let legacy_status = legacy_resp.status();
        if legacy_status.is_success() {
            return Ok(MoonrakerUploadReport {
                protocol: PrinterNetworkProtocol::PrusaLink,
                remote_path: clean_filename,
                bytes_uploaded,
                print_started: start_print,
            });
        }
        let body = legacy_resp.text().unwrap_or_default();
        return Err(format_prusalink_http_error(
            legacy_status,
            cfg,
            &legacy_url,
            &body,
        ));
    }

    let body = put_resp.text().unwrap_or_default();
    Err(format_prusalink_http_error(put_status, cfg, &put_url, &body))
}

/// Uploads G-code to a Moonraker (`POST /server/files/upload`) or PrusaLink (`PUT /api/v1/files/{storage}/{path}`)
/// printer and optionally starts printing immediately (`start_print = true`).
pub fn upload_gcode_to_moonraker(
    cfg: &MoonrakerNetworkConfig,
    filename: &str,
    gcode_text: String,
    start_print: bool,
) -> Result<MoonrakerUploadReport, String> {
    if cfg.protocol == PrinterNetworkProtocol::PrusaLink {
        return upload_gcode_to_prusalink(cfg, filename, gcode_text, start_print);
    }

    let base = normalize_moonraker_base_uri(&cfg.printer_uri)?;
    let client = build_moonraker_client(cfg, Duration::from_secs(90))?;

    let clean_filename = if filename.ends_with(".gcode") || filename.ends_with(".nc") {
        filename.to_string()
    } else {
        format!("{filename}.gcode")
    };
    let bytes_uploaded = gcode_text.len();

    let file_part = multipart::Part::bytes(gcode_text.into_bytes())
        .file_name(clean_filename.clone())
        .mime_str("application/octet-stream")
        .map_err(|e| format!("Failed to construct multipart file payload: {e}"))?;

    let form = multipart::Form::new()
        .text("root", "gcodes")
        .text("print", if start_print { "true" } else { "false" })
        .part("file", file_part);

    let upload_url = format!("{base}/server/files/upload");
    let resp = client
        .post(&upload_url)
        .multipart(form)
        .send()
        .map_err(|e| format_reqwest_error(&format!("Failed to upload G-code to {upload_url}"), &e))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!(
            "Moonraker upload failed (HTTP {status}): {}",
            body.chars().take(240).collect::<String>()
        ));
    }

    let resp_json: Value = resp.json().unwrap_or(Value::Null);
    let remote_path = resp_json
        .get("item")
        .and_then(|item| item.get("path"))
        .and_then(Value::as_str)
        .or_else(|| {
            resp_json
                .get("result")
                .and_then(|r| r.get("item"))
                .and_then(|item| item.get("path"))
                .and_then(Value::as_str)
        })
        .unwrap_or(&clean_filename)
        .to_string();

    Ok(MoonrakerUploadReport {
        protocol: PrinterNetworkProtocol::Moonraker,
        remote_path,
        bytes_uploaded,
        print_started: start_print,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn test_normalize_moonraker_uri_and_resolve_web_ui_url() {
        assert_eq!(
            normalize_moonraker_base_uri("http://voron.local:7125/").unwrap(),
            "http://voron.local:7125"
        );
        assert_eq!(
            normalize_moonraker_base_uri("192.168.1.42:7125").unwrap(),
            "http://192.168.1.42:7125"
        );
        assert_eq!(
            normalize_moonraker_base_uri("https://printer.example.com/").unwrap(),
            "https://printer.example.com"
        );
        assert!(normalize_moonraker_base_uri("   ").is_err());

        let mut cfg = MoonrakerNetworkConfig {
            printer_uri: "http://voron.local:7125/".to_string(),
            ..Default::default()
        };
        // Default Web UI strips :7125 so Mainsail/Fluidd opens on standard port 80
        assert_eq!(
            resolve_printer_web_ui_url(&cfg).unwrap(),
            "http://voron.local"
        );

        // Explicit Web UI override takes precedence
        cfg.web_ui_uri = "https://mainsail.example.com:8443/".to_string();
        assert_eq!(
            resolve_printer_web_ui_url(&cfg).unwrap(),
            "https://mainsail.example.com:8443"
        );
    }

    #[test]
    fn test_parse_status_and_eta_calculation() {
        let sample_json: Value = serde_json::from_str(
            r#"{
                "result": {
                    "status": {
                        "print_stats": {
                            "state": "printing",
                            "filename": "manifold_part.gcode",
                            "print_duration": 600.0,
                            "total_duration": 620.0,
                            "message": ""
                        },
                        "virtual_sdcard": {
                            "progress": 0.25
                        },
                        "display_status": {
                            "progress": 0.24,
                            "message": "Layer 12/48"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        // With slicer metadata estimated_time = 2000s at 25% progress (600s elapsed):
        // metadata_eta = 2000 * 0.75 = 1500s
        // linear_eta   = 600 * 0.75 / 0.25 = 1800s
        // blended      = 0.75 * 1500 + 0.25 * 1800 = 1575s
        let status = parse_printer_objects_status_json(&sample_json, Some(2000.0)).unwrap();
        assert_eq!(status.state, "printing");
        assert_eq!(status.filename, "manifold_part.gcode");
        assert!((status.progress - 0.25).abs() < 1.0e-5);
        assert_eq!(status.message, "Layer 12/48");
        let eta = status.eta_remaining_s.expect("Expected ETA while printing");
        assert!((eta - 1575.0).abs() < 1.0e-3, "Expected 1575s ETA, got {eta}");
        assert_eq!(status.formatted_eta().as_deref(), Some("26:15"));
        assert_eq!(status.formatted_elapsed(), "10:00");
    }

    #[test]
    fn test_tls_pem_validation_errors_are_clear() {
        let cfg_missing_ca = MoonrakerNetworkConfig {
            printer_uri: "https://printer.local:7125".to_string(),
            ca_cert_pem_path: "/nonexistent/path/ca_root.pem".to_string(),
            ..Default::default()
        };
        let err = build_moonraker_client(&cfg_missing_ca, Duration::from_secs(5)).unwrap_err();
        assert!(
            err.contains("Failed to read CA certificate PEM file"),
            "Unexpected error: {err}"
        );

        let cfg_orphan_key = MoonrakerNetworkConfig {
            printer_uri: "https://printer.local:7125".to_string(),
            client_key_pem_path: "/some/client.key".to_string(),
            ..Default::default()
        };
        let err_key = build_moonraker_client(&cfg_orphan_key, Duration::from_secs(5)).unwrap_err();
        assert!(err_key.contains("Client Certificate PEM path is empty"));
    }

    #[test]
    fn test_mock_moonraker_server_test_connection_status_and_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server_thread = std::thread::spawn(move || {
            // Handle 5 sequential HTTP requests:
            // 1. GET /server/info
            // 2. GET /printer/info
            // 3. GET /printer/objects/query...
            // 4. GET /server/files/metadata...
            // 5. POST /server/files/upload
            for req_idx in 0..5 {
                let (mut stream, _) = listener.accept().expect("Accept connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = vec![0u8; 65536];
                let mut total_read = 0usize;
                loop {
                    match stream.read(&mut buf[total_read..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            total_read += n;
                            let req_str = String::from_utf8_lossy(&buf[..total_read]);
                            if let Some(hdr_end) = req_str.find("\r\n\r\n") {
                                let headers_part = &req_str[..hdr_end];
                                let content_len = headers_part
                                    .lines()
                                    .find_map(|l| {
                                        let lower = l.to_ascii_lowercase();
                                        lower
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if total_read >= hdr_end + 4 + content_len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let req_text = String::from_utf8_lossy(&buf[..total_read]);
                assert!(
                    req_text.to_ascii_lowercase().contains("x-api-key: secret-test-key"),
                    "Request #{req_idx} missing X-Api-Key header:\n{req_text}"
                );

                let body_json = match req_idx {
                    0 => {
                        assert!(req_text.starts_with("GET /server/info "));
                        r#"{"result":{"klippy_connected":true,"klippy_state":"ready","moonraker_version":"v0.8.0-320"}}"#
                    }
                    1 => {
                        assert!(req_text.starts_with("GET /printer/info "));
                        r#"{"result":{"state":"ready","state_message":"Printer is ready","hostname":"voron-5axis"}}"#
                    }
                    2 => {
                        assert!(req_text.starts_with("GET /printer/objects/query?print_stats&virtual_sdcard&display_status "));
                        r#"{"result":{"status":{"print_stats":{"state":"printing","filename":"test_part.gcode","print_duration":300.0,"total_duration":310.0,"message":""},"virtual_sdcard":{"progress":0.5},"display_status":{"progress":0.5,"message":"Printing"}}}}"#
                    }
                    3 => {
                        assert!(req_text.starts_with("GET /server/files/metadata?filename=test_part.gcode "));
                        r#"{"result":{"filename":"test_part.gcode","estimated_time":600.0}}"#
                    }
                    4 => {
                        assert!(req_text.starts_with("POST /server/files/upload "));
                        assert!(req_text.contains("name=\"print\""));
                        assert!(req_text.contains("true"));
                        assert!(req_text.contains("G1 X10 Y20 Z0.2 E0.5"));
                        r#"{"item":{"path":"test_upload.gcode","root":"gcodes"},"print_started":true}"#
                    }
                    _ => unreachable!(),
                };

                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_json.len(),
                    body_json
                );
                stream.write_all(http_resp.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let cfg = MoonrakerNetworkConfig {
            printer_uri: format!("http://{addr}"),
            api_key: "secret-test-key".to_string(),
            ..Default::default()
        };

        // 1. Test connection
        let conn = test_moonraker_connection(&cfg).expect("Connection test should succeed");
        assert_eq!(conn.moonraker_version, "v0.8.0-320");
        assert_eq!(conn.klippy_state, "ready");
        assert_eq!(conn.hostname, "voron-5axis");

        // 2. Query status + metadata ETA
        let (status_res, cache) = query_moonraker_print_status(&cfg, None);
        let status = status_res.expect("Status query should succeed");
        assert_eq!(status.state, "printing");
        assert_eq!(status.filename, "test_part.gcode");
        assert!((status.progress - 0.5).abs() < 1.0e-5);
        assert!((status.eta_remaining_s.unwrap() - 300.0).abs() < 1.0e-3);
        assert_eq!(cache, Some(("test_part.gcode".to_string(), Some(600.0))));

        // 3. Upload & start print
        let upload = upload_gcode_to_moonraker(
            &cfg,
            "test_upload.gcode",
            "; Manifold2 test\nG1 X10 Y20 Z0.2 E0.5\n".to_string(),
            true,
        )
        .expect("Upload should succeed");
        assert_eq!(upload.protocol, PrinterNetworkProtocol::Moonraker);
        assert_eq!(upload.remote_path, "test_upload.gcode");
        assert!(upload.print_started);

        server_thread.join().unwrap();
    }

    #[test]
    fn test_md5_sha256_and_rfc2617_digest_auth_vectors() {
        // RFC 1321 MD5 standard vectors
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );

        // FIPS 180-4 SHA-256 standard vectors
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // RFC 2617 Section 3.5 canonical HTTP Digest vector:
        // username="Mufasa", password="Circle Of Life", method="GET", uri="/dir/index.html"
        let www_auth = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let challenge = DigestChallenge::parse(www_auth).expect("Should parse Digest header");
        assert_eq!(challenge.realm, "testrealm@host.com");
        assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert!(challenge.qop_auth);

        let auth_hdr = challenge.build_authorization_header(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert!(
            auth_hdr.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "Unexpected RFC 2617 Digest response: {auth_hdr}"
        );
        assert!(auth_hdr.contains("qop=auth, nc=00000001, cnonce=\"0a4f113b\""));

        // Prusa Buddy `nhttp` RFC 2069 no-qop Digest challenge
        let buddy_www = r#"Digest realm="Printer API", nonce="65f8a21b0042c901""#;
        let buddy_ch = DigestChallenge::parse(buddy_www).expect("Should parse Buddy Digest header");
        assert!(!buddy_ch.qop_auth);
        let buddy_hdr = buddy_ch.build_authorization_header(
            "maker",
            "prusa12345",
            "GET",
            "/api/v1/info",
            1,
            "unused",
        );
        let expected_ha1 = md5_hex(b"maker:Printer API:prusa12345");
        let expected_ha2 = md5_hex(b"GET:/api/v1/info");
        let expected_resp =
            md5_hex(format!("{expected_ha1}:65f8a21b0042c901:{expected_ha2}").as_bytes());
        assert!(buddy_hdr.contains(&format!("response=\"{expected_resp}\"")));
        assert!(!buddy_hdr.contains("qop="));
    }

    #[test]
    fn test_mock_prusalink_v1_digest_auth_connection_status_and_put_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server_thread = std::thread::spawn(move || {
            let realm = "Printer API";
            let nonce = "a1b2c3d4e5f60718";
            let expected_user = "maker";
            let expected_pass = "coreone-secret-pw";

            let mut handled_authenticated = 0usize;
            while handled_authenticated < 6 {
                let (mut stream, _) = listener.accept().expect("Accept PrusaLink connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = vec![0u8; 65536];
                let mut total_read = 0usize;
                loop {
                    match stream.read(&mut buf[total_read..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            total_read += n;
                            let req_str = String::from_utf8_lossy(&buf[..total_read]);
                            if let Some(hdr_end) = req_str.find("\r\n\r\n") {
                                let headers_part = &req_str[..hdr_end];
                                let content_len = headers_part
                                    .lines()
                                    .find_map(|l| {
                                        let lower = l.to_ascii_lowercase();
                                        lower
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if total_read >= hdr_end + 4 + content_len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }

                let req_text = String::from_utf8_lossy(&buf[..total_read]);
                let first_line = req_text.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                // Verify Digest Authorization header; if missing, issue 401 WWW-Authenticate: Digest
                let auth_line = req_text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"));

                let is_authorized = if let Some(line) = auth_line {
                    let val = line["authorization:".len()..].trim();
                    if let Some(rest) = val.strip_prefix("Digest ") {
                        let params = parse_auth_key_values(rest);
                        let u = params.get("username").map(String::as_str).unwrap_or("");
                        let nc_val = params.get("nc").map(String::as_str).unwrap_or("");
                        let cnonce_val = params.get("cnonce").map(String::as_str).unwrap_or("");
                        let resp_val = params.get("response").map(String::as_str).unwrap_or("");
                        let ha1 = md5_hex(format!("{expected_user}:{realm}:{expected_pass}").as_bytes());
                        let ha2 = md5_hex(format!("{method}:{path}").as_bytes());
                        let expected_digest = md5_hex(
                            format!("{ha1}:{nonce}:{nc_val}:{cnonce_val}:auth:{ha2}").as_bytes(),
                        );
                        u == expected_user && resp_val == expected_digest
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !is_authorized {
                    let unauth_resp = format!(
                        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"{realm}\", nonce=\"{nonce}\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    stream.write_all(unauth_resp.as_bytes()).unwrap();
                    stream.flush().unwrap();
                    continue;
                }

                let (status_line, body_json) = match (method, path) {
                    ("GET", "/api/v1/info") => (
                        "200 OK",
                        r#"{"firmware":"6.3.2+8912","hostname":"prusa-core-one-indx","serial":"CZPX123456","nozzle_diameter":0.4,"mmu":false}"#,
                    ),
                    ("GET", "/api/v1/status") => (
                        "200 OK",
                        r#"{"printer":{"state":"PRINTING","temp_nozzle":225.0,"target_nozzle":225.0,"temp_bed":60.0,"target_bed":60.0},"job":{"id":42,"progress":40.0,"time_remaining":900,"time_printing":600}}"#,
                    ),
                    ("GET", "/api/v1/job") => (
                        "200 OK",
                        r#"{"id":42,"state":"PRINTING","progress":40.0,"time_remaining":900,"time_printing":600,"file":{"name":"BENCHY~1.GCO","display_name":"core_one_benchy.gcode"}}"#,
                    ),
                    ("GET", "/api/v1/storage") => (
                        "200 OK",
                        r#"{"storage_list":[{"path":"/usb/","name":"usb","type":"USB","read_only":false,"available":true}]}"#,
                    ),
                    ("PUT", "/api/v1/files/usb/test_upload.gcode") => {
                        let lower_req = req_text.to_ascii_lowercase();
                        assert!(
                            lower_req.contains("overwrite: ?1"),
                            "Missing Overwrite: ?1 header:\n{req_text}"
                        );
                        assert!(
                            lower_req.contains("print-after-upload: ?1"),
                            "Missing Print-After-Upload: ?1 header:\n{req_text}"
                        );
                        assert!(
                            req_text.contains("G1 X50 Y50 Z0.2 E1.25"),
                            "Missing G-code payload in PUT body:\n{req_text}"
                        );
                        ("201 Created", r#"{"name":"TEST_U~1.GCO","display_name":"test_upload.gcode"}"#)
                    }
                    other => panic!("Unexpected PrusaLink request: {other:?}"),
                };

                handled_authenticated += 1;
                let http_resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_json.len(),
                    body_json
                );
                stream.write_all(http_resp.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let cfg = MoonrakerNetworkConfig {
            protocol: PrinterNetworkProtocol::PrusaLink,
            printer_uri: format!("http://{addr}"),
            username: String::new(), // blank -> should automatically default to "maker"
            password: "coreone-secret-pw".to_string(),
            ..Default::default()
        };

        // 1. Test PrusaLink connection (performs initial 401 -> Digest auth handshake + /api/v1/info + /api/v1/status)
        let conn = test_moonraker_connection(&cfg).expect("PrusaLink connection test should succeed");
        assert_eq!(conn.protocol, PrinterNetworkProtocol::PrusaLink);
        assert_eq!(conn.moonraker_version, "6.3.2+8912");
        assert_eq!(conn.klippy_state, "printing");
        assert_eq!(conn.hostname, "prusa-core-one-indx");
        assert!(conn.summary_line().contains("PrusaLink FW 6.3.2+8912"));

        // 2. Query PrusaLink live status + job filename + ETA (/api/v1/status + /api/v1/job)
        let (status_res, _) = query_moonraker_print_status(&cfg, None);
        let status = status_res.expect("PrusaLink status query should succeed");
        assert_eq!(status.state, "printing");
        assert_eq!(status.filename, "core_one_benchy.gcode");
        assert!((status.progress - 0.40).abs() < 1.0e-5);
        assert_eq!(status.eta_remaining_s, Some(900.0));
        assert_eq!(status.formatted_eta().as_deref(), Some("15:00"));
        assert!(status.message.contains("Nozzle 225/225°C"));

        // 3. Upload G-code & start print (/api/v1/storage + PUT /api/v1/files/usb/test_upload.gcode)
        let upload = upload_gcode_to_moonraker(
            &cfg,
            "test_upload.gcode",
            "; Prusa CORE One INDX test\nG1 X50 Y50 Z0.2 E1.25\n".to_string(),
            true,
        )
        .expect("PrusaLink PUT upload should succeed");
        assert_eq!(upload.protocol, PrinterNetworkProtocol::PrusaLink);
        assert_eq!(upload.remote_path, "usb/test_upload.gcode");
        assert!(upload.print_started);
        assert!(upload.summary_line().contains("to PrusaLink and started print"));

        server_thread.join().unwrap();
    }
}
