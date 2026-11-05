import { expect, it } from 'vitest';
import { spawnSync } from 'node:child_process';
import process from 'node:process';
import { requiredVariableReference } from '../src/variableReferences';

// Catches source substitution, unquoted shell data, and colon-question treating empty as missing.
it.skipIf(process.platform === 'win32').each(['sh', 'python3', 'node'])(
  'executes %s references as data, permits empty values and fails when absent',
  (interpreter) => {
    const reference = requiredVariableReference('MESSAGE', `/usr/bin/${interpreter}`);
    expect(reference).toBeTruthy();
    const program = interpreter === 'node' ? process.execPath : `/bin/${interpreter}`;
    const executable = interpreter === 'python3' ? '/usr/bin/python3' : program;
    const script =
      interpreter === 'sh'
        ? `printf '%s' ${reference}`
        : interpreter === 'python3'
          ? `import sys; sys.stdout.write(${reference})`
          : `process.stdout.write(${reference})`;
    for (const value of ["literal '$HOME'\n$(printf must-stay-literal)", '']) {
      const result = spawnSync(executable, [interpreter === 'node' ? '-e' : '-c', script], {
        env: { PATH: '/usr/bin:/bin', CLAI_VAR_MESSAGE: value },
        encoding: 'utf8',
      });
      expect(result.status, result.stderr).toBe(0);
      expect(result.stdout).toBe(value);
    }
    const missing = spawnSync(executable, [interpreter === 'node' ? '-e' : '-c', script], {
      env: { PATH: '/usr/bin:/bin' },
      encoding: 'utf8',
    });
    expect(missing.status).not.toBe(0);
  }
);
