import { test, expect } from 'bun:test';
import { LoomMcpClient, McpTransportError } from './mcp-client';

test('negotiates protocol, decodes chunked SSE, preserves diagnostics, closes session', async () => {
  let closed = false;
  const server = Bun.serve({port:0, async fetch(request) {
    if (request.method === 'DELETE') { closed = true; return new Response(null, {status:204}); }
    const call = await request.json() as {id?:number;method:string};
    if (call.method === 'initialize') return Response.json({jsonrpc:'2.0',id:call.id,result:{protocolVersion:'2025-03-26'}},{headers:{'mcp-session-id':'session'}});
    expect(request.headers.get('mcp-session-id')).toBe('session');
    expect(request.headers.get('mcp-protocol-version')).toBe('2025-03-26');
    if (!call.id) return new Response(null,{status:202});
    const reply = {jsonrpc:'2.0',id:call.id,result:{content:[{type:'text',text:JSON.stringify({ok:false,seq:2,result:null,diagnostics:[{message:'type mismatch'}]})}]}};
    const bytes = new TextEncoder().encode(`: keepalive\r\n\r\ndata: ${JSON.stringify(reply)}\r\n\r\n`);
    return new Response(new ReadableStream({start(controller) { for (let i=0;i<bytes.length;i+=3) controller.enqueue(bytes.slice(i,i+3)); controller.close(); }}),{headers:{'content-type':'text/event-stream'}});
  }});
  const client = new LoomMcpClient({endpoint:server.url.origin,token:'test'});
  try { await client.connect(); const reply=await client.callTool('loom_define',{}); expect(reply.ok).toBe(false); expect(reply.diagnostics).toHaveLength(1); await client.close(); expect(closed).toBe(true); }
  finally { server.stop(true); }
});

test('tool errors remain transport errors', async () => {
  const server=Bun.serve({port:0,async fetch(request) { const call=await request.json() as {id:number}; return Response.json({jsonrpc:'2.0',id:call.id,result:{isError:true,content:[{type:'text',text:'failed'}]}}); }});
  try { const client=new LoomMcpClient({endpoint:server.url.origin,token:'test'}); await expect(client.callTool('loom_define',{})).rejects.toBeInstanceOf(McpTransportError); }
  finally { server.stop(true); }
});
