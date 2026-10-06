import { type ServerMessage, useStore } from "./store";

type ClientMessage =
  | { type: "play_pause" | "next" | "previous" }
  | { type: "seek"; position_ms: number }
  | { type: "volume"; volume: number }
  | { type: "search"; text: string }
  | { type: "play" | "enqueue"; track: string };

let socket: WebSocket | null = null;

export function send(msg: ClientMessage) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(msg));
}

export function connect() {
  const ws = new WebSocket(`ws://${location.host}/ws`);
  socket = ws;
  ws.onopen = () => useStore.getState().setConnected(true);
  ws.onmessage = (e) => useStore.getState().receive(JSON.parse(e.data) as ServerMessage);
  ws.onclose = () => {
    useStore.getState().setConnected(false);
    setTimeout(connect, 1000);
  };
}
