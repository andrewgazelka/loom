import { describe, expect, test } from "bun:test";
import { parseStages, sortedStages } from "../../src/lib/board/stages";

describe("build stage parsing", () => {
  test("merges every build_stages object by summing stage names and ignores other lines", () => {
    const log = [
      "   Compiling guest v0.1.0",
      '{"level":"info","message":"cargo metadata"}',
      '{"build_stages":{"preflight":12,"compile":4000,"unattributed_ms":3},"build_stages_total_ms":4015}',
      "not json at all {",
      '{"build_stages_warning":"3 ms of 4015 ms belong to no stage (limit 5 percent)"}',
      '{"build_stages":{"compile":500,"link":80,"unattributed_ms":1}}',
      '{"build_stages":"a string is not a stage map"}',
      '{"build_stages":{"compile":"fast"}}',
      "",
    ].join("\n");
    expect(parseStages(log)).toEqual({
      preflight: 12,
      compile: 4500,
      unattributed_ms: 4,
      link: 80,
    });
  });

  test("a log without stage lines yields an empty map", () => {
    expect(parseStages("warning: unused import\n{\"other\":1}\n")).toEqual({});
    expect(parseStages("")).toEqual({});
  });

  test("stages sort by descending milliseconds, then by name", () => {
    expect(sortedStages({ link: 80, compile: 4500, preflight: 80, unattributed_ms: 4 })).toEqual([
      ["compile", 4500],
      ["link", 80],
      ["preflight", 80],
      ["unattributed_ms", 4],
    ]);
  });
});
