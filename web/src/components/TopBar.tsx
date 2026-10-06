import { useState } from "react";
import { clock } from "../format";
import { send, sendVolume } from "../socket";
import { useStore } from "../store";

// Shows the user's slider value until the first state frame after release, so the thumb doesn't snap back.
function useHeld(server: number): [number, (v: number) => void, () => void] {
  const frame = useStore((s) => s.frame);
  const [held, setHeld] = useState<{ value: number; releasedAt: number | null } | null>(null);
  const shown = held && (held.releasedAt === null || held.releasedAt === frame) ? held.value : server;
  const hold = (value: number) => setHeld({ value, releasedAt: null });
  const release = () => setHeld((h) => h && { ...h, releasedAt: useStore.getState().frame });
  return [shown, hold, release];
}

const SEEK_KEYS = ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"];

export function TopBar() {
  const p = useStore((s) => s.player);
  const [position, holdPosition, releasePosition] = useHeld(p.position_ms);
  const [volume, holdVolume, releaseVolume] = useHeld(p.volume);

  const commitSeek = (e: { currentTarget: HTMLInputElement }) => {
    send({ type: "seek", position_ms: Number(e.currentTarget.value) });
    releasePosition();
  };

  return (
    <header className="topbar">
      <div className="now">
        <strong>{p.track?.title ?? "Nothing playing"}</strong>
        <span>{p.track?.artists.join(", ")}</span>
      </div>
      <div className="controls">
        <button onClick={() => send({ type: "previous" })} aria-label="Previous">⏮</button>
        <button onClick={() => send({ type: "play_pause" })} aria-label="Play or pause">
          {p.buffering ? "…" : p.state === "playing" ? "⏸" : "▶"}
        </button>
        <button onClick={() => send({ type: "next" })} aria-label="Next">⏭</button>
      </div>
      <div className="seek">
        <span>{clock(position)}</span>
        <input
          type="range"
          min={0}
          max={p.duration_ms}
          value={Math.min(position, p.duration_ms)}
          disabled={p.state === "stopped"}
          onChange={(e) => holdPosition(Number(e.target.value))}
          onPointerUp={commitSeek}
          onKeyUp={(e) => SEEK_KEYS.includes(e.key) && commitSeek(e)}
          onPointerCancel={releasePosition}
          onBlur={releasePosition}
        />
        <span>{clock(p.duration_ms)}</span>
      </div>
      <label className="volume">
        🔊
        <input
          type="range"
          min={0}
          max={1}
          step={0.01}
          value={volume}
          onChange={(e) => {
            holdVolume(Number(e.target.value));
            sendVolume(Number(e.target.value));
          }}
          onPointerUp={releaseVolume}
          onPointerCancel={releaseVolume}
          onBlur={releaseVolume}
          onKeyUp={releaseVolume}
        />
      </label>
    </header>
  );
}
