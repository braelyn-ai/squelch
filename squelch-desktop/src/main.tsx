import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";

// Fonts bundled locally — NO font CDNs (privacy). Hanken Grotesk (body/UI) and
// Newsreader (editorial serif: brand + hero) are the mockup's exact latin
// variable-font woff2 subsets, self-hosted under src/assets/fonts and declared
// via @font-face in global.css (Vite fingerprints + bundles them). IBM Plex
// Mono still carries genuine tabular data (timestamps, patterns, 2FA codes) and
// stays on @fontsource.
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "@fontsource/ibm-plex-mono/600.css";

import "./styles/global.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
