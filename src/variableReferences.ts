export function requiredVariableReference(name: string, interpreter: string): string | null {
  const key = `CLAI_VAR_${name}`;
  const parts = interpreter.split('/');
  switch (parts[parts.length - 1]) {
    case 'sh': case 'bash': case 'zsh': return '"${' + key + '?Missing or unavailable}"';
    case 'python': case 'python3': return `__import__("os").environ["${key}"]`;
    case 'node': return `(() => { const value = process.env["${key}"]; if (value === undefined) throw new Error("Missing or unavailable"); return value; })()`;
    default: return null;
  }
}
