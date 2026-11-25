// Fails unless `npm pack` would ship exactly the files a consumer needs.
// Runs the prepack build, so dist is fresh.
import { execFileSync } from 'node:child_process';

const expected = [
  'CHANGELOG.md', 'LICENSE', 'README.md', 'package.json',
  'dist/index.d.ts', 'dist/index.js', 'dist/types.d.ts', 'dist/types.js',
];
// On Windows npm is npm.cmd, which Node runs only through a shell. The
// arguments are constants, so the shell adds no risk.
const [pack] = JSON.parse(execFileSync('npm', ['pack', '--dry-run', '--json'], {
  encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], shell: process.platform === 'win32',
}));
const files = pack.files.map(file => file.path).sort();
if (JSON.stringify(files) !== JSON.stringify([...expected].sort())) {
  console.error(`npm pack would ship:\n  ${files.join('\n  ')}\nexpected:\n  ${expected.join('\n  ')}`);
  process.exit(1);
}
if (pack.name !== '@beatzball/sandpiper-client') {
  console.error(`unexpected package name ${pack.name}`);
  process.exit(1);
}
console.log(`${pack.name}@${pack.version}: ${files.length} files, ${pack.size} bytes packed`);
