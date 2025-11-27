import createConfig from './';

export default createConfig({
  format: ['cjs'],
  entry: ['cli.ts'],
  external: ['yargs'],
  platform: 'node',
});
