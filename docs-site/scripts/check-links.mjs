import { readFileSync, readdirSync, existsSync, statSync } from 'node:fs';
import { resolve, dirname, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { load } from 'cheerio';

const site = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(site, 'dist'), repo = resolve(site, '..');
const walk = dir => readdirSync(dir, {withFileTypes:true}).flatMap(e=>e.isDirectory()?walk(resolve(dir,e.name)):[resolve(dir,e.name)]);
const files = walk(dist).filter(p=>p.endsWith('.html'));
const pages = new Map(files.map(p=>[p,load(readFileSync(p,'utf8'))]));
const problems = new Set();
let checked = 0;
for (const [path,$] of pages) {
  const url = new URL(relative(dist,path).replace(/index\.html$/, ''), 'http://docs.local/');
  for (const el of $('a[href], img[src], script[src], link[href]').toArray()) {
    const value = $(el).attr('href') ?? $(el).attr('src');
    if (!value || /^(mailto:|tel:|data:|javascript:)/.test(value)) continue;
    let target;
    try { target = new URL(value,url); } catch { problems.add(`${relative(dist,path)}: invalid URL ${value}`); continue; }
    if (target.hostname === 'github.com' && target.pathname.startsWith('/chalk-ai/chalk-router/blob/main/')) {
      const sourcePath = resolve(repo,decodeURIComponent(target.pathname.split('/blob/main/')[1]));
      if (!sourcePath.startsWith(repo+'/') || !existsSync(sourcePath)) problems.add(`Missing repository source: ${value}`);
      else if (/^#L\d+$/.test(target.hash) && Number(target.hash.slice(2)) > readFileSync(sourcePath,'utf8').split('\n').length) problems.add(`Source line out of range: ${value}`);
      continue;
    }
    if (target.origin !== url.origin) continue;
    let targetPath = resolve(dist,'.'+decodeURIComponent(target.pathname));
    if (!targetPath.startsWith(dist+'/') && targetPath !== dist) { problems.add(`Escaping local path: ${value}`); continue; }
    if (existsSync(targetPath) && statSync(targetPath).isDirectory()) targetPath = resolve(targetPath,'index.html');
    if (!existsSync(targetPath)) { problems.add(`${relative(dist,path)}: missing ${value}`); continue; }
    checked++;
    if (target.hash && pages.has(targetPath)) {
      const id = decodeURIComponent(target.hash.slice(1));
      const doc = pages.get(targetPath);
      if (!doc('[id]').toArray().some(e=>doc(e).attr('id')===id)) problems.add(`${relative(dist,path)}: missing fragment ${value}`);
    }
  }
}
if (problems.size) { console.error([...problems].join('\n')); process.exit(1); }
console.log(`Checked ${files.length} HTML pages and ${checked} local links/assets, plus repository source paths and line bounds.`);
