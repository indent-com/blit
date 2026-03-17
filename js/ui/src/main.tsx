import { render } from "solid-js/web";
import { initWasm } from "./wasm";
import { connectConfigWs } from "./storage";
import { App } from "./App";

connectConfigWs();

initWasm().then((wasm) => {
  render(() => <App wasm={wasm} />, document.getElementById("root")!);
});
