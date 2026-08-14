import { createRoot } from "react-dom/client";
import { App, WaitlistPage } from "./App";

// One bundle, one index.html fallback: the path picks the page, no router dep.
const page = location.pathname === "/waitlist" ? <WaitlistPage /> : <App />;

createRoot(document.getElementById("root")!).render(page);
