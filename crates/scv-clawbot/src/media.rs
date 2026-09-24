//! Files on WeChat's iLink CDN: parsing the media items of a message,
//! downloading and decrypting them, and uploading files to send.
//!
//! The CDN stores every file encrypted with AES-128-ECB (PKCS#7 padding)
//! under a key the message carries. Uploads ask `getuploadurl` for a CDN
//! address, post the encrypted bytes there, and send a message that points
//! at the CDN's reply parameter with the key.

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use md5::Md5;
use scv_channels::{Media, MediaKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// The CDN iLink files live on, when a message gives no full URL.
pub const CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// iLink item types.
pub const TEXT: i64 = 1;
pub const IMAGE: i64 = 2;
pub const VOICE: i64 = 3;
pub const FILE: i64 = 4;
pub const VIDEO: i64 = 5;

/// `getuploadurl` media types.
const UPLOAD_IMAGE: u8 = 1;
const UPLOAD_VIDEO: u8 = 2;
const UPLOAD_FILE: u8 = 3;

/// Where one CDN file is and how to decrypt it; kept as a message's media
/// source until its turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// The CDN's full download URL, when the message had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The encrypted query parameter the download URL is built from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub param: String,
    /// The raw 16-byte AES key, base64; absent for a plain file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// The media of one iLink message item, or `None` for text and anything
/// without a file.
pub fn item_media(item: &Value) -> Option<Media> {
    let kind = item.get("type").and_then(Value::as_i64)?;
    let (field, kind) = match kind {
        IMAGE => ("image_item", MediaKind::Image),
        VOICE => ("voice_item", MediaKind::Audio),
        FILE => ("file_item", MediaKind::File),
        VIDEO => ("video_item", MediaKind::Video),
        _ => return None,
    };
    let body = item.get(field)?;
    let cdn = body.get("media")?;
    let url = cdn
        .get("full_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    let param = cdn
        .get("encrypt_query_param")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if url.is_none() && param.is_empty() {
        return None;
    }
    // Images may carry the key as hex beside the media; it wins.
    let key = body
        .get("aeskey")
        .and_then(Value::as_str)
        .and_then(key_from_hex)
        .or_else(|| {
            cdn.get("aes_key")
                .and_then(Value::as_str)
                .and_then(key_from_base64)
        })
        .map(|key| STANDARD.encode(key));
    let source = Source { url, param, key };
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |key: &str| {
        body.get(key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    };
    let (name, size, mime, transcript) = match kind {
        MediaKind::File => (
            text("file_name").unwrap_or_default(),
            number("len"),
            None,
            None,
        ),
        MediaKind::Audio => (
            String::new(),
            None,
            voice_mime(body.get("encode_type").and_then(Value::as_i64)),
            text("text").filter(|text| !text.trim().is_empty()),
        ),
        _ => (String::new(), None, None, None),
    };
    Some(Media {
        kind,
        name,
        size,
        mime: mime.map(str::to_owned),
        transcript,
        source: serde_json::to_string(&source).ok()?,
    })
}

/// A voice item's encoding as a MIME type.
fn voice_mime(encode_type: Option<i64>) -> Option<&'static str> {
    Some(match encode_type? {
        5 => "audio/amr",
        6 => "audio/silk",
        7 => "audio/mpeg",
        8 => "audio/ogg",
        _ => return None,
    })
}

/// A key given as 32 hex digits.
fn key_from_hex(value: &str) -> Option<[u8; 16]> {
    let value = value.trim();
    if value.len() != 32 {
        return None;
    }
    let mut key = [0; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

/// A key given in base64, of either the raw 16 bytes or their 32 hex digits:
/// iLink uses both.
fn key_from_base64(value: &str) -> Option<[u8; 16]> {
    let decoded = STANDARD.decode(value.trim()).ok()?;
    match decoded.len() {
        16 => decoded.try_into().ok(),
        32 => key_from_hex(std::str::from_utf8(&decoded).ok()?),
        _ => None,
    }
}

/// A short label for a quoted item, such as its text or `[image]`.
pub fn item_label(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_i64) {
        Some(TEXT) => item
            .pointer("/text_item/text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some(VOICE) => Some(
            item.pointer("/voice_item/text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map_or_else(
                    || "[voice message]".into(),
                    |text| format!("[voice message] {text}"),
                ),
        ),
        Some(IMAGE) => Some("[image]".into()),
        Some(FILE) => Some(
            item.pointer("/file_item/file_name")
                .and_then(Value::as_str)
                .map_or_else(|| "[file]".into(), |name| format!("[file {name}]")),
        ),
        Some(VIDEO) => Some("[video]".into()),
        _ => None,
    }
}

/// Where a download or upload may go: HTTPS on a Tencent host. The address
/// comes from iLink, but a message should never make SCV fetch elsewhere.
pub fn check_cdn_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid CDN address"))?;
    let host = parsed.host_str().unwrap_or_default();
    // Lifecycle tests serve a fake CDN on loopback.
    #[cfg(test)]
    if parsed.scheme() == "http" && host == "127.0.0.1" {
        return Ok(parsed);
    }
    if parsed.scheme() != "https"
        || !(host == "qq.com" || host.ends_with(".qq.com"))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("CDN address is not an HTTPS Tencent host")
    }
    Ok(parsed)
}

/// The download address of `source`.
pub fn download_url(source: &Source) -> Result<reqwest::Url> {
    match &source.url {
        Some(url) => check_cdn_url(url),
        None => {
            let mut url = check_cdn_url(&format!("{CDN_BASE}/download"))?;
            url.query_pairs_mut()
                .append_pair("encrypted_query_param", &source.param);
            Ok(url)
        }
    }
}

/// Download `source`, decrypting it, and fail beyond `max_bytes`.
pub async fn download(
    client: &reqwest::Client,
    source: &Source,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let url = download_url(source)?;
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|_| anyhow!("CDN download failed"))?;
    if !response.status().is_success() {
        bail!(
            "CDN download failed with status {}",
            response.status().as_u16()
        )
    }
    // Ciphertext is at most one block longer than the file.
    let limit = max_bytes.saturating_add(16);
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        bail!("file is larger than the limit")
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("CDN download was interrupted"))?
    {
        if (body.len() + chunk.len()) as u64 > limit {
            bail!("file is larger than the limit")
        }
        body.extend_from_slice(&chunk);
    }
    let plain = match &source.key {
        Some(key) => decrypt(
            &body,
            &key_from_base64(key).ok_or_else(|| anyhow!("bad media key"))?,
        )?,
        None => body,
    };
    if plain.len() as u64 > max_bytes {
        bail!("file is larger than the limit")
    }
    Ok(plain)
}

/// AES-128-ECB with PKCS#7 padding.
pub fn encrypt(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - plain.len() % 16;
    let mut data = Vec::with_capacity(plain.len() + pad);
    data.extend_from_slice(plain);
    data.resize(plain.len() + pad, pad as u8);
    for block in data.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    data
}

pub fn decrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        bail!("encrypted file has a bad length")
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut plain = data.to_vec();
    for block in plain.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let pad = usize::from(*plain.last().expect("not empty"));
    if pad == 0
        || pad > 16
        || plain[plain.len() - pad..]
            .iter()
            .any(|&b| usize::from(b) != pad)
    {
        bail!("encrypted file has bad padding (wrong key?)")
    }
    plain.truncate(plain.len() - pad);
    Ok(plain)
}

/// A file prepared for upload: its encrypted bytes and what the upload and
/// the message need to name it.
pub struct Upload {
    pub kind: MediaKind,
    pub filekey: String,
    pub key: [u8; 16],
    pub raw_size: u64,
    pub raw_md5: String,
    pub encrypted: Vec<u8>,
}

impl Upload {
    pub fn new(plain: &[u8], kind: MediaKind) -> Self {
        // Two random UUIDs hashed give a full 128 random bits.
        use sha2::{Digest as _, Sha256};
        let seed = Sha256::new()
            .chain_update(uuid::Uuid::new_v4().as_bytes())
            .chain_update(uuid::Uuid::new_v4().as_bytes())
            .finalize();
        let mut key = [0; 16];
        key.copy_from_slice(&seed[..16]);
        Self {
            kind,
            filekey: uuid::Uuid::new_v4().simple().to_string(),
            key,
            raw_size: plain.len() as u64,
            raw_md5: format!("{:x}", Md5::digest(plain)),
            encrypted: encrypt(plain, &key),
        }
    }

    fn key_hex(&self) -> String {
        self.key.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The `getuploadurl` request body.
    pub fn request(&self, to_user_id: &str) -> Value {
        let media_type = match self.kind {
            MediaKind::Image => UPLOAD_IMAGE,
            MediaKind::Video => UPLOAD_VIDEO,
            _ => UPLOAD_FILE,
        };
        json!({
            "filekey": self.filekey,
            "media_type": media_type,
            "to_user_id": to_user_id,
            "rawsize": self.raw_size,
            "rawfilemd5": self.raw_md5,
            "filesize": self.encrypted.len(),
            "no_need_thumb": true,
            "aeskey": self.key_hex(),
            "base_info": {"channel_version": "1.0.0"},
        })
    }

    /// Where to post the encrypted bytes, from a `getuploadurl` reply.
    pub fn target(&self, reply: &Value) -> Result<reqwest::Url> {
        if let Some(full) = reply
            .get("upload_full_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            return check_cdn_url(full);
        }
        let param = reply
            .get("upload_param")
            .and_then(Value::as_str)
            .filter(|param| !param.is_empty())
            .ok_or_else(|| anyhow!("iLink gave no upload address"))?;
        let mut url = check_cdn_url(&format!("{CDN_BASE}/upload"))?;
        url.query_pairs_mut()
            .append_pair("encrypted_query_param", param)
            .append_pair("filekey", &self.filekey);
        Ok(url)
    }

    /// The message item that sends the uploaded file, from the CDN's
    /// download parameter.
    pub fn item(&self, download_param: &str, name: &str) -> Value {
        let media = json!({
            "encrypt_query_param": download_param,
            // The key's hex digits, base64: the form iLink's own clients send.
            "aes_key": STANDARD.encode(self.key_hex()),
            "encrypt_type": 1,
        });
        let size = self.encrypted.len();
        match self.kind {
            MediaKind::Image => {
                json!({"type": IMAGE, "image_item": {"media": media, "mid_size": size}})
            }
            MediaKind::Video => {
                json!({"type": VIDEO, "video_item": {"media": media, "video_size": size}})
            }
            _ => {
                json!({"type": FILE, "file_item": {"media": media, "file_name": name, "len": self.raw_size.to_string()}})
            }
        }
    }
}

/// The key a sent item carries, for tests that decrypt uploads.
#[cfg(test)]
pub fn tests_key(aes_key: &str) -> [u8; 16] {
    key_from_base64(aes_key).expect("a valid key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_ecb_matches_the_fips_197_vector_and_round_trips() {
        // FIPS-197 appendix C.1 (AES-128): one block, then PKCS#7 adds a
        // full block of padding.
        let key: [u8; 16] = key_from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let plain = key_from_hex("00112233445566778899aabbccddeeff").unwrap();
        let encrypted = encrypt(&plain, &key);
        assert_eq!(encrypted.len(), 32);
        assert_eq!(
            encrypted[..16],
            key_from_hex("69c4e0d86a7b0430d8cdb78070b4c55a").unwrap()
        );
        assert_eq!(decrypt(&encrypted, &key).unwrap(), plain);
        for length in [0, 1, 15, 16, 17, 1000] {
            let data = vec![7; length];
            assert_eq!(decrypt(&encrypt(&data, &key), &key).unwrap(), data);
        }
        let other = [9; 16];
        assert!(decrypt(&encrypt(b"hello", &key), &other).is_err());
        assert!(decrypt(&[0; 15], &key).is_err());
    }

    #[test]
    fn keys_come_as_hex_or_base64_of_raw_bytes_or_hex() {
        let raw = [0xab; 16];
        let hex = "ab".repeat(16);
        assert_eq!(key_from_hex(&hex), Some(raw));
        assert_eq!(key_from_base64(&STANDARD.encode(raw)), Some(raw));
        assert_eq!(key_from_base64(&STANDARD.encode(&hex)), Some(raw));
        assert_eq!(key_from_base64("short"), None);
        assert_eq!(key_from_hex("zz"), None);
    }

    #[test]
    fn media_items_parse_by_type() {
        let key = STANDARD.encode([1; 16]);
        let image = item_media(&json!({"type":2,"image_item":{
            "aeskey":"02".repeat(16),
            "media":{"encrypt_query_param":"p","aes_key":key,"full_url":"https://cdn.weixin.qq.com/d?x=1"}}}))
        .unwrap();
        assert_eq!(image.kind, MediaKind::Image);
        let source: Source = serde_json::from_str(&image.source).unwrap();
        assert_eq!(source.key, Some(STANDARD.encode([2; 16])), "hex key wins");
        assert_eq!(
            source.url.as_deref(),
            Some("https://cdn.weixin.qq.com/d?x=1")
        );

        let file = item_media(
            &json!({"type":4,"file_item":{"file_name":"a.pdf","len":"42",
            "media":{"encrypt_query_param":"p","aes_key":key}}}),
        )
        .unwrap();
        assert_eq!(
            (file.kind, file.name.as_str(), file.size),
            (MediaKind::File, "a.pdf", Some(42))
        );

        let voice = item_media(
            &json!({"type":3,"voice_item":{"encode_type":6,"text":"hi there",
            "media":{"encrypt_query_param":"p","aes_key":key}}}),
        )
        .unwrap();
        assert_eq!(voice.kind, MediaKind::Audio);
        assert_eq!(voice.mime.as_deref(), Some("audio/silk"));
        assert_eq!(voice.transcript.as_deref(), Some("hi there"));

        let video =
            item_media(&json!({"type":5,"video_item":{"media":{"encrypt_query_param":"p"}}}))
                .unwrap();
        assert_eq!(video.kind, MediaKind::Video);
        assert!(item_media(&json!({"type":1,"text_item":{"text":"hi"}})).is_none());
        assert!(item_media(&json!({"type":2,"image_item":{"media":{}}})).is_none());
    }

    #[test]
    fn cdn_addresses_must_be_https_tencent_hosts() {
        assert!(check_cdn_url("https://novac2c.cdn.weixin.qq.com/c2c/download?x=1").is_ok());
        assert!(check_cdn_url("http://novac2c.cdn.weixin.qq.com/x").is_err());
        assert!(check_cdn_url("https://evil.example/x").is_err());
        assert!(check_cdn_url("https://qq.com.evil.example/x").is_err());
        assert!(check_cdn_url("https://user@cdn.weixin.qq.com/x").is_err());
        let url = download_url(&Source {
            url: None,
            param: "a b&c".into(),
            key: None,
        })
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=a+b%26c"
        );
    }

    #[test]
    fn uploads_describe_the_file_and_point_at_the_cdn() {
        let upload = Upload::new(b"hello", MediaKind::File);
        assert_eq!(upload.raw_size, 5);
        assert_eq!(upload.raw_md5, "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(upload.encrypted.len(), 16);
        let request = upload.request("user");
        assert_eq!(request["media_type"], 3);
        assert_eq!(request["filesize"], 16);
        assert_eq!(request["aeskey"].as_str().unwrap().len(), 32);
        let target = upload.target(&json!({"upload_param":"up"})).unwrap();
        assert!(target.as_str().starts_with(
            "https://novac2c.cdn.weixin.qq.com/c2c/upload?encrypted_query_param=up&filekey="
        ));
        assert!(
            upload
                .target(&json!({"upload_full_url":"https://evil.example/u"}))
                .is_err()
        );
        assert!(upload.target(&json!({})).is_err());
        let item = upload.item("down", "a.txt");
        assert_eq!(item["type"], FILE);
        assert_eq!(item["file_item"]["file_name"], "a.txt");
        assert_eq!(item["file_item"]["len"], "5");
        let key = key_from_base64(item["file_item"]["media"]["aes_key"].as_str().unwrap()).unwrap();
        assert_eq!(decrypt(&upload.encrypted, &key).unwrap(), b"hello");
        assert_eq!(
            Upload::new(b"x", MediaKind::Image).item("d", "")["type"],
            IMAGE
        );
    }
}
