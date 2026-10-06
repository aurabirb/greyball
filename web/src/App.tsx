import { SearchPanel } from "./components/SearchPanel";
import { TopBar } from "./components/TopBar";
import { useStore } from "./store";

export function App() {
  const connected = useStore((s) => s.connected);
  return (
    <>
      <TopBar />
      {!connected && <p className="offline">Reconnecting to medley…</p>}
      <SearchPanel />
    </>
  );
}
