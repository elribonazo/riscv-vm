import { defineConfig } from "tsup";
import { wasmPlugin } from "./";

// JIT Worker needs to be self-contained (no code splitting) because
// it runs in a separate thread and can't resolve relative imports.
// We use ESM format but bundle everything inline.
export default defineConfig({
    entry: { "jit-worker": "jit-worker.ts" },
    format: ["esm"], // ESM works in workers without module.exports issues
    outDir: "build",
    outExtension: () => ({ js: ".js" }),
    target: "esnext",
    minify: false,
    platform: "browser",
    clean: false,
    splitting: false, // No code splitting - single file
    treeshake: true,
    sourcemap: true,
    esbuildPlugins: [wasmPlugin],
    noExternal: [/.*/], // Bundle everything including WASM bindings
    esbuildOptions: (options) => {
        options.platform = "browser";
        // Bundle all imports inline
        options.bundle = true;
    },
});

