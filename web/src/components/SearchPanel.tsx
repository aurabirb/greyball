import { useState } from "react";
import { clock } from "../format";
import { send } from "../socket";
import { useStore } from "../store";

export function SearchPanel() {
  const search = useStore((s) => s.search);
  const clearSearch = useStore((s) => s.clearSearch);
  const [text, setText] = useState("");

  return (
    <main>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          send({ type: "search", text });
          if (!text.trim()) clearSearch();
        }}
      >
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder="Search all sources" autoFocus />
        <button type="submit">Search</button>
      </form>
      {search && (
        <>
          <p className="status">
            {search.hits.length} results
            {search.pending.length > 0 && ` · searching ${search.pending.join(", ")}…`}
          </p>
          <ul className="results">
            {search.hits.map((t, i) => (
              <li key={`${t.id}-${i}`}>
                <div className="info">
                  <strong>{t.title}</strong>
                  <span>
                    {t.artists.join(", ")}
                    {t.album && ` · ${t.album}`}
                  </span>
                </div>
                <span className="sources">{t.sources.join(", ")}</span>
                <span className="duration">{t.duration_ms ? clock(t.duration_ms) : ""}</span>
                <button onClick={() => send({ type: "play", track: t.id })}>Play</button>
                <button onClick={() => send({ type: "enqueue", track: t.id })}>Enqueue</button>
              </li>
            ))}
          </ul>
        </>
      )}
    </main>
  );
}
