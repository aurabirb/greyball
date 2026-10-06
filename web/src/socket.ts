import { type ServerMessage, useStore } from "./store";

type ClientMessage =
  | { type: "play_pause" | "next" | "previous" }
  | { type: "seek"; position_ms: number }
  | { type: "volume"; volume: number }
  | { type: "search"; text: string }
  | { type: "play" | "enqueue"; track: string };

const VOLUME_INTERVAL_MS = 50;
const MAX_RETRY_MS = 10_000;

let socket: WebSocket | null = null;
let retryMs = 1000;
let volumeTimer: number | null = null;
let volumePending: number | null = null;

export function send(msg: ClientMessage) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(msg));
}

// Leading send, then at most one trailing send per interval.
export function sendVolume(volume: number) {
  if (volumeTimer !== null) {
    volumePending = volume;
    return;
  }
  send({ type: "volume", volume });
  volumeTimer = window.setTimeout(() => {
    volumeTimer = null;
    const pending = volumePending;
    volumePending = null;
    if (pending !== null) sendVolume(pending);
  }, VOLUME_INTERVAL_MS);
}

export function connect() {
  const ws = new WebSocket(`ws://${location.host}/ws`);
  socket = ws;
  ws.onopen = () => {
    retryMs = 1000;
    useStore.getState().setConnected(true);
  };
  ws.onmessage = (e) => {
    try {
      useStore.getState().receive(JSON.parse(e.data) as ServerMessage);
    } catch {
      // ignore a malformed frame
    }
  };
  ws.onclose = () => {
    useStore.getState().setConnected(false);
    setTimeout(connect, retryMs);
    retryMs = Math.min(retryMs * 2, MAX_RETRY_MS);
  };
}
