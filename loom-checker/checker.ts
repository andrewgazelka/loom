import ts from 'typescript';
import type {TypeSig,ValueShape} from '../loom-guest-ts/protocol.generated';
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
    const exported = checker.getSymbolAtLocation(file);
    const functions = exported ? checker.getExportsOfModule(exported).flatMap(symbol => {
      const type = checker.getTypeOfSymbolAtLocation(symbol, file);
      return type.getCallSignatures().map(signature => ({name:symbol.name,params:signature.parameters.map(p=>({name:p.name,shape:shape(checker.getTypeOfSymbolAtLocation(p,file))})),returns:shape(signature.getReturnType())}));
    }) : [];
    if (functions.length === 0 && diagnostics.length === 0) reject(file, 'Export a default function, a named function, or actor run/fold functions.');
    return {canonical,sig:{exports:functions},diagnostics};
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
