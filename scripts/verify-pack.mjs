// 发布包体检：确认 npm 打出来的包里该有的都有、不该有的一样没有。
//
//   node scripts/verify-pack.mjs
//
// 只依赖 Node 内置模块，不需要 npm install，所以 CI 和干净环境都能直接跑。
// 检查项：
//   1. package.json 不再是 private，发布字段齐全
//   2. 包里有 bin/ 的引擎，且大小合理（不是空壳、不是旧版）
//   3. 必备文件齐全（lib、patch、LICENSE、README、CHANGELOG）
//   4. 没有混进源码、编译产物、备份、依赖目录
import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const pkg = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8'));

let failed = 0;
const pass = (m) => console.log(`  PASS  ${m}`);
const fail = (m) => {
  failed++;
  console.error(`  FAIL  ${m}`);
};
const check = (cond, okMsg, badMsg) => (cond ? pass(okMsg) : fail(badMsg));

// ---- 1. 发布字段 ----
console.log('=== 发布字段 ===');
check(pkg.private === undefined, 'package.json 已不是 private', 'package.json 还是 private，npm publish 会被拒');
for (const field of ['name', 'version', 'description', 'license', 'author', 'repository', 'keywords', 'files']) {
  check(Boolean(pkg[field]), `有 ${field}`, `缺发布字段 ${field}`);
}
check(pkg.publishConfig?.access === 'public', 'publishConfig.access = public', 'publishConfig.access 不是 public');
check(Array.isArray(pkg.files) && pkg.files.includes('bin'), 'files 白名单包含 bin', 'files 白名单没包含 bin，用户装完会缺引擎');

// ---- 2. 实际打包 ----
console.log('=== 打包内容 ===');
let info;
try {
  const out = execSync('npm pack --dry-run --json', {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const json = JSON.parse(out.slice(out.indexOf('{')));
  info = json[pkg.name] ?? Object.values(json)[0];
} catch (e) {
  fail(`npm pack 失败：${e.message}`);
  process.exit(1);
}

const paths = info.files.map((f) => f.path);
const has = (p) => paths.includes(p);

check(Boolean(info.id), `包标识 ${info.id}`, '没拿到包标识');
check(has('bin/screen-control-engine.exe'), '包里有 bin/screen-control-engine.exe', '包里没有引擎 exe，用户装完用不了');

const exe = info.files.find((f) => f.path === 'bin/screen-control-engine.exe');
const exeSize = exe?.size ?? 0;
check(exeSize > 1_000_000, `引擎大小正常 ${exeSize} B`, `引擎大小可疑 ${exeSize} B（小于 1 MB，可能是坏的或旧版）`);

for (const must of ['lib/index.js', 'cordis.patch.yml', 'LICENSE', 'README.md', 'CHANGELOG.md', 'package.json']) {
  check(has(must), `包含 ${must}`, `缺必备文件 ${must}`);
}

// ---- 3. 不许混进来的东西 ----
const forbidden = [
  [/^rust-engine\//, 'Rust 源码/编译产物'],
  [/^node_modules\//, '依赖目录'],
  [/^src\//, 'TypeScript 源码'],
  [/^scripts\//, '开发脚本'],
  [/\.bak-/, '备份文件'],
  [/\btarget\b/, 'Rust target 目录'],
  [/\.tgz$/, '上一轮的打包产物'],
  [/\.map$/, 'source map'],
];
for (const [re, label] of forbidden) {
  const hit = paths.filter((p) => re.test(p));
  check(hit.length === 0, `没有混进${label}`, `混进了${label}：${hit.slice(0, 5).join(', ')}`);
}

console.log('');
console.log(`包：${info.id}  文件数 ${info.files.length}  压缩后 ${(info.size / 1048576).toFixed(2)} MB  解包后 ${(info.unpackedSize / 1048576).toFixed(2)} MB`);
if (failed > 0) {
  console.error(`结果：${failed} 项失败`);
  process.exit(1);
}
console.log('结果：全部通过');
