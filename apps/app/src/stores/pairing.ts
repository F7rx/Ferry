// Pairing "my devices": show a QR code / link, use one from another device,
// or compare a 6-digit code with a nearby device.
import { platform, type DeviceSummary, type PairingRequest } from "../platform";
import { attempt, displayName, store, toast } from "./engine";

export async function showPairingCode() {
  const offer = await attempt(() => platform.createPairingOffer());
  if (offer) store.pairing.offer = offer;
  return offer;
}

export async function hidePairingCode() {
  const offer = store.pairing.offer;
  store.pairing.offer = null;
  if (offer) await attempt(() => platform.cancelPairingOffer(offer.id));
}

export async function pairWithLink(uri: string) {
  const device = await attempt(() => platform.pairWithUri(uri.trim()));
  if (device) {
    store.devices.set(device.id, device);
    toast({ level: "success", title: `${displayName(device)} is now one of your devices`, body: "Files between them arrive without asking." }, 5200);
  }
  return device;
}

export async function startCompare(device: DeviceSummary) {
  const pairing = await attempt(() => platform.startCodePairing(device.id));
  if (pairing) store.pairing.outgoing = pairing;
  return pairing;
}

export async function cancelCompare() {
  const pairing = store.pairing.outgoing;
  store.pairing.outgoing = null;
  if (pairing) await attempt(() => platform.cancelCodePairing(pairing.id));
}

export async function answerPairing(request: PairingRequest, match: boolean) {
  store.pairing.requests = store.pairing.requests.filter((r) => r.id !== request.id);
  await attempt(() => platform.respondPairing(request.id, match));
}

export async function unpair(device: DeviceSummary) {
  const updated = await attempt(() => platform.unpairDevice(device.id));
  if (updated) store.devices.set(updated.id, updated);
  else if (updated === null) store.devices.delete(device.id);
  toast({ level: "info", title: `${displayName(device)} is no longer one of your devices` }, 3200);
}

/** Devices that can become "mine": verified Ferry devices nearby. */
export function canPair(d: DeviceSummary) {
  return platform.capabilities.pairing && d.verified && d.isFerry && d.online && !d.mine;
}
