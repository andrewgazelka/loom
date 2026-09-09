import ts from 'typescript';
import type {EffectSet,TypeSig,ValueShape} from '../loom-guest-ts/protocol.generated';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const filename = '/loom/definition.ts';
const declarations = '/loom/loom.d.ts';
const protocolPath = '/loom/protocol.generated.ts';
const dependencyPath = '/loom/deps.d.ts';
const options: ts.CompilerOptions = {
  strict: true, noUncheckedIndexedAccess: true, exactOptionalPropertyTypes: true,
  noImplicitOverride: true, noImplicitReturns: true, noFallthroughCasesInSwitch: true,
  isolatedModules: true, verbatimModuleSyntax: true, types: [], lib: ['lib.es2023.d.ts'],
  target: ts.ScriptTarget.ES2023, module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
};
export interface Diagnostic { lang: 'ts'; file: string; line: number; col: number; code: string; message: string; snippet: string | null; hint: string | null }
export class Checker {
  private source = '';
  private dependencyDeclarations = '';
  private version = 0;
  private service: ts.LanguageService;
  constructor() {
    const shim = readFileSync(resolve(import.meta.dir, '../loom-guest-ts/loom.d.ts'), 'utf8');
    const protocol = readFileSync(resolve(import.meta.dir, '../loom-guest-ts/protocol.generated.ts'), 'utf8');
    const libraries = new Map<string,string>();
    const readLibrary = (path:string) => {
      if(libraries.has(path))return libraries.get(path);
      let source=ts.sys.readFile(path);
      if(source!==undefined && path.endsWith('/lib.es5.d.ts')) {
        const parsed=ts.createSourceFile(path,source,ts.ScriptTarget.ES2023,true);
        const statements=parsed.statements.map(statement=>{
          if(ts.isInterfaceDeclaration(statement)&&statement.name.text==='Math')return ts.factory.updateInterfaceDeclaration(statement,statement.modifiers,statement.name,statement.typeParameters,statement.heritageClauses,statement.members.filter(member=>!(member.name&&ts.isIdentifier(member.name)&&member.name.text==='random')));
          return statement;
        });
        source=ts.createPrinter().printFile(ts.factory.updateSourceFile(parsed,statements));
      }
      if(source!==undefined)libraries.set(path,source);
      return source;
    };
    const read = (path: string) => path === filename ? this.source : path === declarations ? shim : path === protocolPath ? protocol : path === dependencyPath ? this.dependencyDeclarations : readLibrary(path);
    this.service = ts.createLanguageService({
      getCompilationSettings: () => options,
      getScriptFileNames: () => [filename, declarations, protocolPath, dependencyPath],
      getScriptVersion: path => path === filename || path === dependencyPath ? String(this.version) : '0',
      getScriptSnapshot: path => { const text = read(path); return text === undefined ? undefined : ts.ScriptSnapshot.fromString(text); },
      getCurrentDirectory: () => '/loom', getDefaultLibFileName: ts.getDefaultLibFilePath,
      fileExists: path => path === filename || path === declarations || path === protocolPath || path === dependencyPath || ts.sys.fileExists(path),
      readFile: read, readDirectory: ts.sys.readDirectory,
    });
  }
  check(source: string, signatures: Record<string,TypeSig> = {}) {
    const shapeType = (shape:ValueShape):string => {
      switch(shape.type){
        case 'null':return 'null';case 'boolean':return 'boolean';case 'number':return 'number';case 'string':return 'string';case 'value':return 'Value';
        case 'array':return `Array<${shapeType(shape.items)}>`;
        case 'ref':return `Ref<${shapeType(shape.target)}>`;
        case 'object':return `{${Object.entries(shape.properties).map(([name,value])=>`${JSON.stringify(name)}${shape.optional.includes(name)?'?':''}:${shapeType(value!)};`).join('')}}`;
      }
    };
    this.dependencyDeclarations = `declare module "loom:defs" { import type {Def,Value,Ref} from "loom";\n${Object.entries(signatures).map(([name,sig])=>{
      const scanner=ts.createScanner(ts.ScriptTarget.ES2023,false);scanner.setText(name);
      if(scanner.scan()!==ts.SyntaxKind.Identifier||scanner.getTokenText()!==name||scanner.scan()!==ts.SyntaxKind.EndOfFileToken)throw new Error(`Invalid dependency alias ${name}`);
      const exported=sig.exports.find(item=>item.name==='default')??sig.exports[0];
      if(!exported)return `export const ${name}:Def<Value,Value>;`;
      const args=`[${exported.params.map(parameter=>shapeType(parameter.shape)).join(',')}]`;
      return `export const ${name}:Def<${args},${shapeType(exported.returns)}>;`;
    }).join('\n')} }`;
    this.source = source; this.version++;
    const program = this.service.getProgram()!;
    const file = program.getSourceFile(filename)!;
    const checker = program.getTypeChecker();
    const diagnostics: Diagnostic[] = [...this.service.getSyntacticDiagnostics(filename), ...this.service.getSemanticDiagnostics(filename)].map(d => {
      const loc = file.getLineAndCharacterOfPosition(d.start ?? 0);
      return { lang: 'ts', file: 'definition.ts', line: loc.line + 1, col: loc.character + 1, code: `TS${d.code}`, message: ts.flattenDiagnosticMessageText(d.messageText, '\n'), snippet: source.split('\n')[loc.line] ?? null, hint: null };
    });
    const reject = (node: ts.Node, message: string) => {
      const loc = file.getLineAndCharacterOfPosition(node.getStart(file));
      diagnostics.push({lang:'ts',file:'definition.ts',line:loc.line+1,col:loc.character+1,code:'LOOM_CLOSED',message,snippet:source.split('\n')[loc.line]??null,hint:'Use an ability from the loom module for effects.'});
    };
    const forbidden = new Set(['fetch','Date','eval','Function','globalThis','setTimeout','setInterval','queueMicrotask','process','require','WebAssembly']);
    const visit = (node: ts.Node) => {
      if (ts.isImportDeclaration(node) && (!ts.isStringLiteral(node.moduleSpecifier) || !['loom','loom:defs'].includes(node.moduleSpecifier.text))) reject(node, 'Only loom and declared loom:defs imports are available.');
      if (ts.isIdentifier(node) && forbidden.has(node.text)) reject(node, `${node.text} is unavailable in pure handlers.`);
      if ((ts.isPropertyAccessExpression(node) && node.name.text === 'constructor') || (ts.isElementAccessExpression(node) && ts.isStringLiteral(node.argumentExpression) && node.argumentExpression.text === 'constructor')) reject(node, 'Dynamic constructors are unavailable.');
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) reject(node, 'Dynamic imports are unavailable.');
      ts.forEachChild(node, visit);
    };
    visit(file);
    const printer = ts.createPrinter({removeComments:true,newLine:ts.NewLineKind.LineFeed});
    const localNames = new Map<ts.Symbol, string>();
    const registerBinding = (name: ts.BindingName) => {
      if (ts.isIdentifier(name)) {
        const symbol = checker.getSymbolAtLocation(name);
        if (symbol && !localNames.has(symbol)) localNames.set(symbol, `_local${localNames.size}`);
      } else for (const element of name.elements) if (ts.isBindingElement(element)) registerBinding(element.name);
    };
    const collectLocals = (node: ts.Node) => {
      if (ts.isParameter(node)) registerBinding(node.name);
      if (ts.isVariableDeclaration(node)) {
        const statement = node.parent.parent;
        if (!ts.isVariableStatement(statement) || !statement.modifiers?.some(m => m.kind === ts.SyntaxKind.ExportKeyword)) registerBinding(node.name);
      }
      ts.forEachChild(node, collectLocals);
    };
    collectLocals(file);
    const normalized = ts.transform(file, [context => root => {
      const visit: ts.Visitor = node => {
        if (ts.isExportSpecifier(node)) {
          const symbol=checker.getExportSpecifierLocalTargetSymbol(node);
          const name=symbol&&localNames.get(symbol);
          if(name)return ts.factory.updateExportSpecifier(node,node.isTypeOnly,ts.factory.createIdentifier(name),node.name);
        }
        if (ts.isShorthandPropertyAssignment(node)) {
          const symbol = checker.getShorthandAssignmentValueSymbol(node);
          const name = symbol && localNames.get(symbol);
          if (name) return ts.factory.createPropertyAssignment(node.name.text, ts.factory.createIdentifier(name));
        }
        if (ts.isBindingElement(node) && !node.propertyName && ts.isIdentifier(node.name) && ts.isObjectBindingPattern(node.parent)) {
          const symbol = checker.getSymbolAtLocation(node.name);
          const name = symbol && localNames.get(symbol);
          if (name) return ts.factory.updateBindingElement(node, node.dotDotDotToken, node.dotDotDotToken ? undefined : node.name, ts.factory.createIdentifier(name), node.initializer && ts.visitNode(node.initializer, visit) as ts.Expression);
        }
        if (ts.isIdentifier(node)) {
          const symbol = checker.getSymbolAtLocation(node);
          const name = symbol && localNames.get(symbol);
          if (name) return ts.factory.createIdentifier(name);
        }
        const visited = ts.visitEachChild(node, visit, context);
        if (ts.isBlock(visited)) return ts.factory.createBlock(visited.statements, true);
        if (ts.isObjectLiteralExpression(visited)) return ts.factory.createObjectLiteralExpression(visited.properties, false);
        if (ts.isArrayLiteralExpression(visited)) return ts.factory.createArrayLiteralExpression(visited.elements, false);
        if (ts.isStringLiteral(visited)) return ts.factory.createStringLiteral(visited.text, false);
        return visited;
      };
      return ts.visitNode(root, visit) as ts.SourceFile;
    }]);
    const canonical = printer.printFile(normalized.transformed[0]!);
    normalized.dispose();
    const shape = (type: ts.Type, depth = 0): object => {
      if (depth > 12) return {type:'value'};
      if (type.flags & ts.TypeFlags.StringLike) return {type:'string'};
      if (type.flags & ts.TypeFlags.NumberLike) return {type:'number'};
      if (type.flags & ts.TypeFlags.BooleanLike) return {type:'boolean'};
      if (type.flags & (ts.TypeFlags.Null | ts.TypeFlags.Void | ts.TypeFlags.Undefined)) return {type:'null'};
      if (checker.isArrayType(type) || checker.isTupleType(type)) {
        const element = checker.getTypeArguments(type as ts.TypeReference)[0];
        return {type:'array',items:element ? shape(element,depth+1) : {type:'value'}};
      }
      if (type.flags & ts.TypeFlags.Object) {
        const properties: Record<string,object> = {};
        const optional: string[] = [];
        if (type.getCallSignatures().length) return {type:'value'};
        for (const property of type.getProperties()) {
          properties[property.name] = shape(checker.getTypeOfSymbolAtLocation(property,file),depth+1);
          if (property.flags & ts.SymbolFlags.Optional) optional.push(property.name);
        }
        if ('$ref' in properties) return {type:'ref',target:{type:'value'}};
        return {type:'object',properties,optional};
      }
      return {type:'value'};
    };
    // Resolve provenance through symbols: spelling alone cannot distinguish aliases or shadows.
    type Effects = EffectSet;
    type Target = {kind:'loom'|'dependency';name:string}|{kind:'local';node:ts.FunctionLikeDeclaration};
    const unwrap = (expression:ts.Expression):ts.Expression => {
      while(ts.isParenthesizedExpression(expression)||ts.isAsExpression(expression)||ts.isTypeAssertionExpression(expression)||ts.isNonNullExpression(expression))expression=expression.expression;
      return expression;
    };
    const reassigned=new Set<ts.Symbol>();
    let propertyMutation=false;
    const mutations=(node:ts.Node) => {
      const target=ts.isBinaryExpression(node)&&node.operatorToken.kind>=ts.SyntaxKind.FirstAssignment&&node.operatorToken.kind<=ts.SyntaxKind.LastAssignment?node.left:(ts.isPrefixUnaryExpression(node)||ts.isPostfixUnaryExpression(node))&&(node.operator===ts.SyntaxKind.PlusPlusToken||node.operator===ts.SyntaxKind.MinusMinusToken)?node.operand:undefined;
      if(target){if(ts.isIdentifier(target)){const symbol=checker.getSymbolAtLocation(target);if(symbol)reassigned.add(symbol);}else propertyMutation=true;}
      ts.forEachChild(node,mutations);
    };
    mutations(file);
    const resolveTarget = (expression:ts.Expression, seen=new Set<ts.Symbol>()):Target|undefined => {
      expression=unwrap(expression);
      if(ts.isArrowFunction(expression)||ts.isFunctionExpression(expression))return {kind:'local',node:expression};
      if(ts.isPropertyAccessExpression(expression)||ts.isElementAccessExpression(expression)) {
        const base=resolveTarget(expression.expression,seen);
        const name=ts.isPropertyAccessExpression(expression)?expression.name.text:expression.argumentExpression&&ts.isStringLiteral(expression.argumentExpression)?expression.argumentExpression.text:undefined;
        if(base&&base.kind!=='local'&&name)return {...base,name:base.name?`${base.name}.${name}`:name};
      }
      const symbol=checker.getSymbolAtLocation(expression);
      if(!symbol||seen.has(symbol)||reassigned.has(symbol))return undefined;
      seen.add(symbol);
      for(const declaration of symbol.declarations??[]) {
        if(ts.isImportSpecifier(declaration)||ts.isNamespaceImport(declaration)) {
          const clause=ts.isImportSpecifier(declaration)?declaration.parent.parent:declaration.parent;
          const imported=clause.parent;
          if(ts.isImportDeclaration(imported)&&ts.isStringLiteral(imported.moduleSpecifier)) {
            const kind=imported.moduleSpecifier.text==='loom'?'loom':imported.moduleSpecifier.text==='loom:defs'?'dependency':undefined;
            if(kind)return {kind,name:ts.isImportSpecifier(declaration)?(declaration.propertyName??declaration.name).text:''};
          }
        }
        if(ts.isFunctionDeclaration(declaration)&&declaration.body)return {kind:'local',node:declaration};
        if(ts.isVariableDeclaration(declaration)&&declaration.initializer&&(declaration.parent.flags&ts.NodeFlags.Const))return resolveTarget(declaration.initializer,seen);
        if(ts.isExportSpecifier(declaration)) {
          const local=checker.getExportSpecifierLocalTargetSymbol(declaration);
          const value=local?.valueDeclaration;
          if(value&&ts.isFunctionDeclaration(value))return {kind:'local',node:value};
          if(value&&ts.isVariableDeclaration(value)&&value.initializer)return resolveTarget(value.initializer,seen);
        }
      }
      return undefined;
    };
    const summaries=new Map<ts.FunctionLikeDeclaration|ts.SourceFile,{labels:Set<string>;unknown:boolean;calls:Set<ts.FunctionLikeDeclaration>}>();
    const knownAbilities=new Set(['exec','llm','now','random','sleep','fs.list','fs.stat','fs.read','fs.snapshot','cas.put','cas.get','send','join']);
    const summarize=(node:ts.FunctionLikeDeclaration|ts.SourceFile) => {
      const existing=summaries.get(node);if(existing)return existing;
      const summary={labels:new Set<string>(),unknown:false,calls:new Set<ts.FunctionLikeDeclaration>()};summaries.set(node,summary);
      const dependency=(expression:ts.Expression|undefined) => {
        const target=expression&&resolveTarget(expression);
        const sig=target?.kind==='dependency'?signatures[target.name]:undefined;
        const exported=sig?.exports.find(item=>item.name==='default')??sig?.exports[0];
        const effects=sig?.effects??exported?.effects;
        if(!effects){summary.unknown=true;return;}
        effects.labels.forEach(label=>summary.labels.add(label));summary.unknown ||= effects.unknown;
      };
      const dereference=(expression:ts.Expression,seen=new Set<ts.Symbol>()):ts.Expression => {
        expression=unwrap(expression);
        if(ts.isIdentifier(expression)) {
          const symbol=checker.getSymbolAtLocation(expression);
          if(symbol&&!seen.has(symbol))for(const declaration of symbol.declarations??[])if(ts.isVariableDeclaration(declaration)&&declaration.initializer&&(declaration.parent.flags&ts.NodeFlags.Const)){seen.add(symbol);return dereference(declaration.initializer,seen);}
        }
        return expression;
      };
      const descriptors=(expression:ts.Expression|undefined) => {
        if(!expression){summary.unknown=true;return;}
        expression=dereference(expression);
        if(ts.isArrayLiteralExpression(expression))expression.elements.forEach(item=>descriptor(item));else summary.unknown=true;
      };
      const descriptor=(expression:ts.Expression|undefined) => {
        if(!expression){summary.unknown=true;return;}
        expression=dereference(expression);
        if(ts.isCallExpression(expression)) {
          const target=resolveTarget(expression.expression);
          if(target?.kind==='loom'&&target.name.endsWith('.desc')&&knownAbilities.has(target.name.slice(0,-5))){summary.labels.add(target.name.slice(0,-5));return;}
        }
        if(ts.isObjectLiteralExpression(expression)) {
          const properties=new Map<string,ts.Expression>();
          for(const property of expression.properties) {
            if(ts.isPropertyAssignment(property)&&(ts.isIdentifier(property.name)||ts.isStringLiteral(property.name)))properties.set(property.name.text,property.initializer);
            else {summary.unknown=true;return;}
          }
          const op=properties.get('op');
          if(op&&ts.isStringLiteral(dereference(op))) {
            const name=(dereference(op) as ts.StringLiteral).text;
            if(name==='all'||name==='race') {
              summary.labels.add(name);
              const args=properties.get('args');const value=args&&dereference(args);
              if(value&&ts.isObjectLiteralExpression(value)){const descs=value.properties.find(p=>ts.isPropertyAssignment(p)&&p.name.getText(file)==='descs');descriptors(descs&&ts.isPropertyAssignment(descs)?descs.initializer:undefined);}else summary.unknown=true;
            } else {summary.labels.add(name);if(['call','fork','spawn'].includes(name))summary.unknown=true;}
            return;
          }
        }
        summary.unknown=true;
      };
      const walk=(child:ts.Node) => {
        if(child!==node&&ts.isFunctionLike(child))return;
        // Destructuring and spread perform implicit reads/iteration. Getter and
        // iterator dispatch can run guest code without an explicit call node.
        if(ts.isObjectBindingPattern(child)||ts.isArrayBindingPattern(child)||ts.isSpreadAssignment(child)||ts.isSpreadElement(child)||ts.isForOfStatement(child))summary.unknown=true;
        if(ts.isBinaryExpression(child)&&child.operatorToken.kind===ts.SyntaxKind.EqualsToken&&(ts.isObjectLiteralExpression(child.left)||ts.isArrayLiteralExpression(child.left)))summary.unknown=true;
        if(ts.isPropertyAccessExpression(child)||ts.isElementAccessExpression(child)) {
          const symbol=checker.getSymbolAtLocation(ts.isPropertyAccessExpression(child)?child.name:child);
          for(const declaration of symbol?.declarations??[])if(ts.isGetAccessorDeclaration(declaration)&&declaration.body){summary.calls.add(declaration);summarize(declaration);}
        }
        if(ts.isCallExpression(child)||ts.isNewExpression(child)) {
          const target=resolveTarget(child.expression);
          if(target?.kind==='local'){summary.calls.add(target.node);summarize(target.node);}
          else if(target?.kind==='loom') {
            if(knownAbilities.has(target.name))summary.labels.add(target.name);
            else if(target.name==='perform')descriptor(child.arguments?.[0]);
            else if(target.name==='all'||target.name==='race'){summary.labels.add(target.name);descriptors(child.arguments?.[0]);}
            else if(['call','fork','spawn'].includes(target.name)){summary.labels.add(target.name);dependency(child.arguments?.[0]);}
            else if(!(target.name.endsWith('.desc')&&knownAbilities.has(target.name.slice(0,-5))))summary.unknown=true;
          } else summary.unknown=true;
        }
        ts.forEachChild(child,walk);
      };
      walk(node);return summary;
    };
    const exportEffects=(symbol:ts.Symbol):Effects => {
      const declaration=symbol.valueDeclaration??symbol.declarations?.[0];
      let target:Target|undefined;
      if(declaration&&ts.isFunctionDeclaration(declaration))target={kind:'local',node:declaration};
      else if(declaration&&ts.isVariableDeclaration(declaration)&&declaration.initializer)target=resolveTarget(declaration.initializer);
      else if(declaration&&ts.isExportSpecifier(declaration))target=resolveTarget(declaration.propertyName??declaration.name);
      else if(declaration&&ts.isExportAssignment(declaration))target=resolveTarget(declaration.expression);
      if(target?.kind!=='local')return {labels:[],unknown:true};
      const summary=summarize(target.node);
      const moduleEffects=summarize(file);
      let changed=true;
      while(changed){changed=false;for(const value of summaries.values())for(const call of value.calls){const other=summaries.get(call)!;for(const label of other.labels)if(!value.labels.has(label)){value.labels.add(label);changed=true;}if(other.unknown&&!value.unknown){value.unknown=true;changed=true;}}}
      return {labels:[...new Set([...summary.labels,...moduleEffects.labels])].sort(),unknown:summary.unknown||moduleEffects.unknown||propertyMutation};
    };
    const exported = checker.getSymbolAtLocation(file);
    const functions = exported ? checker.getExportsOfModule(exported).flatMap(symbol => {
      const type = checker.getTypeOfSymbolAtLocation(symbol, file);
      return type.getCallSignatures().map(signature => ({name:symbol.name,params:signature.parameters.map(p=>({name:p.name,shape:shape(checker.getTypeOfSymbolAtLocation(p,file))})),returns:shape(signature.getReturnType()),effects:exportEffects(symbol)}));
    }) : [];
    if (functions.length === 0 && diagnostics.length === 0) reject(file, 'Export a default function, a named function, or actor run/fold functions.');
    const moduleEffects=summarize(file);
    const effects:Effects={labels:[...new Set([...moduleEffects.labels,...functions.flatMap(item=>item.effects.labels)])].sort(),unknown:functions.length===0||moduleEffects.unknown||functions.some(item=>item.effects.unknown)};
    return {canonical,sig:{exports:functions,effects},diagnostics};
  }
}
if (import.meta.main) {
  const checker = new Checker();
  const { createInterface } = await import('node:readline');
  for await (const line of createInterface({input:process.stdin})) {
    try { const request = JSON.parse(line); console.log(JSON.stringify(checker.check(request.source,request.dep_sigs))); }
    catch(error) { console.log(JSON.stringify({error:String(error)})); }
  }
}
