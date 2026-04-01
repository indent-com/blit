import { createRoot } from "react-dom/client";
import { initWasm } from "./wasm";
import { connectConfigWs } from "./storage";
import { App } from "./App";

connectConfigWs();

initWasm().then((wasm) => {
  createRoot(document.getElementById("root")!).render(<App wasm={wasm} />);
});
