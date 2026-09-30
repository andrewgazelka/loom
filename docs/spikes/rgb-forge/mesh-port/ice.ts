const call = async (args: any) => (await (await fetch("http://127.0.0.1:8832/v1/command", { method: "POST", headers: { Authorization: "Bearer replbench", "Content-Type": "application/json" }, body: JSON.stringify({ command: "eval", args }) })).json()) as any;
const cases: Record<string, string> = {
  derive_default: `#[derive(Clone, Copy, Default)]\nenum K { A, #[default] B }\npub fn f() -> u32 { K::default() as u32 }`,
  self_unit: `#[derive(Clone, Copy)]\nenum K { A, B }\nimpl K { fn pick(x: bool) -> Self { if x { Self::A } else { Self::B } } }\npub fn f() -> u32 { K::pick(true) as u32 }`,
  manual_default: `#[derive(Clone, Copy)]\nenum K { A, B }\nimpl Default for K { fn default() -> Self { K::B } }\npub fn f() -> u32 { K::default() as u32 }`,
  dyn_fn_param: `pub fn f() -> u32 { let g = |h: &dyn Fn(u32) -> u32| h(3); g(&|x| x + 1) }`,
  impl_fn_fn: `fn g(h: impl Fn(u32) -> u32) -> u32 { h(3) }\npub fn f() -> u32 { g(|x| x + 1) }`,
};
for (const [k, source] of Object.entries(cases)) {
  const j = await call({ source, entry: "f", args: [] });
  const err: string = j.result?.error ?? "";
  const msg = j.ok ? "OK " + JSON.stringify(j.result.output) : (err.match(/"message":"(effect analysis[^"]*|[^"]*try_resolve[^"]*)/)?.[1] ?? (err.includes("Instance::try_resolve") ? "ICE Instance::try_resolve Ctor(Variant, Const)" : err.slice(0, 200)));
  console.log(k.padEnd(16), msg);
}
