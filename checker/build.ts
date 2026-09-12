import { componentize } from '@bytecodealliance/componentize-js';
import { join, resolve } from 'node:path';
import { writeFile, readFile } from 'node:fs/promises';
const [directoryArgument, rootArgument] = process.argv.slice(2);
if (!directoryArgument || !rootArgument) throw new Error('usage: bun build.ts BUILD_DIR REPO_ROOT');
const directory = resolve(directoryArgument);
const root = resolve(rootArgument);
const adapter = join(root, 'guest-ts/adapter.ts');
await writeFile(join(directory, 'entry.ts'), `import {handler} from ${JSON.stringify(adapter)};\nimport * as definition from './definition.ts';\nconst guest = handler(definition);\nexport const run=guest.run, fold=guest.fold, call=guest.call;\n`);
const dependencySource = await readFile(join(directory,'dependencies.js'),'utf8');
const bundle = await Bun.build({
  entrypoints: [join(directory,'entry.ts')], target:'browser', format:'esm',
  external:['loom:host/effects'],
  plugins:[{name:'loom-effects',setup(build){build.onResolve({filter:/^loom:defs$/},()=>({path:'loom:defs',namespace:'loom-defs'}));build.onLoad({filter:/.*/,namespace:'loom-defs'},()=>({contents:dependencySource,loader:'js'}));build.onResolve({filter:/^loom$/},()=>({path:join(root,'guest-ts/index.ts')}));}}],
});
if (!bundle.success || !bundle.outputs[0]) throw new Error(bundle.logs.map(String).join('\n'));
const source = await bundle.outputs[0].text();
await writeFile(join(directory, 'bundle.js'), source);
const {component} = await componentize(source, {witPath:join(root,'wit/handler.wit'),worldName:'handler',disableFeatures:['stdio','random','clocks','http','fetch-event']});
await writeFile(join(directory,'component.wasm'), component);
