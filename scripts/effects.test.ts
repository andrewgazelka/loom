import {test,expect} from "bun:test";
import {effectRows} from "../loom-ui/src/lib/effects";
import type {LogEvent} from "../loom-ui/src/lib/api";
const event=(seq:number,type:string,extra:Record<string,unknown>={}):LogEvent=>({seq,actor:"system",ts:0,event:{type,scope:"call/a",occurrence:1,desc_hash:"hash",...extra}});
test("completion joins invocation, preserving pending and denied operations",()=>{
 const rows=effectRows([event(1,"effect_invoked"),event(2,"effect_completed",{cached:true,result_hash:"result"}),event(3,"effect_invoked",{occurrence:2}),event(4,"effect_denied",{occurrence:3})]);
 expect(rows.map(row=>row.status)).toEqual(["Cached","Invoked","Denied"]);
 expect(rows[0]?.data.result_hash).toBe("result");
});
test("completion remains visible when invocation is outside loaded window",()=>{
 expect(effectRows([event(4,"effect_completed",{error:"denied"})])[0]?.status).toBe("Failed");
});

test("page boundary keeps completion without duplicate cache record",()=>{
 const rows=effectRows([event(3,"effect_recorded",{result_hash:"result"}),event(4,"effect_completed",{cached:false,result_hash:"result"})]);
 expect(rows).toHaveLength(1);
 expect(rows[0]?.status).toBe("Completed");
 expect(rows[0]?.event.seq).toBe(4);
});

test("same-key retries retain each attempt's error or cached success",()=>{
 const rows=effectRows([event(1,"effect_invoked"),event(2,"effect_completed",{error:"first failure"}),event(3,"effect_invoked"),event(4,"effect_completed",{cached:true,result_hash:"retry"})]);
 expect(rows.map(row=>row.status)).toEqual(["Failed","Cached"]);
 expect(rows[0]?.data.error).toBe("first failure");
 expect(rows[0]?.data.result_hash).toBeUndefined();
 expect(rows[1]?.data.result_hash).toBe("retry");
});
test("pending retry never steals an earlier completion",()=>{
 const rows=effectRows([event(3,"effect_invoked"),event(2,"effect_completed",{error:"earlier"}),event(1,"effect_invoked")]);
 expect(rows.map(row=>row.status)).toEqual(["Failed","Invoked"]);
 expect(rows[1]?.data.error).toBeUndefined();
});
test("page-cut completion remains separate from subsequent same-key retry",()=>{
 const rows=effectRows([event(2,"effect_completed",{cached:true}),event(3,"effect_invoked")]);
 expect(rows.map(row=>row.status)).toEqual(["Cached","Invoked"]);
});
