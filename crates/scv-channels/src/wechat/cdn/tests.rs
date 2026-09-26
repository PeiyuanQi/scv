//! Unit tests for `src/wechat/cdn.rs`.

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
        item_media(&json!({"type":5,"video_item":{"media":{"encrypt_query_param":"p"}}})).unwrap();
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
