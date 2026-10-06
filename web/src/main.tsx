import { createRoot } from "react-dom/client";
import { App } from "./App";
import { connect } from "./socket";
import "./style.css";

connect();
createRoot(document.getElementById("root")!).render(<App />);
