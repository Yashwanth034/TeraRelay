#!/usr/bin/env node
// Exercise the real App.tsx saved-session startup effect with mocked native calls.
// Never opens Telegram, launches TeraRelay or reads the user's session.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {createRequire} = require('node:module');
const appRequire = createRequire(path.resolve(__dirname, '../app/package.json'));
const ts = appRequire('typescript');
const source = fs.readFileSync(path.resolve(__dirname, '../app/src/App.tsx'), 'utf8');
const ast = ts.createSourceFile('App.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
let effect;
function walk(node) {
  if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect' &&
      node.arguments[0]?.getText(ast).includes('cmd_saved_telegram_session_exists')) {
    assert.equal(effect, undefined, 'one startup effect should own session restoration');
    effect = node.arguments[0].getText(ast);
  }
  ts.forEachChild(node, walk);
}
walk(ast);
assert.ok(effect, 'saved-session recovery effect exists');
assert.ok(!effect.includes('cmd_check_connection'), 'login restoration must not reset the live runner');
assert.ok(!effect.includes('cmd_logout'), 'restoration never signs out');
const transpiled = ts.transpileModule(
  'const startupEffect = ' +
    effect.replaceAll('import.meta.env.DEV', 'false').replaceAll('import.meta.env.VITE_TERA_QA_FEATURE_A', 'undefined') +
    '; startupEffect;',
  {compilerOptions: {target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS}}
).outputText;

const tick = () => new Promise(resolve => setImmediate(resolve));
async function runScenario({sessionExists = true, apiId = '12345', connected = [true], manualSignIn = false}) {
  const states = [];
  const details = [];
  const called = [];
  let index = 0;
  const context = {
    manualSignIn,
    setAuthStatus: state => states.push(state),
    setSessionRecoveryDetail: value => details.push(value),
    console: {warn: () => {}},
    load: async () => ({get: async () => apiId}),
    invoke: async (name, args) => {
      called.push(name);
      if (name === 'cmd_saved_telegram_session_exists') return sessionExists;
      if (name === 'cmd_migrate_legacy_api_credentials') return {};
      if (name === 'cmd_restore_saved_telegram_session') {
        assert.equal(args.apiId, Number(apiId));
        const result = connected[Math.min(index++, connected.length - 1)];
        if (result instanceof Error) throw result;
        return result;
      }
      throw new Error('Unexpected native command: ' + name);
    },
    setTimeout: callback => setImmediate(callback),
    clearTimeout: clearImmediate,
    globalThis: {},
  };
  const cleanup = vm.runInNewContext(transpiled, context)();
  for (let i = 0; i < 30 && !states.includes('authenticated') &&
       !(states.includes('unauthenticated')) && index < connected.length; i++) await tick();
  cleanup?.();
  await tick();
  return {states, details, called, attempts: index};
}
(async () => {
  const happy = await runScenario({});
  assert.ok(happy.states.includes('authenticated'));
  assert.ok(!happy.states.includes('unauthenticated'));

  const transient = await runScenario({connected: [new Error('offline'), true]});
  assert.ok(transient.states.includes('authenticated'), 'transient error eventually recovers');
  assert.ok(!transient.states.includes('unauthenticated'), 'network error cannot show phone login');
  assert.equal(transient.attempts, 2);
  assert.ok(transient.details.some(detail => detail.includes('retrying automatically')));

  const noFile = await runScenario({sessionExists: false});
  assert.ok(noFile.states.includes('unauthenticated'), 'no saved session permits phone login');
  assert.ok(!noFile.called.includes('cmd_restore_saved_telegram_session'));

  const missingId = await runScenario({apiId: null, connected: [true]});
  assert.ok(missingId.states.includes('restoring'), 'missing API ID with file does not falsely logout');
  assert.ok(!missingId.states.includes('unauthenticated'));

  const manual = await runScenario({manualSignIn: true});
  assert.deepEqual(manual.states, ['unauthenticated']);
  console.log('PASS saved session stays authenticated, retries transient failures, never auto-logs out, handles missing file/ID, manual phone login');
})().catch(error => {console.error(error); process.exitCode = 1;});
