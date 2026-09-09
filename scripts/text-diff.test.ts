import {expect,test} from "bun:test";
import {diffLines,decodeText} from "../loom-ui/src/lib/text-diff";
test("insertions and removals preserve unchanged context",()=>{
 expect(diffLines("a\nb\nc","a\nx\nc")).toEqual([{kind:"same",text:"a"},{kind:"removed",text:"b"},{kind:"added",text:"x"},{kind:"same",text:"c"}]);
});
test("created deleted and empty content",()=>{
 expect(diffLines("","new")).toEqual([{kind:"added",text:"new"}]);
 expect(diffLines("old","")).toEqual([{kind:"removed",text:"old"}]);
 expect(diffLines("","")).toEqual([]);
});
test("binary and invalid UTF8 are not presented as text",()=>{
 expect(decodeText(new Uint8Array([0,1]))).toBeNull();expect(decodeText(new Uint8Array([255]))).toBeNull();expect(decodeText(new TextEncoder().encode("hello"))).toBe("hello");
});
