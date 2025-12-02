#!/usr/bin/env bash
set -e  # Exit on any error

is_mac() {
  [[ "$OSTYPE" == "darwin"* ]]
}

PACKAGEJSON=./pkg/package.json
IMPORTFILE=./pkg/riscv_vm.js

echo "Building the rust library"
RUSTFLAGS=--cfg=web_sys_unstable_apis npx wasm-pack build  --target web 

if is_mac; then
  sed -i '' 's/"module": "ridb_core.js",/"main": "ridb_core.js",/' $PACKAGEJSON
  sed -i '' "/if (typeof module_or_path === 'undefined') {/,/}/d" $IMPORTFILE
else
  sed -i  's/"module": "ridb_core.js",/"main": "ridb_core.js",/' $PACKAGEJSON
  sed -i "/if (typeof module_or_path === 'undefined') {/,/}/d" $IMPORTFILE
fi

npx tsup --config tsup/tsup.cli.ts
npx tsup --config tsup/tsup.core.cjs.ts
npx tsup --config tsup/tsup.core.esm.ts
npx tsup --config tsup/tsup.core.cjs.ts --dts-only
npx tsup --config tsup/tsup.worker.ts
npx tsup --config tsup/tsup.node-worker.ts

