const fs = require('node:fs');

fs.mkdirSync('out', { recursive: true });
fs.copyFileSync('input.bin', 'out/artifact.bin');
fs.appendFileSync(process.env.NX_FIXTURE_MARKER, 'executed\n');
