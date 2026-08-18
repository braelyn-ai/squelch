import { createRoot } from "react-dom/client";
import { App } from "./App";

// ONE PAGE, one bundle, no router dep. /waitlist is a state of App rather than
// a document of its own, so the path is read there (and written back with
// pushState) instead of picking a component here.
createRoot(document.getElementById("root")!).render(<App />);
