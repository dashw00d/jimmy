import { execFileSync } from 'node:child_process';
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const web = fileURLToPath(new URL('./', import.meta.url));
const out = join(web, 'dist');
const target = resolve(root, process.env.CARGO_TARGET_DIR ?? 'target');
const cactusRelease = 'https://huggingface.co/Cactus-Compute/cactus/resolve/f4ab2741203a282c6e7b89983de2b5e5b2e805a8/releases/4.1.4';
const artifact = async (name, hash) => {
  const cache = join(web, 'vendor', name);
  let contents;
  try { contents = await readFile(cache); } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    const local = process.env.JIMMY_CACTUS_ARTIFACTS;
    if (local) contents = await readFile(join(local, name));
    else {
      const response = await fetch(`${cactusRelease}/${name}`, { signal: AbortSignal.timeout(90_000) });
      if (!response.ok) throw new Error(`Fetch ${name}: HTTP ${response.status}`);
      contents = Buffer.from(await response.arrayBuffer());
    }
  }
  if (createHash('sha256').update(contents).digest('hex') !== hash) throw new Error(`SHA-256 mismatch for ${name}`);
  await mkdir(join(cache, '..'), { recursive: true });
  await writeFile(cache, contents);
  return contents;
};

const types = execFileSync('cargo', ['run', '--quiet', '-p', 'jimmy-proto', '--features', 'typescript', '--example', 'typescript'], { cwd: root });
await writeFile(join(web, 'src/protocol.ts'), types);
execFileSync('cargo', ['build', '--release', '--target', 'wasm32-unknown-unknown', '-p', 'jimmy-web'], { cwd: root, stdio: 'inherit' });
execFileSync(process.env.WASM_BINDGEN ?? 'wasm-bindgen', [
  '--target', 'web', '--out-dir', join(out, 'core'),
  join(target, 'wasm32-unknown-unknown', 'release', 'jimmy_web.wasm'),
], { stdio: 'inherit' });
execFileSync(process.execPath, [join(web, 'node_modules/typescript/bin/tsc')], { cwd: web, stdio: 'inherit' });
await mkdir(join(out, 'cactus'), { recursive: true });
const [script, wasm, license] = await Promise.all([
  artifact('wasm/cactus.js', '707341e44776ab0eb8dfc8f6a579ed5ac41e2ffc6419a18012f61619a6dfc70c'),
  artifact('wasm/cactus.wasm', 'fae56a5aae883224e8b196d2bc2fcd6ba1db5347ed2fcfd733f545775c7d3be0'),
  artifact('LICENSE', 'cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30'),
]);
await Promise.all([
  writeFile(join(out, 'cactus/cactus.js'), Buffer.concat([script, Buffer.from('\nexport default createCactus;\n')])),
  writeFile(join(out, 'cactus/cactus.wasm'), wasm),
  writeFile(join(out, 'cactus/LICENSE'), license),
  cp(join(web, 'node_modules/phonon-tts'), join(out, 'phonon'), { recursive: true }),
]);
