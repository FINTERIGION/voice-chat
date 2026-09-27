import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import Subtitle from "./routes/Subtitle";
import { I18nProvider } from "./lib/i18n";
import { UpdateProvider } from "./lib/update";
import "./index.css";

// The subtitle window loads the same `index.html` as the main one (see
// `subtitle::open` on the Rust side) — there is no separate build target —
// so which UI renders is decided here, by the window's own label, rather
// than by two entry points.
const isSubtitleWindow = getCurrentWindow().label === "subtitle";

if (!isSubtitleWindow) {
  document.documentElement.classList.add("app-shell");
}

// No React.StrictMode: its dev-only double-invoke of effects races Tauri's
// async listen()/unlisten() (see subscribe() in lib/ipc.ts) and was causing
// every chat event to be delivered twice in `tauri dev`. That double-invoke
// never happens in a production build anyway, so this only affects dev-mode
// fidelity, not the shipped app's correctness.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <I18nProvider>
    {isSubtitleWindow ? (
      <Subtitle />
    ) : (
      <UpdateProvider>
        <App />
      </UpdateProvider>
    )}
  </I18nProvider>,
);
