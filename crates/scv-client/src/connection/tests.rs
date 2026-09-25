//! Unit tests for src/connection.rs.

use super::*;
use scv_protocol::Overflow;
use tokio::io::BufReader;

#[tokio::test]
async fn frames_survive_small_reads_and_the_end_is_reported() {
    let input: &[u8] = b"{\"a\":1}\r\n{\"b\":2}\npartial";
    let mut reader = BufReader::with_capacity(3, input);
    let mut decoder = FrameDecoder::new(64, Overflow::Stop);
    assert_eq!(
        read_frame(&mut reader, &mut decoder).await.unwrap(),
        Frame::Line(b"{\"a\":1}\r".to_vec())
    );
    assert_eq!(
        read_frame(&mut reader, &mut decoder).await.unwrap(),
        Frame::Line(b"{\"b\":2}".to_vec())
    );
    assert_eq!(
        read_frame(&mut reader, &mut decoder).await.unwrap(),
        Frame::Truncated(b"partial".to_vec())
    );
}

#[tokio::test]
async fn a_frame_over_the_limit_is_refused() {
    let input: &[u8] = b"0123456789\n";
    let mut reader = BufReader::new(input);
    let mut decoder = FrameDecoder::new(8, Overflow::Stop);
    assert_eq!(
        read_frame(&mut reader, &mut decoder).await.unwrap(),
        Frame::TooLarge
    );
}

#[tokio::test]
async fn a_connection_writes_one_line_per_message() {
    let (client, server) = tokio::io::duplex(1024);
    let (_client_read, client_write) = tokio::io::split(client);
    let (server_read, _server_write) = tokio::io::split(server);
    let mut connection = Connection::new(
        BufReader::new(tokio::io::empty()),
        client_write,
        FrameDecoder::new(64, Overflow::Stop),
    );
    connection
        .send(&ClientMessage::initialize("init", "test"))
        .await
        .unwrap();
    let mut reader = BufReader::new(server_read);
    let mut decoder = FrameDecoder::new(4096, Overflow::Stop);
    let Frame::Line(line) = read_frame(&mut reader, &mut decoder).await.unwrap() else {
        panic!("no frame");
    };
    let message: ClientMessage = serde_json::from_slice(&line).unwrap();
    assert!(matches!(message, ClientMessage::Initialize { .. }));
    assert_eq!(
        read_frame(
            &mut BufReader::new(tokio::io::empty()),
            connection.decoder_mut()
        )
        .await
        .unwrap(),
        Frame::End
    );
}
