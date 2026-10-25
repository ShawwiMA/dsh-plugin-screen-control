// 引擎版本守卫：保证发布包 bin/ 里的 exe 和 rust-engine 的版本对得上。
//
//   node scripts/sync-engine.mjs          同步：把 target/release 的 exe 复制进 bin/
//   node scripts/sync-engine.mjs --check  只读校验，不通过退出 1（prepack 用）
//
// 三道检查：
//   1. package.json 和 rust-engine/Cargo.toml 的版本号必须一致
//   2. bin/ 里必须有引擎（发布包只带 bin，用户不会去编译 Rust）
//   3. 如果 target/release 编译过，bin 的 exe 必须和它字节一致
import { readFileSync, existsSync, copyFileSync, statSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const checkOnly = process.argv.includes('--check');
const EXE = 'screen-control-engine.exe';
const shipped = resolve(root, 'bin', EXE);
const built = resolve(root, 'rust-engine/target/release', EXE);
const cargoToml = resolve(root, 'rust-engine/Cargo.toml');

const fail = (msg) => {
  console.error(`[sync-engine] ✗ ${msg}`);
  process.exit(1);
};
const ok = (msg) => console.log(`[sync-engine] ✓ ${msg}`);
const sha = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');
const shortSha = (p) => sha(p).slice(0, 12);

// 1. 版本一致性
const pkgVersion = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8')).version;
if (!existsSync(cargoToml)) fail(`找不到 ${cargoToml}`);
const cargoVersion = readFileSync(cargoToml, 'utf8').match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1];
if (!cargoVersion) fail('rust-engine/Cargo.toml 里没读到 version');
if (pkgVersion !== cargoVersion) {
  fail(`版本不一致：package.json=${pkgVersion}，Cargo.toml=${cargoVersion}。先统一再发布。`);
}
ok(`版本一致 ${pkgVersion}`);

// 2. bin 必须有引擎
if (!existsSync(shipped)) fail(`bin/${EXE} 不存在，发布包里会缺引擎`);
const shippedSize = statSync(shipped).size;

// 3. 与 target/release 比对
if (!existsSync(built)) {
  ok(`target/release 未编译，跳过 hash 比对（bin 引擎 ${shippedSize} B，${shortSha(shipped)}…）`);
} else if (shortSha(shipped) === shortSha(built)) {
  ok(`bin 引擎与 target/release 一致（${shippedSize} B，${shortSha(shipped)}…）`);
} else if (checkOnly) {
  fail(
    `bin 引擎过期，和 target/release 不一致：\n` +
      `  bin          = ${shortSha(shipped)}… ${shippedSize} B\n` +
      `  target/release = ${shortSha(built)}… ${statSync(built).size} B\n` +
      `先跑 npm run sync-engine`,
  );
} else {
  const before = shortSha(shipped);
  copyFileSync(built, shipped);
  if (shortSha(shipped) !== shortSha(built)) fail('复制后 hash 仍不一致，复制失败');
  ok(`已同步 bin 引擎 ${before}… → ${shortSha(shipped)}…（${statSync(shipped).size} B）`);
}
