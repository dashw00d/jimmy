use jimmy_proto::{ClientEvent, decode_audio, decode_pcm16, encode_audio, pcm16_bytes};

#[test]
fn audio_header_is_little_endian() {
    let pcm = [i16::MIN, 1, i16::MAX];
    let encoded = encode_audio(0x12345678, 2, &pcm16_bytes(&pcm));
    assert_eq!(&encoded[..8], &[0x78, 0x56, 0x34, 0x12, 2, 0, 0, 0]);
    let (turn, sequence, payload) = decode_audio(&encoded).unwrap();
    assert_eq!((turn, sequence), (0x12345678, 2));
    assert_eq!(decode_pcm16(payload).unwrap(), pcm);
    assert!(decode_pcm16(&[1]).is_none());
    assert!(decode_audio(&[0; 7]).is_none());
}

#[test]
fn future_events_do_not_break_current_sessions() {
    assert!(matches!(
        serde_json::from_str::<ClientEvent>(r#"{"type":"future.event","nested":{"x":1}}"#).unwrap(),
        ClientEvent::Unknown
    ));
    assert!(matches!(
        serde_json::from_str::<ClientEvent>(r#"{"type":"cancel"}"#).unwrap(),
        ClientEvent::Cancel { turn: None }
    ));
}
