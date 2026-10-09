// Standard-library harness: require real invariant failures, not parse errors.
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const url = 'https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar';
const sha256 = '936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88';
const specs = dirname(fileURLToPath(import.meta.url));
// Keep downloaded binaries, logs and TLC state files outside the checkout.
const results = mkdtempSync(join(homedir(), 'agent-board-tlc-'));
console.log(`Full TLC reports: ${results}`);

try {
  const jar = join(results, 'tla2tools.jar');
  const download = spawnSync('curl', ['--fail', '--location', '--retry', '3',
    '--connect-timeout', '30', '--max-time', '180', '--output', jar, url], { encoding: 'utf8' });
  if (download.status !== 0) throw new Error(`TLC download failed: ${download.error ?? download.stderr}`);
  if (createHash('sha256').update(readFileSync(jar)).digest('hex') !== sha256) {
    throw new Error('tla2tools.jar SHA-256 mismatch; refusing to execute it');
  }

  for (const [config, expectedStatus] of [['Preview', 0], ['Current', 12], ['Broken', 12]]) {
    const run = spawnSync(process.env.JAVA_BIN ?? 'java', ['-XX:+UseParallelGC', '-Xmx512m',
      '-cp', jar, 'tlc2.TLC', '-workers', '1', '-seed', '1', '-fp', '0',
      '-config', join(specs, `${config}.cfg`), '-metadir', join(results, config),
      join(specs, 'DispatchJoin.tla')], { cwd: results, encoding: 'utf8', timeout: 120_000 });
    const output = `${run.stdout ?? ''}${run.stderr ?? ''}`;
    writeFileSync(join(results, `${config}.log`), output);
    console.log(`\n${config}: TLC exit ${run.status}\n${output}`);
    const message = expectedStatus === 0
      ? 'Model checking completed. No error has been found.'
      : 'Invariant OneLiveRun is violated.';
    if (run.error || run.status !== expectedStatus || !output.includes(message)) {
      throw new Error(`${config}: expected exit ${expectedStatus} and '${message}', not a tooling failure`);
    }
  }
  console.log('Expected results: Preview safe; Current and Broken violate OneLiveRun.');
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
