// Ferry's WebRTC transfer library (docs/05-protocol.md §5; threat model W1 to W7).
// Portable on purpose: no Vue, no DOM globals (WebSocket and RTCPeerConnection
// are injectable), so it runs in a plain browser page and in Node tests.
//
// How the pieces fit:
//
//   SignalingClient  WebSocket to ferry-signal (LocalSend `/v1/ws` compatible):
//                    presence (HELLO/JOIN/LEFT), rooms, and relaying of
//                    OFFER/ANSWER/ICE/CANCEL. Reconnects with backoff and
//                    re-joins its rooms.
//         │
//   PeerConnector    Signaling → RTCPeerConnection: the offerer creates the
//                    ordered, reliable `ferry/1` data channel, the answerer
//                    answers; ICE trickles both ways. Hands the channel and both
//                    DTLS fingerprints to a PeerSession and resolves once the
//                    peer is authenticated.
//         │
//   PeerSession      ferry-dc/1 on one data channel: hello/auth signatures bound
//                    to both DTLS fingerprints (a signaling MITM fails here),
//                    then offer/answer, files streamed with backpressure and a
//                    flow-control window, SHA-256 verified per file, cancel in
//                    either direction, pings, resume offsets.
//
// Minimal usage:
//
//   const identity = deserializeFromIdb(await idb.get("identity")) ?? (await generateIdentity());
//   const publicKey = await exportPublicKey(identity);
//   const device = { alias: "My laptop", deviceType: "web", platform: "browser" };
//   const signaling = new SignalingClient({
//     url: "wss://signal.example/v1/ws",
//     info: { alias: device.alias, deviceType: "web", token: randomId(), publicKey },
//   });
//   const connector = new PeerConnector({
//     signaling, identity, device,
//     sink, // FileSink: where received files go (OPFS, File System Access, …)
//     iceServers: async () => [{ urls: stunUrls }, ...((await signaling.fetchTurn())?.iceServers ?? [])],
//   });
//   signaling.on("hello", ({ peers }) => showNearby(peers));
//   signaling.connect();
//
//   // Receiving: sessions announce offers; the user decides (never auto-accept strangers).
//   connector.on("incoming", (request) => void request.accept().catch(console.warn));
//   connector.on("session", ({ session }) => session.on("offer", (offer) => askUser(offer)));
//
//   // Sending:
//   const session = await connector.connect(peer.id);
//   const outcome = await session.sendTransfer({
//     transferId: randomId(),
//     files: picked.map((f, i) => ({
//       id: String(i), name: f.name, size: f.size, mime: f.type,
//       slice: (start, end) => f.slice(start, end).arrayBuffer(),
//     })),
//   });
//
//   // Link/QR rooms: the secret travels in the URL fragment, only its hash reaches the server.
//   const secret = randomBytes(16); // share `https://…/#room=${b64urlEncode(secret)}`
//   signaling.joinRoom(roomIdFromSecret(secret));
//   const roomSession = await connector.connect(roomPeer.id, undefined, { roomSecret: secret });

// Signaling (§5.1)
export { isValidRoomId, SignalingClient, SIGNALING_CAPS, SIGNALING_VERSION } from "./signaling";
export type {
  ClientExt,
  ClientInfo,
  ServerInfo,
  SignalingClientOptions,
  SignalingDeviceType,
  SignalingErrorEvent,
  SignalingEvents,
  SignalingInfo,
  SignalingState,
  TurnCredentials,
  WebSocketFactory,
  WebSocketLike,
} from "./signaling";

// Connector
export { PeerConnector } from "./peer";
export type { ConnectOptions, IncomingConnection, PeerConnectorEvents, PeerConnectorOptions } from "./peer";

// Session (§5.2)
export { PeerSession } from "./session";
export type {
  CloseReason,
  DataChannelLike,
  Direction,
  FileComplete,
  FileSink,
  IncomingOffer,
  PeerSessionOptions,
  RemotePeer,
  SessionClosed,
  SessionError,
  SessionEvents,
  SessionState,
  SinkAbortReason,
  SinkContext,
  SinkWriter,
  SourceFile,
  TransferAccepted,
  TransferCancelled,
  TransferDone,
  TransferOutcome,
  TransferProgress,
  TransferRequest,
} from "./session";

// Protocol limits and checks the UI can apply before offering
export {
  DC_LABEL,
  fileNameProblem,
  isValidFileName,
  isValidId,
  MAX_FILES,
  MAX_NAME_LENGTH,
  MAX_PATH_DEPTH,
  MAX_TEXT_BYTES,
  parseMaxMessageSize,
  RtcError,
} from "./protocol";
export type { DeviceInfo, FileMeta } from "./protocol";

// Identity (persist with serializeForIdb / deserializeFromIdb)
export { deserializeFromIdb, exportPublicKey, generateIdentity, serializeForIdb } from "./identity";
export type { Identity, IdentityAlg, StoredIdentity } from "./identity";

// Rooms, ids, transcript inputs for custom transports
export { extractFingerprint, roomIdFromSecret } from "./transcript";
export type { Role } from "./transcript";
export { b64urlDecode, b64urlEncode, randomBytes, randomId } from "./bytes";
export type { Bytes } from "./bytes";
