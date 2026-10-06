import { useState } from "react";
import { clock } from "../format";
import { send } from "../socket";
import { useStore } from "../store";

export function TopBar() {
  const p = useStore((s) => s.player);
  const [drag, setDrag] = useState<number | null>(null);
  const position = drag ?? p.position_ms;

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
          onChange={(e) => setDrag(Number(e.target.value))}
          onPointerUp={() => {
            if (drag !== null) send({ type: "seek", position_ms: drag });
            setDrag(null);
          }}
          onKeyUp={() => {
            if (drag !== null) send({ type: "seek", position_ms: drag });
            setDrag(null);
          }}
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
          value={p.volume}
          onChange={(e) => send({ type: "volume", volume: Number(e.target.value) })}
        />
      </label>
    </header>
  );
}
