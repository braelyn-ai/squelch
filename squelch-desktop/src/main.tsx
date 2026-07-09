import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";

// Bundle IBM Plex locally (@fontsource) — NO font CDNs (privacy). Sans is the
// body face; Mono carries data (timestamps, patterns, meters, 2FA codes).
// Weights 400/500/600 cover body / medium / semibold engraved labels.
import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "@fontsource/ibm-plex-mono/600.css";

import "./styles/global.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
