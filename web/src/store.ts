import { create } from "zustand";

export interface Track {
  id: string;
  title: string;
  artists: string[];
  album: string | null;
  duration_ms: number;
  sources: string[];
}

export interface PlayerState {
  state: "playing" | "paused" | "stopped";
  position_ms: number;
  duration_ms: number;
  volume: number;
  buffering: boolean;
  track: Track | null;
}

export interface Search {
  id: number;
  pending: string[];
  hits: Track[];
}

export type ServerMessage =
  | ({ type: "state" } & PlayerState)
  | { type: "search_started"; search: number; sources: string[] }
  | { type: "search_hit"; search: number; track: Track }
  | { type: "search_done"; search: number; source: string };

interface Store {
  connected: boolean;
  player: PlayerState;
  frame: number;
  search: Search | null;
  setConnected: (connected: boolean) => void;
  receive: (msg: ServerMessage) => void;
  clearSearch: () => void;
}

const MAX_HITS = 300;

export const useStore = create<Store>((set) => ({
  connected: false,
  player: { state: "stopped", position_ms: 0, duration_ms: 0, volume: 1, buffering: false, track: null },
  frame: 0,
  search: null,
  // The server forgets a connection's search when it drops.
  setConnected: (connected) => set(connected ? { connected } : { connected, search: null }),
  clearSearch: () => set({ search: null }),
  receive: (msg) =>
    set((s) => {
      switch (msg.type) {
        case "state": {
          const { type: _, ...player } = msg;
          return { player, frame: s.frame + 1 };
        }
        case "search_started":
          return { search: { id: msg.search, pending: msg.sources, hits: [] } };
        case "search_hit":
          return s.search?.id === msg.search && s.search.hits.length < MAX_HITS
            ? { search: { ...s.search, hits: [...s.search.hits, msg.track] } }
            : {};
        case "search_done":
          return s.search?.id === msg.search
            ? { search: { ...s.search, pending: s.search.pending.filter((p) => p !== msg.source) } }
            : {};
      }
    }),
}));
