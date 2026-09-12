import { test, expect } from "bun:test";
import { Client, AuthenticationError, casListing } from "../ui/src/lib/api";

test("every HTTP path reports typed authentication failure, then valid CAS reconnect works", async () => {
 const server=Bun.serve({port:0,fetch(request){
  if(request.headers.get("Authorization") !== "Bearer valid") return new Response("Unauthorized",{status:401});
  const path=new URL(request.url).pathname;
  if(path.endsWith("/command"))return Response.json({ok:true,seq:1,result:{items:[],next_cursor:null}});
  return new Response("bytes");
 }});
 try {
  for(const path of ["request","text","bytes"]){
   let rejected=0;
   const bad=new Client(server.url.origin,"expired",()=>rejected++);
   const action=()=>path==="request"?bad.command("cas.list"):path==="text"?bad.text("hash"):bad.bytes("hash");
   await expect(action()).rejects.toBeInstanceOf(AuthenticationError);
   await expect(action()).rejects.toBeInstanceOf(AuthenticationError);
   expect(rejected).toBe(1);
  }
  const good=new Client(server.url.origin,"valid");
  expect(casListing(await good.command("cas.list")).items).toEqual([]);
  expect(await good.text("hash")).toBe("bytes");
  expect(new TextDecoder().decode(await good.bytes("hash"))).toBe("bytes");
 } finally {server.stop(true);}
});
test("indirect result resolution also triggers authentication recovery", async()=>{
 let rejected=0;
 const server=Bun.serve({port:0,fetch(request){return new URL(request.url).pathname.endsWith("/command")?Response.json({ok:true,seq:1,result:{$ref:"hash"}}):new Response("Unauthorized",{status:401});}});
 try{const client=new Client(server.url.origin,"valid",()=>rejected++);await expect(client.command("call")).rejects.toBeInstanceOf(AuthenticationError);expect(rejected).toBe(1);}finally{server.stop(true);}
});
test("disposed connection cannot surface a late unauthorized response",async()=>{
 let rejected=0;
 const server=Bun.serve({port:0,async fetch(){await Bun.sleep(20);return new Response("Unauthorized",{status:401});}});
 try{const client=new Client(server.url.origin,"old",()=>rejected++);const result=client.command("cas.list");client.dispose();await expect(result).rejects.toBeDefined();expect(rejected).toBe(0);}finally{server.stop(true);}
});
