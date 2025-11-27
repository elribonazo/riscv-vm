import wasmBuffer from "./pkg/riscv_vm_bg.wasm";

let loaded: typeof import("./pkg/riscv_vm") | undefined;

export async function WasmInternal() {
  if (!loaded) {
    const module = await import("./pkg/riscv_vm");
    const wasmInstance = module.initSync(wasmBuffer);
    await module.default(wasmInstance);
    loaded = module;
  }
  return loaded;
}


export  { NetworkStatus, WasmVm } from "./pkg/riscv_vm";