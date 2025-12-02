import createConfig from './';

export default createConfig({
  format: ['cjs'],
  entry: ['node-worker.ts'],
  external: [],
  platform: 'node',
  // Don't bundle the WASM bindings - they're loaded dynamically
  noExternal: [],
});

