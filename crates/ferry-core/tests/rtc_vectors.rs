//! The native `ferry-dc/1` implementation against the vectors the TypeScript
//! reference writes (`apps/app/src/lib/rtc/vectors.gen.test.ts` →
//! `tests/vectors/rtc.json`): same bytes on the wire, same accept/reject
//! decisions, same transcript, signatures, room ids and fingerprints.

use ferry_core::rtc::b64;
use ferry_core::rtc::identity::{RtcIdentity, is_valid_public_key, verify};
use ferry_core::rtc::protocol::*;
use ferry_core::rtc::signaling;
use ferry_core::rtc::transcript::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn vectors() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/rtc.json")).expect("vectors file");
    serde_json::from_str(&text).expect("vectors parse")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("expected a string, got {v}"))
}

fn hex(v: &Value) -> Vec<u8> {
    hex::decode(s(v)).unwrap()
}

fn arr(v: &Value) -> &Vec<Value> {
    v.as_array().unwrap_or_else(|| panic!("expected an array, got {v}"))
}

fn sha_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[test]
fn base64url() {
    let v = vectors();
    for case in arr(&v["base64url"]["encode"]) {
        assert_eq!(b64::encode(&hex(&case["hex"])), s(&case["b64"]));
    }
    for case in arr(&v["base64url"]["decode"]) {
        let got = b64::decode(s(&case["text"])).map(hex::encode);
        assert_eq!(got.as_deref(), case["hex"].as_str(), "decode {}", case["text"]);
    }
}

#[test]
fn ed25519_signatures_are_identical() {
    let v = vectors();
    for id in arr(&v["identity"]["ed25519"]) {
        let seed: [u8; 32] = hex(&id["seed"]).try_into().unwrap();
        let identity = RtcIdentity::from_seed(&seed);
        assert_eq!(identity.public_key(), s(&id["publicKey"]));
        // The stored engine key is PKCS#8: the same key comes back from PEM.
        let der = hex(&id["pkcs8"]);
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64_std(&der));
        assert_eq!(RtcIdentity::from_pkcs8_pem(&pem).unwrap().public_key(), s(&id["publicKey"]));
        for sig in arr(&id["signatures"]) {
            let data = hex(&sig["data"]);
            assert_eq!(identity.sign(&data), s(&sig["sig"]));
            assert!(verify(Alg::Ed25519, identity.public_key(), &data, s(&sig["sig"])));
            assert!(!verify(Alg::Ed25519, identity.public_key(), b"other", s(&sig["sig"])));
        }
    }
}

fn base64_std(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[test]
fn p256_and_malformed_verification() {
    let v = vectors();
    let p256 = arr(&v["identity"]["p256"]);
    assert!(p256.len() >= 10);
    for case in p256 {
        assert_eq!(
            verify(Alg::P256, s(&case["publicKey"]), &hex(&case["data"]), s(&case["sig"])),
            case["valid"].as_bool().unwrap(),
            "{case}"
        );
    }
    for case in arr(&v["identity"]["verifyRejects"]) {
        let got = match Alg::parse(s(&case["alg"])) {
            Some(alg) => verify(alg, s(&case["key"]), &hex(&case["data"]), s(&case["sig"])),
            None => false,
        };
        assert_eq!(got, case["valid"].as_bool().unwrap(), "{case}");
    }
    for case in arr(&v["identity"]["keyChecks"]) {
        let alg = Alg::parse(s(&case["alg"])).unwrap();
        assert_eq!(is_valid_public_key(alg, &hex(&case["key"])), case["valid"].as_bool().unwrap(), "{case}");
    }
}

#[test]
fn transcript_for_both_roles() {
    let v = vectors();
    for t in arr(&v["transcript"]) {
        let key_o = b64::decode(s(&t["keyOfferer"])).unwrap();
        let key_a = b64::decode(s(&t["keyAnswerer"])).unwrap();
        let parts = TranscriptParts {
            session_id: s(&t["sessionId"]),
            fp_offerer: s(&t["fpOfferer"]),
            fp_answerer: s(&t["fpAnswerer"]),
            nonce_offerer: &hex(&t["nonceOfferer"]),
            nonce_answerer: &hex(&t["nonceAnswerer"]),
            key_offerer: &key_o,
            key_answerer: &key_a,
        };
        let hash = transcript_hash(&parts);
        assert_eq!(sha_hex(&hex(&t["input"])), s(&t["transcript"]), "input layout");
        assert_eq!(hex::encode(hash), s(&t["transcript"]));
        assert_eq!(short_code(&hash), s(&t["shortCode"]));
        for (role, seed_index) in [(Role::Offerer, 0), (Role::Answerer, 1)] {
            let payload = auth_payload(role, &hash);
            assert_eq!(hex::encode(&payload), s(&t["authPayload"][role.as_str()]));
            let seed: [u8; 32] = hex(&v["identity"]["ed25519"][seed_index]["seed"]).try_into().unwrap();
            let id = RtcIdentity::from_seed(&seed);
            assert_eq!(id.sign(&payload), s(&t["signature"][role.as_str()]));
            // A signature for one role never verifies as the other.
            assert!(!verify(Alg::Ed25519, id.public_key(), &auth_payload(role.other(), &hash), s(&t["signature"][role.as_str()])));
        }
        let secret = hex(&t["roomSecret"]);
        let room_key = derive_room_key(&secret);
        assert_eq!(hex::encode(room_key), s(&t["roomKey"]));
        assert_eq!(room_mac(&room_key, &hash), s(&t["mac"]));
        assert!(verify_room_mac(&room_key, &hash, s(&t["mac"])));
        assert!(!verify_room_mac(&room_key, &hash, s(&t["wrongSecretMac"])));
        assert!(!verify_room_mac(&room_key, &hash, "garbage!"));
        assert_eq!(room_id_from_secret(&secret), s(&t["roomId"]));
        assert_eq!(b64::encode(&secret), s(&t["roomSecretB64"]));
        // The hello and auth frames the reference sends for these inputs.
        for (field, role) in [("helloOfferer", "Offerer"), ("helloAnswerer", "Answerer")] {
            let parsed = parse_control(s(&t[field])).unwrap();
            assert_eq!(parsed.to_json(), s(&t[field]), "{role} hello re-encodes identically");
        }
        let auth = Control::Auth { sig: s(&t["signature"]["offerer"]).into(), mac: None };
        assert_eq!(auth.to_json(), s(&t["authOfferer"]));
        let auth = Control::Auth { sig: s(&t["signature"]["answerer"]).into(), mac: Some(s(&t["mac"]).into()) };
        assert_eq!(auth.to_json(), s(&t["authAnswererInRoom"]));
    }
    for c in arr(&v["shortCodes"]) {
        assert_eq!(short_code(&hex(&c["transcript"])), s(&c["code"]));
    }
    for r in arr(&v["rooms"]) {
        let secret = hex(&r["secret"]);
        assert_eq!(room_id_from_secret(&secret), s(&r["roomId"]));
        assert_eq!(b64::encode(&secret), s(&r["secretB64"]));
        assert_eq!(hex::encode(derive_room_key(&secret)), s(&r["roomKey"]));
    }
}

#[test]
fn dtls_fingerprints_and_message_size() {
    let v = vectors();
    for f in arr(&v["fingerprints"]) {
        assert_eq!(extract_fingerprint(s(&f["sdp"])).as_deref(), f["fingerprint"].as_str(), "{}", f["name"]);
    }
    for m in arr(&v["maxMessageSizes"]) {
        let got = parse_max_message_size(s(&m["sdp"]));
        if m["unlimited"].as_bool().unwrap() {
            assert_eq!(got, MaxMessageSize::Unlimited, "{}", m["name"]);
        } else {
            let want = m["maxMessageSize"].as_f64().unwrap();
            match got {
                MaxMessageSize::Limit(n) if want < 1e19 => assert_eq!(n as f64, want, "{}", m["name"]),
                MaxMessageSize::Limit(n) => assert_eq!(n, u64::MAX, "{}", m["name"]),
                other => panic!("{}: {other:?}", m["name"]),
            }
        }
        assert_eq!(chunk_size_for(Some(got)) as u64, m["chunkSize"].as_u64().unwrap(), "{}", m["name"]);
    }
    for c in arr(&v["chunkSizes"]) {
        let max = c["maxMessageSize"].as_u64().map(MaxMessageSize::Limit);
        assert_eq!(chunk_size_for(max) as u64, c["chunkSize"].as_u64().unwrap(), "{c}");
    }
    assert_eq!(chunk_size_for(Some(MaxMessageSize::Unlimited)), LARGE_CHUNK);
    assert_eq!(v["limits"]["maxControlBytes"].as_u64().unwrap() as usize, MAX_CONTROL_BYTES);
}

#[test]
fn control_messages_encode_identically() {
    let v = vectors();
    for case in arr(&v["control"]["encode"]) {
        let json = s(&case["json"]);
        let parsed = parse_control(json).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(encode_control(&parsed).unwrap(), json);
    }
    for case in arr(&v["control"]["errorFrames"]) {
        let frame = error_frame(s(&case["code"]), s(&case["message"]));
        assert_eq!(encode_control(&frame).unwrap(), s(&case["json"]));
    }
}

#[test]
fn control_parser_accepts_and_rejects_the_same_frames() {
    let v = vectors();
    let cases = arr(&v["control"]["parse"]);
    assert!(cases.len() > 90);
    for case in cases {
        let frame = s(&case["frame"]);
        let got = parse_control(frame);
        if case["ok"].as_bool().unwrap() {
            let msg = got.unwrap_or_else(|e| panic!("should accept {frame}: {e}"));
            assert_eq!(msg.to_json(), s(&case["reencoded"]), "normalized form of {frame}");
        } else {
            let err = got.err().unwrap_or_else(|| panic!("should reject {frame}"));
            assert_eq!(err.code, s(&case["code"]), "{frame}");
        }
    }
}

#[test]
fn file_name_table() {
    let v = vectors();
    let names = arr(&v["fileNames"]);
    assert!(names.len() >= 40);
    let (mut accepted, mut rejected) = (0, 0);
    for case in names {
        let want = case["problem"].as_str();
        match case.get("text").and_then(Value::as_str) {
            Some(name) => {
                assert_eq!(file_name_problem(name), want, "{name:?}");
                // The offer parser applies the same rule.
                let frame = format!(
                    r#"{{"t":"offer","transferId":"t1","files":[{{"id":"f","name":{},"size":1,"mime":""}}]}}"#,
                    serde_json::to_string(name).unwrap()
                );
                assert_eq!(parse_control(&frame).is_ok(), want.is_none(), "{name:?}");
            }
            None => {
                // Lone surrogates: not representable in Rust; the frame fails to parse.
                let units: Vec<u16> = arr(&case["utf16"]).iter().map(|u| u.as_u64().unwrap() as u16).collect();
                assert!(String::from_utf16(&units).is_err());
                assert_eq!(want, Some("is not valid Unicode"));
            }
        }
        if want.is_some() { rejected += 1 } else { accepted += 1 }
    }
    assert!(accepted >= 15 && rejected >= 25, "{accepted} accepted, {rejected} rejected");
}

fn metas(input: &Value) -> Vec<FileMeta> {
    arr(&input["files"])
        .iter()
        .map(|f| FileMeta {
            id: s(&f["id"]).into(),
            name: s(&f["name"]).into(),
            size: f["size"].as_u64().unwrap(),
            mime: s(&f["mime"]).into(),
            modified: f.get("modified").and_then(Value::as_i64),
        })
        .collect()
}

#[test]
fn split_offers_and_answers() {
    let v = vectors();
    let split = &v["split"];
    let small = &split["smallOffer"];
    let frames = encode_offer(s(&small["input"]["transferId"]), &metas(&small["input"]), small["input"]["text"].as_str()).unwrap();
    assert_eq!(frames, arr(&small["frames"]).iter().map(|f| s(f).to_string()).collect::<Vec<_>>());
    for a in arr(&split["smallAnswers"]) {
        let accept: Vec<String> = arr(&a["input"]["accept"]).iter().map(|x| s(x).to_string()).collect();
        let offsets: Vec<(String, u64)> =
            a["input"]["offsets"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap())).collect();
        let frames = encode_answer("t1", &accept, &offsets, a["input"]["declined"].as_bool().unwrap()).unwrap();
        assert_eq!(frames, arr(&a["frames"]).iter().map(|f| s(f).to_string()).collect::<Vec<_>>());
    }

    // 3000 files: same rule as the generator.
    let files: Vec<FileMeta> = (0..3000u64)
        .map(|i| FileMeta {
            id: format!("file-{i}"),
            name: format!("Holiday {}/IMG_{:05} \u{b7} {}.jpg", i % 7, i, "\u{e4}".repeat(40)),
            size: i * 1000,
            mime: "image/jpeg".into(),
            modified: (i % 3 == 0).then_some(1_700_000_000_000 + i as i64),
        })
        .collect();
    let frames = encode_offer("t1", &files, Some("for you")).unwrap();
    assert_eq!(&frames[0][..s(&split["offer"]["firstFrameHead"]).len()], s(&split["offer"]["firstFrameHead"]));
    let want: Vec<(u64, String)> =
        arr(&split["offer"]["frames"]).iter().map(|f| (f["bytes"].as_u64().unwrap(), s(&f["sha256"]).to_string())).collect();
    let got: Vec<(u64, String)> = frames.iter().map(|f| (f.len() as u64, sha_hex(f.as_bytes()))).collect();
    assert_eq!(got, want, "offer frames");
    // ...and they reassemble on the receiving side.
    let mut seen = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let Control::Offer(o) = parse_control(f).unwrap() else { panic!() };
        assert_eq!(o.more, i + 1 < frames.len());
        assert_eq!(o.text.is_some(), i == 0);
        seen.extend(o.files);
    }
    assert_eq!(seen, files);

    let accept: Vec<String> = (0..10_000).map(|i| format!("file-{i}-{}", "x".repeat(20))).collect();
    let offsets: Vec<(String, u64)> = (0..10_000).step_by(7).map(|i| (accept[i].clone(), i as u64 + 1)).collect();
    let frames = encode_answer("t1", &accept, &offsets, false).unwrap();
    let want: Vec<(u64, String)> =
        arr(&split["answer"]["frames"]).iter().map(|f| (f["bytes"].as_u64().unwrap(), s(&f["sha256"]).to_string())).collect();
    let got: Vec<(u64, String)> = frames.iter().map(|f| (f.len() as u64, sha_hex(f.as_bytes()))).collect();
    assert_eq!(got, want, "answer frames");

    for t in arr(&split["textLimits"]) {
        let text = "x".repeat(t["textLength"].as_u64().unwrap() as usize);
        let got = encode_offer("t1", &[], Some(&text)).err().map(|e| e.code);
        assert_eq!(got.as_deref(), t["error"]["code"].as_str());
    }
    let huge: Vec<FileMeta> = (0..10_000)
        .map(|i| FileMeta { id: format!("f{i}"), name: format!("{}{i}", "n".repeat(900)), size: 1, mime: String::new(), modified: None })
        .collect();
    assert_eq!(encode_offer("t1", &huge, None).unwrap_err().code, s(&split["offerTooLarge"]["code"]));
}

#[test]
fn binary_framing() {
    let v = vectors();
    for b in arr(&v["binary"]) {
        let size = b["size"].as_u64().unwrap() as usize;
        let offset = b["offset"].as_u64().unwrap() as usize;
        let chunk = b["chunkSize"].as_u64().unwrap() as usize;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        assert_eq!(Control::File { id: "0".into(), offset: offset as u64 }.to_json(), s(&b["fileMsg"]));
        let chunks = ferry_core::rtc::session::chunk_plan(size as u64, offset as u64, chunk);
        match &b["chunks"] {
            Value::Array(want) => assert_eq!(chunks, want.iter().map(|c| c.as_u64().unwrap() as usize).collect::<Vec<_>>()),
            summary => {
                assert_eq!(chunks.len() as u64, summary["count"].as_u64().unwrap());
                assert_eq!(chunks.iter().sum::<usize>() as u64, summary["total"].as_u64().unwrap());
                assert_eq!(&chunks[..3], arr(&summary["first"]).iter().map(|c| c.as_u64().unwrap() as usize).collect::<Vec<_>>());
            }
        }
        assert_eq!(hex::encode(&data[offset..(offset + 8).min(size)]), s(&b["firstChunkHead"]));
        let digest = sha_hex(&data);
        assert_eq!(digest, s(&b["sha256"]));
        assert_eq!(Control::FileEnd { id: "0".into(), sha256: digest }.to_json(), s(&b["fileEnd"]));
    }
}

#[test]
fn signaling_forms() {
    let v = vectors();
    let sig = &v["signaling"];
    // The ?d= client info.
    let info = signaling::ClientInfoOut {
        alias: format!("Maya's laptop {}", "\u{1F600}".repeat(70)),
        device_model: Some("Windows".into()),
        device_type: Some("desktop".into()),
        token: "tok-1".into(),
        public_key: "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc".into(),
        nearby: None,
    };
    assert_eq!(info.to_json(), s(&sig["clientInfo"]["json"]));
    assert_eq!(signaling::connect_url("wss://signal.example/v1/ws", &info).unwrap(), s(&sig["clientInfo"]["url"]));
    let kiosk = signaling::ClientInfoOut {
        alias: "Kiosk".into(),
        device_model: None,
        device_type: None,
        token: "t".into(),
        public_key: s(&serde_json::from_str::<Value>(s(&sig["clientInfoNearbyOff"]["json"])).unwrap()["ext"]["key"]).into(),
        nearby: Some(false),
    };
    assert_eq!(kiosk.to_json(), s(&sig["clientInfoNearbyOff"]["json"]));
    assert_eq!(signaling::connect_url("ws://127.0.0.1:39000/v1/ws?x=1", &kiosk).unwrap(), s(&sig["clientInfoNearbyOff"]["url"]));

    // Outbound messages.
    let out: Vec<String> = arr(&sig["outbound"]).iter().map(|o| s(&o["text"]).to_string()).collect();
    let cand = signaling::IceCandidate {
        candidate: "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host".into(),
        sdp_mid: Some(Some("0".into())),
        sdp_m_line_index: Some(Some(0)),
        username_fragment: Some(Some("abcd".into())),
    };
    let bare =
        signaling::IceCandidate { candidate: cand.candidate.clone(), sdp_mid: None, sdp_m_line_index: None, username_fragment: None };
    let mine = [
        signaling::outbound::sdp("OFFER", "p1", "s1", "<sdp>"),
        signaling::outbound::sdp("ANSWER", "p1", "s1", "<sdp>"),
        signaling::outbound::ice("p1", "s1", Some(&cand)),
        signaling::outbound::ice("p1", "s1", Some(&bare)),
        signaling::outbound::ice("p1", "s1", None),
        signaling::outbound::cancel("p1", "s1"),
        signaling::outbound::room("ROOM_JOIN", &format!("r:{}", "B".repeat(22))),
        signaling::outbound::room("ROOM_LEAVE", &format!("r:{}", "B".repeat(22))),
        signaling::outbound::update(&signaling::ClientInfoOut { alias: "Renamed".into(), ..info.clone() }),
    ];
    assert_eq!(mine.to_vec(), out);

    // SDP compression: the reference's zlib output decodes here, ours round-trips.
    for case in arr(&sig["sdp"]) {
        let sdp = s(&case["sdp"]);
        assert_eq!(signaling::decode_sdp(s(&case["encoded"])).unwrap(), sdp);
        let ours = signaling::encode_sdp(sdp);
        assert_eq!(signaling::decode_sdp(&ours).unwrap(), sdp);
        assert_eq!(&hex::encode(b64::decode(&ours).unwrap())[..2], "78", "zlib header");
        let standard = ours.replace('-', "+").replace('_', "/") + &"=".repeat((4 - ours.len() % 4) % 4);
        assert_eq!(signaling::decode_sdp(&standard).unwrap(), sdp);
    }
    assert!(sig["sdpBombRejected"].as_bool().unwrap());
    assert!(signaling::decode_sdp(&signaling::encode_sdp(&"a".repeat(300 * 1024))).is_err());
    assert!(signaling::decode_sdp("!!!not-base64").is_err());

    for r in arr(&sig["roomIds"]) {
        assert_eq!(signaling::is_valid_room_id(s(&r["room"])), r["valid"].as_bool().unwrap(), "{}", r["room"]);
    }

    // Inbound frames → the same events (or none).
    for case in arr(&sig["inbound"]) {
        let frame = s(&case["frame"]);
        let got = signaling::parse_server_message(frame).map(|e| e.to_reference_json()).into_iter().collect::<Vec<_>>();
        let want: Vec<Value> = arr(&case["events"]).clone();
        assert_eq!(got, want, "{frame}");
    }
}
