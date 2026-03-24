import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { initWasm } from "./wasm";
import { App } from "./App";

initWasm().then(() => {
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
});
